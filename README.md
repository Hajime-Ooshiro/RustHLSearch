# RustHLSearch

HLSearch（素数シフト探索）の Rust 実装です。指定した深さまでの素数シフト列を深さ優先探索（DFS）し、葉のビット数（popcount）が `limit` に一致する最初のパスを記録して探索を即座に終了します。葉で観測した popcount の最大値は `max_count` として記録されます。

## 機能・特徴

- **高速なビット並列処理**: 64-bit 単位の独自 `BitMask` 構造体による高速 bitwise AND および popcount。
- **並列 DFS（Rayon）**: 第1素数のシフトを並列分散し、マルチコア CPU をフル活用。
- **早期打ち切り**: いずれかのスレッドで `limit` 一致解が検出された瞬間、アトミックフラグにより全スレッドの探索を停止。
- **降順探索**: 各素数のシフト候補を降順（$p-1 \dots 0$）に探索。
- **リアルタイム進捗表示**: `indicatif` による探索ノード数・処理速度・最良 popcount のライブ表示。
- **チェックポイント再開**: 逐次モードでは 10,000 ノードごとに進捗をログ出力し、指定したチェックポイントから探索を再開可能。

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
cargo test parallel_search_records_a_valid_matching_leaf

# フォーマット確認
cargo fmt -- --check

# Clippy
cargo clippy --all-targets --all-features -- -D warnings
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

デフォルト（並列モード、深さ 8、limit 447、cols 3159）:

```bash
cargo run --release
```

逐次モード（単一スレッド）で実行:

```bash
cargo run --release -- --mode sequential --depth 8 --limit 447
```

逐次探索では `checkpoint.json` を自動保存・再開します:

```bash
cargo run --release -- --mode sequential
```

逐次モードでは 10,000 ノードごとに `探索経過` をログへ出力し、DFS のスタックと集計値を `checkpoint.json` に自動保存します。起動時に `checkpoint.json` が存在すれば自動的に読み込んで続行し、探索が正常終了すると `searched.json` に変更します。チェックポイント機能は探索順序を保てる逐次モード専用です。

素数の個数や出力先を指定して実行:

```bash
cargo run --release -- --depth 10 --limit 400 --primes-count 100 -o result.json
```

> **Note**: 並列モード時のスレッド数は Rayon の既定値（論理コア数）となります。環境変数 `RAYON_NUM_THREADS` でスレッド数を指定可能です。

### 入力値の検証

実行開始前に次の条件を検証します。条件に違反した場合はエラーを表示して終了します。

- `depth` は 1 以上
- `cols` は 1 以上
- `limit` は `cols` 以下
- `primes-count` は 1 以上かつ利用可能な素数数以下
- `depth` は使用する素数数以下

## 探索アルゴリズムの概要

1. **素数生成**:
   - 1579 以下の素数（最大 249 個）をエラトステネスの篩で生成し、先頭から `depth` 個を探索階層に使用します（`--primes-count` で上限指定可能）。
2. **補集合シフトテーブル作成**:
   - 各素数 $p$ とシフト $k \in [0, p)$ について、長さ `cols` の補集合ビットマスクを事前構築します。
3. **深さ優先探索 (DFS)**:
   - マスクを AND 演算しながら非再帰（スタック）DFS を行います。
   - 途中の累積 popcount が `limit` 未満になった枝は即座に枝刈り（pruning）します。
   - 各素数のシフト探索は降順（$p-1 \dots 0$）に進めます。
4. **葉ノード（深さ `depth`）の判定**:
   - popcount がこれまでの最大値を超えた場合、`max_count` を更新します。
   - popcount が `limit` と一致した場合、そのシフトパスを記録して**探索全体を打ち切り終了**します。

### 探索モード

- `--mode parallel`（デフォルト）: 第1素数のシフト候補を逆順（降順）で Rayon の並列イテレータに分配し、複数スレッドで並列 DFS します。いずれかのスレッドが解を見つけた時点で全スレッドを停止します。
- `--mode sequential`: 単一スレッドで決定論的に非再帰 DFS を実行します。

並列モードではスレッドの実行順序により、記録される解のシフト列が逐次モードと異なる場合があります。どちらのモードも、最初に見つかった `limit` 一致の解を1件記録して探索を終了します。

## コマンドラインオプション

| フラグ | 短縮 | 既定値 | 説明 |
| --- | --- | --- | --- |
| `--depth` | `-d` | `8` | 探索する階層数（使用する素数の個数） |
| `--mode` | `-m` | `parallel` | 探索モード（`parallel` または `sequential`） |
| `--limit` | `-l` | `447` | 枝刈り下限値、かつ探索完了・記録対象とする葉の popcount |
| `--cols` | | `3159` | ビット列の長さ |
| `--primes-count` | | 全素数 (249) | 使用する素数の最大個数制限 |
| `--output` | `-o` | `shift_path.json` | JSON出力ファイルパス（実行時にタイムスタンプが挿入されます） |
| `--max-depth` | | `249` | 出力設定に記録される予約パラメータ |
| `--target` | `-t` | `447` | 出力設定に記録される予約パラメータ |

> **Note**: `--max-depth` と `--target` は現在の探索条件には影響せず、実行設定として出力ファイルに記録されます。

## 出力ファイル形式

出力ファイル名には実行時のタイムスタンプが付与されます（例: `shift_path.json` の場合 `shift_path_YYYYMMDD_HHMMSS.json`）。

ファイルには実行時設定（`config`）と探索結果（`result`）を含むJSONオブジェクトが出力されます。

```json
{
  "config": {
    "mode": "parallel",
    "depth": 8,
    "limit": 447,
    "max_depth": 249,
    "target": 447,
    "cols": 3159,
    "primes_count": "all",
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
- `result.max_count`: 早期終了までに到達した葉ノードの最大 popcount
- `result.results`: 最初に `limit` にヒットした解の個数（見つかった場合は 1、見つからなかった場合は 0）
- `result.shifts`: 見つかったシフト列の配列

## ライセンス

[MIT License](LICENSE)
