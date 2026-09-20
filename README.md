# RustHLSearch

HLSearch（素数シフト探索）の Rust 実装です。指定した深さまでの素数シフト列を深さ優先探索（DFS）し、葉のビット数（popcount）の最大値と、その最大値を持つシフトパスを記録します。

## 機能・特徴

- **高速なビット並列処理**: 64-bit 単位の独自 `BitMask` 構造体による bitwise AND および popcount。
- **並列 DFS（Rayon）**: 利用可能な CPU 数に応じて先頭の複数階層を自動分割し、マルチコア CPU を活用。
- **割り当てを抑えたビット演算**: 深さごとの作業バッファを再利用し、AND 演算と popcount を1回の走査で実行。
- **popcount 上界による枝刈り**: 部分木の popcount は現在値を超えられないため、現在の最大値未満の枝を打ち切る。
- **降順探索**: 各素数のシフト候補を降順（$p-1 \dots 0$）に探索。
- **リアルタイム進捗表示**: ワーカごとのローカル計数を定期集約し、`indicatif` で探索ノード数・処理速度・最良 popcount を表示。
- **チェックポイント再開**: 逐次モードでは 100,000 ノードごとに進捗を更新し、`checkpoint.json` へ保存・再開可能。

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
  output.rs    # タイムスタンプ付き出力パス生成
```

## 実行

ヘルプの表示:

```bash
cargo run --release -- --help
```

### 実行例

デフォルト（並列モード、深さ 8、cols 3159）:

```bash
cargo run --release
```

逐次モード（単一スレッド）で実行:

```bash
cargo run --release -- --mode sequential --depth 8
```

逐次探索では `checkpoint.json` を自動保存・再開します:

```bash
cargo run --release -- --mode sequential
```

逐次モードでは 100,000 ノードごとに進捗を更新し、DFS のスタックと集計値を `checkpoint.json` に自動保存します。起動時に `checkpoint.json` が存在すれば自動的に読み込んで続行し、探索が正常終了すると `searched_YYYYMMDD_HHMMSS.json` に改名します。チェックポイント機能は探索順序を保てる逐次モード専用です。並列モード起動時に `checkpoint.json` があるとエラーになります。

チェックポイント保存周期を変更する場合:

```bash
cargo run --release -- --mode sequential --checkpoint-interval 50000
```

`--checkpoint-interval` でノード数を指定できます（デフォルト: 100,000）。

深さ、列数、出力先を指定して実行:

```bash
cargo run --release -- --depth 10 --cols 4000 -o result.json
```

> **Note**: 並列モード時のスレッド数は Rayon の既定値（論理コア数）となります。環境変数 `RAYON_NUM_THREADS` でスレッド数を指定可能です。
> 並列モードではスレッドごとに探索結果を集約します。記録されるパスの順序は実行ごとに異なる場合があります。

### 入力値の検証

実行開始前に次の条件を検証します。条件に違反した場合はエラーを表示して終了します。

- `depth` は 1 以上
- `cols` は 1 以上
- `checkpoint-interval` は 1 以上
- `depth` は使用する素数数以下


## 探索アルゴリズムの概要

1. **素数生成**:
   - 1579 以下の素数（最大 249 個）をエラトステネスの篩で生成し、先頭から `depth` 個を探索階層に使用します。
2. **補集合シフトテーブル作成**:
   - 各素数 $p$ とシフト $k \in [0, p)$ について、長さ `cols` の補集合ビットマスクを事前構築します。
3. **深さ優先探索 (DFS)**:
   - マスクを AND 演算しながら非再帰（スタック）DFS を行います。
   - 途中の累積 popcount が現在の `max_count` 未満になった枝は即座に枝刈りします。
   - 各素数のシフト探索は降順（$p-1 \dots 0$）に進めます。
4. **葉ノード（深さ `depth`）の判定**:
   - popcount がこれまでの最大値を超えた場合、`max_count` と記録済みパスを更新します。

### 探索モード

- `--mode parallel`（デフォルト）: Rayon のワーカ数に応じて先頭の複数階層を分割し、複数スレッドで並列 DFS します。各階層のシフト候補は降順で処理されます。
- `--mode sequential`: 単一スレッドで決定論的に非再帰 DFS を実行します。

並列モードではスレッドの実行順序により、記録されるシフトパスの順序が逐次モードと異なる場合があります。

## コマンドラインオプション

| フラグ | 短縮 | 既定値 | 説明 |
| --- | --- | --- | --- |
| `--depth` | `-d` | `8` | 探索する階層数（使用する素数の個数） |
| `--mode` | `-m` | `parallel` | 探索モード（`parallel` または `sequential`） |
| `--cols` | | `3159` | ビット列の長さ |
| `--output` | `-o` | `shift_path.json` | JSON出力ファイルパス（実行時にタイムスタンプが挿入されます） |
| `--checkpoint-interval` | | `100000` | チェックポイント保存周期 (ノード数) |

## 出力ファイル形式

出力ファイル名には実行時のタイムスタンプが付与されます（例: `shift_path.json` の場合 `shift_path_YYYYMMDD_HHMMSS.json`）。

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
    "max_count": 447,
    "results": 1,
   "shifts": [[1, 1, 4, 3, 5, 10, 1, 9]]
  }
}
```

- `config`: 実行時設定と経過時間
- `result.max_count`: 全探索で到達した葉ノードの最大 popcount
- `result.results`: `max_count` に一致するパスの個数
- `result.shifts`: `max_count` に一致するシフト列の配列

## ライセンス

[MIT License](LICENSE)
