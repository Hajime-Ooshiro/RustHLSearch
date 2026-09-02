# RustHLSearch

HLSearch（素数シフト探索）の Rust 実装です。指定深さまでのシフト列を DFS し、葉のビット数が `limit` に一致する最初のパスを記録して探索を終了します。葉で観測した popcount の最大値は `max_count` として別途保持します。

## ビルド

```bash
cargo build --release
```

バイナリは `target/release/hlsearch`（Windows では `hlsearch.exe`）です。

## 実行

```bash
cargo run --release -- --help
```

デフォルトは並列探索です。

```bash
cargo run --release
cargo run --release -- --mode sequential --depth 8 --limit 447
```

スレッド数は Rayon の既定（環境変数 `RAYON_NUM_THREADS`）に従います。

## 探索の動き

1. 1579 以下の素数を生成し、先頭から `depth` 個を階層に使う（`--primes-count` で個数を制限可能）。
2. 各素数 `p` とシフト `k = 0..p` について、列長 `cols` の補集合ビットマスクを作る。
3. マスクを AND しながら非再帰 DFS する。popcount が `limit` 未満のノードは枝刈りする。
4. 深さ `depth` の葉で:
   - popcount がこれまでの最大なら `max_count` を更新する（パスは保存しない）
   - popcount が `limit` と一致したらそのシフト列を 1 件記録し、**探索全体を打ち切る**

`--mode sequential` は単一スレッド DFS の出現順で最初のヒットです。`--mode parallel` は第 1 素数のシフトで分岐し、先にヒットしたスレッドが記録します（出現順は逐次と一致しません）。

`--max-depth` と `--target` は CLI と出力ファイルの config に残していますが、現在の探索ロジックでは未使用です。

## オプション

| フラグ | 既定 | 意味 |
| --- | --- | --- |
| `-d`, `--depth` | `8` | 探索する階層数 |
| `-m`, `--mode` | `parallel` | `sequential` または `parallel` |
| `-l`, `--limit` | `447` | 枝刈り下限、かつ記録する葉の popcount |
| `--max-depth` | `249` | 未使用（config に記録） |
| `-t`, `--target` | `447` | 未使用（config に記録） |
| `--cols` | `3159` | ビット列の長さ |
| `--primes-count` | 全素数 | 使用する素数の個数 |
| `-o`, `--output` | `shift_path.txt` | 出力パス（タイムスタンプを挿入） |

`depth` が使用可能な素数の個数を超えるとエラー終了します。

## 出力

ファイル名は `shift_path.txt` なら `shift_path_YYYYMMDD_HHMMSS.txt` のようになります。先頭に実行時設定、続けて結果を書きます。

```
# ---- config ----
mode:Parallel
depth:8
...
# ---- result ----
max_count:<葉で見た最大 popcount>
results:<limit ヒット件数（0 または 1）>
[0, 1, 2, ...]
```

パス行はヒットしたときだけ付きます。

## ライセンス

MIT License
