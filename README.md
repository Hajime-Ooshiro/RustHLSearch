# RustHLSearch

HLSearch（素数シフト探索）の Rust 実装です。指定した深さまでの素数シフト列を深さ優先探索（DFS）し、葉のビット数（popcount）の最大値と、その最大値を持つシフトパスを記録します。

## 機能・特徴

- **高速なビット並列処理**: 64-bit 単位の独自 `BitMask` 構造体による bitwise AND および popcount。
- **並列 DFS（Rayon）**: 利用可能な CPU 数に応じて先頭の複数階層を自動分割し、マルチコア CPU を活用。
- **割り当てを抑えたビット演算**: 深さごとの作業バッファを再利用し、AND 演算と popcount を1回の走査で実行。
- **固定ターゲットによる枝刈り**: `State` の固定ターゲット値 `447` 未満となった枝を打ち切る。
- **降順探索**: 各素数のシフト候補を降順（$p-1 \dots 0$）に探索。
- **進捗表示**: スピナーに探索ノード数、最良値、ヒット数、現在深さを表示し、チェックポイント保存周期ごとに更新する。
- **チェックポイント再開**: 逐次・並列ともに 100,000 ノードごとに進捗を更新し、`--output-dir` 配下の `checkpoint.sqlite` へ保存・再開可能。SQLite のトランザクションでチェックポイントを更新します。並列モードでは完了済み分割タスクと実行中タスクの探索スタックを保存して再開します。

## ビルド

### Cargo によるビルド

```bash
cargo build --release
```

バイナリは `target/release/hlsearch`（Windows では `target/release/hlsearch.exe`）に出力されます。

### Windows 用バッチファイル

Windows 環境向けに `build.bat` も用意されています（デバッグビルドおよびリリースビルドを順に実行）。

```cmd
build.bat
```

## テスト・静的解析

```bash
# 単体テストをすべて実行
cargo test

# テスト名を指定して実行
cargo test sequential_and_parallel_search_find_maximum_results

# フォーマット確認
cargo fmt -- --check

# Clippy
cargo clippy --all-targets -- -D warnings
```

## ソース構成

```text
src/
  main.rs      # CLI解析、入力検証、探索実行、結果出力
  bitmask.rs   # BitMaskによる64-bit単位のビット演算
  primes.rs    # エラトステネスの篩による素数生成
  search.rs    # シフトテーブル生成と逐次・並列DFS
  rocksdb_output.rs # RocksDB の親ノード形式シフトパス永続化
  output.rs    # タイムスタンプ付き出力パス生成
```

## 実行

ヘルプの表示:

```bash
cargo run --release -- --help
```

### 実行例

デフォルト（並列モード、深さ 8、列数 3159）:

```bash
cargo run --release
```

逐次モード（単一スレッド）で実行:

```bash
cargo run --release -- --mode sequential --depth 8
```

逐次探索では `--output-dir`（既定はカレントディレクトリ）配下に `checkpoint.sqlite` を自動保存・再開します:

```bash
cargo run --release -- --mode sequential
```

並列探索も同じファイルへチェックポイントを保存し、同じコマンドで再開できます:

```bash
cargo run --release -- --mode parallel
```

探索中は 100,000 ノードごとに進捗を更新し、DFS の状態と集計値を `--output-dir`（既定は `.`）配下の `checkpoint.sqlite` に自動保存します。チェックポイントは SQLite のトランザクションで更新され、保存直前に RocksDB の保留中のシフトパスも flush されます。実行中に `Ctrl+C` を1回押すと、安全な探索地点で同じチェックポイントを保存して終了します。次回は同じコマンドを実行すると中断地点から再開できます。並列探索では、完了済みワーク項目を飛ばし、保存済みスタックの続きから実行します。シフトパスとターゲット一致パスは `HLSearch.rocksdb` に保存され、再開時にそこから復元します。既存の `checkpoint.json` または `checkpoint.bak` があり SQLite 形式のチェックポイントがない場合は、起動時に SQLite へ移行します。探索が正常終了すると `checkpoint.sqlite` は削除されます。チェックポイントと RocksDB に保存された結果数が一致しない場合は、保存データを破棄して探索を最初からやり直します。逐次チェックポイントは `--mode sequential` でのみ、並列チェックポイントは `--mode parallel` でのみ再開できます。

探索開始時、同じ `HLSearch.rocksdb` に前深さの保存済みパスがあれば、そのパスを開始パスとして再利用します。前深さのパスが存在しない場合は、通常どおり先頭レベルから探索します。

探索中はスピナーにノード数、最良値、ヒット数、現在深さを表示します。総探索ノード数は探索中に確定しないため、完了率や残り時間は表示しません。表示内容はチェックポイント保存周期ごとに更新され、完了または中断時に最終集計へ切り替わります。ログは `SimpleLogger` により既定で `INFO` レベル以上が出力されます。主要イベントは `INFO`、詳細な探索経過は `DEBUG` として実装されていますが、既定のログレベルでは `DEBUG` ログは表示されません。

