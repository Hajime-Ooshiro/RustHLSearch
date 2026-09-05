# Copilot instructions for RustHLSearch

## Project overview

RustHLSearch is a Rust 2021 command-line program for searching prime-shift
sequences. The implementation is currently concentrated in `src/main.rs`.
`Cargo.toml` defines the `hlsearch` binary and its runtime dependencies; there
are no additional library crates, workspace members, CI workflows, or
repository-specific assistant instructions.

## Build, run, test, and lint

Run commands from the repository root:

```bash
# Debug build
cargo build

# Optimized release build
cargo build --release

# Windows: run both debug and release builds (the batch file pauses at the end)
build.bat

# Show CLI help
cargo run --release -- --help

# Run the default parallel search
cargo run --release

# Run deterministic single-threaded DFS
cargo run --release -- --mode sequential --depth 8 --limit 447
```

Cargo has no repository-defined test suite or lint configuration. Use the
standard commands when changing code:

```bash
# Run all tests (including tests in src/main.rs if added)
cargo test

# Run one named test
cargo test test_name

# Check formatting
cargo fmt -- --check

# Run Clippy's default checks
cargo clippy --all-targets --all-features -- -D warnings
```

The README is the source of truth for user-facing CLI defaults and examples.
Release builds use LTO, `opt-level = 3`, and one codegen unit, so performance
comparisons should use `cargo build --release`.

## Architecture and data flow

`main` parses the Clap-derived `Cli`, generates all primes up to 1579, selects
the requested number of primes, and builds a per-prime/per-shift lookup table
before starting the search. The table contains complement masks for each shift,
so the DFS only needs repeated `BitMask::bitand` operations.

The main layers are:

- `generate_primes`: Sieve of Eratosthenes; the resulting prime list supplies
  one search level per prime.
- `BitMask`: fixed-size `Vec<u64>` storage with bounded `set`, bitwise AND, and
  popcount operations. Its `size` is the logical column count; unused bits in
  the final word are cleared when masks are initialized.
- `build_shift_table`: eagerly constructs `shift_table[level][shift]`, where
  shifts are indexed from `0` through `p - 1`. Search code relies on this
  table being built for exactly the depth being searched.
- `State`: owns search configuration, the current key/mask, counters, result
  paths, and the precomputed table.
- `State::search`: non-recursive, single-threaded DFS using an explicit
  `Frame` stack and a progress spinner.
- `State::search_parallel`: Rayon parallelizes the first-prime choices; each
  worker performs its own stack-based DFS while sharing atomics for counts, a
  mutex-protected result vector, and an atomic stop flag.

Both search modes prune a branch when its accumulated popcount is below
`limit`. At a leaf, they update `max_count`; the first leaf whose count equals
`limit` records its shift path and stops the overall search. Sequential search
is deterministic in descending shift order. Parallel search can stop in a
different worker, so do not assume its result path or traversal order matches
sequential mode.

After searching, `main` writes configuration, elapsed time, summary counts, and
recorded paths to a timestamped output path. For example,
`shift_path.txt` becomes `shift_path_YYYYMMDD_HHMMSS.txt`; parent directories
are created if needed. `--max-depth` and `--target` are currently parsed and
written to output but do not affect the search algorithm.

## Repository-specific conventions

- Keep the executable-oriented implementation in `src/main.rs` unless the
  project is deliberately split into modules. Pure helpers and state methods
  are `pub` where useful for unit testing.
- Preserve the distinction between logical column count (`BitMask::size`) and
  the allocated 64-bit words. Operations must not reintroduce set bits beyond
  the requested column count.
- Treat prime and shift indices as zero-based in Rust collections. A search
  level uses the corresponding prime, and its candidate shifts are visited in
  descending order by decrementing `next_idx`.
- Keep the sequential and parallel implementations semantically aligned:
  both must apply the same complement masks, prune at the same threshold, update
  leaf maxima, record a matching path, and honor early termination.
- Use the existing `log`/`SimpleLogger` messages and `indicatif` spinner for
  runtime observability rather than introducing ad-hoc output. CLI and log
  text currently includes Japanese descriptions/messages; retain that style
  when editing adjacent user-facing text.
- New CLI options belong on the `Cli` Clap derive and should also be reflected
  in `README.md` and the generated output configuration if they affect a run.
- Do not treat generated `shift_path*.txt` files or `target/` artifacts as
  source inputs. The output filename is intentionally timestamped, so tests
  should use a temporary output path or test pure helpers instead of relying on
  a fixed generated filename.
- Rayon uses logical CPU count by default. Set `RAYON_NUM_THREADS` when
  reproducing or comparing parallel runs.
