# Copilot instructions for RustHLSearch

## Project overview

RustHLSearch is a Rust 2021 command-line program for searching prime-shift
sequences. The implementation is split across `src/main.rs`, `src/search.rs`,
`src/bitmask.rs`, `src/primes.rs`, `src/output.rs`, and
`src/rocksdb_output.rs`. It uses SQLite for checkpoint persistence and RocksDB
for shift-path persistence.

## Build, test, and lint

Run commands from the repository root:

```bash
cargo build
cargo build --release
cargo run --release -- --help
cargo test
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

`cargo test` runs the unit tests in the Rust source files. Use `cargo fmt`
after Rust changes and run the narrowest relevant tests before broader checks.

## Architecture and data flow

- `main.rs` parses and validates the CLI, builds the prime list and shift
  table, starts the selected search, and writes timestamped JSON results.
- `primes.rs` generates primes up to `MAX_PRIME`.
- `bitmask.rs` implements logical-size-aware bit masks and in-place AND plus
  popcount.
- `search.rs` contains shift-table construction, iterative sequential and
  Rayon-parallel DFS, result aggregation, and SQLite-backed checkpoint/resume
  handling.
- `rocksdb_output.rs` stores reusable shift-path prefixes and target paths in
  `HLSearch.rocksdb`.
- `output.rs` builds timestamped result paths.

The CLI accepts `--cols` from 1 through 3159. The current search target is
stored in `State` and defaults to 447. Keep sequential and parallel search
behavior aligned, especially pruning, maximum-result aggregation, checkpoint
recovery, and early interruption.

## Repository conventions

- Preserve the distinction between `BitMask::size` and allocated 64-bit words;
  operations must not expose bits beyond the logical size.
- Prime and shift indices are zero-based. Search visits candidate shifts in
  descending order.
- Keep user-facing CLI and log text in the existing Japanese style.
- New CLI options must be reflected in `README.md` and in output configuration
  when they affect a run.
- Tests that create RocksDB databases or checkpoint files must use unique
  temporary paths and clean them up.
- Do not treat generated database contents or `target/` artifacts as source.
- Rayon uses logical CPU count by default; set `RAYON_NUM_THREADS` when
  reproducing parallel runs.