チェックポイント保存周期を変更する場合:

```bash
cargo run --release -- --checkpoint-interval 50000
```

`--checkpoint-interval` でノード数を指定できます（デフォルト: 100,000）。逐次・並列のどちらでも有効です。

深さと出力先を指定して実行:

```bash
cargo run --release -- --depth 10 --output-dir results
```

列数は 3159 に固定されています。

> **Note**: 並列モード時のスレッド数は Rayon の既定値（論理コア数）となります。環境変数 `RAYON_NUM_THREADS` でスレッド数を指定可能です。
> 並列モードではスレッドごとに探索結果を集約します。記録されるパスの順序は実行ごとに異なる場合があります。

### 入力値の検証

実行開始前に次の条件を検証します。条件に違反した場合はエラーを表示して終了します。

- `depth` は 1 以上
- `checkpoint-interval` は 1 以上
- `depth` は使用する素数数以下

## 探索アルゴリズムの概要

1. **素数生成**:
   - 1579 以下の素数（最大 249 個）をエラトステネスの篩で生成し、先頭から `depth` 個を探索階層に使用します。
2. **補集合シフトテーブル作成**:
   - 各素数 $p$ とシフト $k \in [0, p)$ について、長さ 3159 の補集合ビットマスクを事前構築します。
3. **深さ優先探索 (DFS)**:
   - マスクを AND 演算しながら非再帰（スタック）DFS を行います。
   - 途中の累積 popcount が固定ターゲット値 `447` 未満になった枝は即座に枝刈りします。
   - 各素数のシフト探索は降順（$p-1 \dots 0$）に進めます。
4. **葉ノード（深さ `depth`）の判定**:
   - popcount がこれまでの最大値を超えた場合、`max_score` と記録済みパスを更新します。

### 探索モード

- `--mode parallel`（デフォルト）: Rayon のワーカ数に応じて先頭の複数階層を分割し、複数スレッドで並列 DFS します。各階層のシフト候補は降順で処理されます。
- `--mode sequential`: 単一スレッドで決定論的に非再帰 DFS を実行します。探索中の結果集計とビット列更新は別関数に分離して扱います。

並列モードではスレッドの実行順序により、記録されるシフトパスの順序が逐次モードと異なる場合があります。

## コマンドラインオプション

| フラグ | 短縮 | 既定値 | 説明 |
| --- | --- | --- | --- |
| `--depth` | `-d` | `8` | 探索する階層数（使用する素数の個数） |
| `--mode` | `-m` | `parallel` | 探索モード（`parallel` または `sequential`） |
| `--output-dir` | `-o` | `.` | 出力ディレクトリ |
| `--checkpoint-interval` | | `100000` | チェックポイント保存周期 (ノード数) |

## 出力ファイル形式

出力ファイルは、シフトパスを `HLSearch.rocksdb`、探索結果を `result_depth8_YYYYMMDD_HHMMSS.json` の形式で出力します。ターゲット値による枝刈りを通過したシフトパスは、探索中に RocksDB の親ノード形式でバッファへ追加され、定期チェックポイントの直前にまとめてDBへ flush されます。深さ `d` の探索で得られたシフトパスは、長さ `k`（1 から `d`）の接頭辞ごとに `nodes` カラムファミリーへ保存されます。各ノードは `node_id`、`parent_depth`（親が属する深さ。深さ 1 は 0）、`parent_node_id`（親ノード。深さ 1 は NULL）、`shift`、そのパスの `score` を持ちます。同じ深さ内では接頭辞を複数パスで共有します。読み込み時には各行の `parent_node_id` をたどってシフト列を復元します。深さ 249 でターゲット値 447 に一致したパスは、発見ごとに `target_paths` カラムファミリーへ追加されます。

ファイルには実行時設定（`config`）と探索結果（`result`）を含むJSONオブジェクトが出力されます。

```json
{
  "config": {
    "mode": "parallel",
    "depth": 8,
    "cols": 3159,
    "elapsed": "1.234567s"
  },
  "result": {
    "max_score": 447,
    "results": 1,
    "shifts": [[1, 1, 4, 3, 5, 10, 1, 9]],
    "target_count": 0,
    "target_shifts": []
  }
}
```

- `config`: 実行時設定と経過時間
- `result.max_score`: 全探索で到達した葉ノードの最大 popcount
- `result.results`: `max_score` に一致するパスの個数
- `result.shifts`: `max_score` に一致するシフト列の配列
- `result.target_count`: 深さ 249 で popcount がターゲット値 447 に一致したパスの個数
- `result.target_shifts`: 上記のシフト列の配列

## ライセンス

[MIT License](LICENSE)
