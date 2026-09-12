mod bitmask;
mod output;
mod primes;
mod search;

use clap::Parser;
use log::{info, LevelFilter};
use output::with_timestamp;
use primes::generate_primes;
use search::{build_shift_table, SearchMode, State};
use serde::Serialize;
use simple_logger::SimpleLogger;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

const CHECKPOINT_PATH: &str = "checkpoint.json";

#[derive(Serialize)]
struct OutputFile<'a> {
    config: OutputConfig<'a>,
    result: OutputResult<'a>,
}

#[derive(Serialize)]
struct OutputConfig<'a> {
    mode: &'a str,
    depth: usize,
    max_depth: usize,
    target: usize,
    cols: usize,
    parallel_tasks_per_thread: usize,
    elapsed: String,
}

#[derive(Serialize)]
struct OutputResult<'a> {
    max_count: usize,
    results: usize,
    shifts: &'a [Vec<usize>],
    target_results: usize,
    target_shifts: &'a [Vec<usize>],
}

#[derive(Parser, Debug)]
#[command(author, version, about = "HLSearch: 素数シフト探索プログラム (Rust版)", long_about = None)]
pub struct Cli {
    #[arg(short, long, default_value_t = 8, help = "探索する階層数")]
    pub depth: usize,

    #[arg(
        short,
        long,
        value_enum,
        default_value_t = SearchMode::Parallel,
        help = "探索モード (sequential | parallel)"
    )]
    pub mode: SearchMode,

    #[arg(long, default_value_t = 249, help = "target 判定を行う探索深さ")]
    pub max_depth: usize,

    #[arg(short, long, default_value_t = 447, help = "記録対象の popcount")]
    pub target: usize,

    #[arg(long, default_value_t = 3159, help = "列数 (長さ)")]
    pub cols: usize,

    #[arg(
        short,
        long,
        default_value = "shift_path.json",
        help = "出力ファイルパス"
    )]
    pub output: PathBuf,

    #[arg(
        long,
        default_value_t = 100_000,
        help = "チェックポイント保存周期 (ノード数)"
    )]
    pub checkpoint_interval: u64,

    #[arg(long, default_value_t = 4, help = "並列時のスレッド当たりタスク数")]
    pub parallel_tasks_per_thread: usize,
}

impl Cli {
    fn validate(&self, available_primes: usize) -> Result<(), String> {
        if self.depth == 0 {
            return Err("depth must be at least 1".to_string());
        }
        if self.cols == 0 {
            return Err("cols must be at least 1".to_string());
        }
        if self.target > self.cols {
            return Err(format!(
                "target ({}) cannot exceed cols ({})",
                self.target, self.cols
            ));
        }
        if self.checkpoint_interval == 0 {
            return Err("checkpoint-interval must be at least 1".to_string());
        }
        if self.parallel_tasks_per_thread == 0 {
            return Err("parallel-tasks-per-thread must be at least 1".to_string());
        }
        if self.depth > available_primes {
            return Err(format!(
                "depth ({}) cannot exceed available primes ({})",
                self.depth, available_primes
            ));
        }
        Ok(())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new().with_level(LevelFilter::Info).init()?;
    let cli = Cli::parse();
    let all_primes = generate_primes(1579);

    if let Err(message) = cli.validate(all_primes.len()) {
        eprintln!("エラー: {}", message);
        std::process::exit(1);
    }

    info!("HLSearch (Rust) 開始");
    info!(
        "設定: mode={:?} depth={} max_depth={} target={}",
        cli.mode, cli.depth, cli.max_depth, cli.target
    );

    let start_time = Instant::now();
    let search_primes = all_primes[..cli.depth].to_vec();
    let shift_table = build_shift_table(&search_primes, cli.cols);
    let mut state =
        State::new(search_primes, cli.cols, shift_table).map_err(std::io::Error::other)?;
    state.max_depth = cli.max_depth;
    state.target = cli.target;
    state.checkpoint_interval = cli.checkpoint_interval;
    state.parallel_tasks_per_thread = cli.parallel_tasks_per_thread;

    match cli.mode {
        SearchMode::Sequential => {
            let checkpoint_path = Path::new(CHECKPOINT_PATH);
            let resume_path = checkpoint_path.exists().then_some(checkpoint_path);
            state.search_with_checkpoint(cli.depth, Some(checkpoint_path), resume_path)?;
            let searched_path = with_timestamp(Path::new("searched.json"));
            std::fs::rename(checkpoint_path, &searched_path)?;
            info!(
                "チェックポイントを探索済みファイルへ変更: {}",
                searched_path.display()
            );
        }
        SearchMode::Parallel => {
            if Path::new(CHECKPOINT_PATH).exists() {
                return Err(
                    "checkpoint.json は sequential モードでのみ再開できます。--mode sequential を指定してください"
                        .into(),
                );
            }
            let result = state.search_parallel(cli.depth);
            state.max_count = result.max_count;
            state.results = result.results;
            state.shifts = result.shifts;
            state.target_results = result.target_results;
            state.target_shifts = result.target_shifts;
        }
    }

    let elapsed = start_time.elapsed();
    info!("探索時間: {:?}", elapsed);
    info!("最大値: {}", state.max_count);
    info!("該当件数: {}", state.results);

    let output_path = with_timestamp(&cli.output);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(&output_path)?;
    let mut writer = BufWriter::new(file);
    info!("出力ファイル: {}", output_path.display());

    let output = OutputFile {
        config: OutputConfig {
            mode: match cli.mode {
                SearchMode::Sequential => "sequential",
                SearchMode::Parallel => "parallel",
            },
            depth: cli.depth,
            max_depth: cli.max_depth,
            target: cli.target,
            cols: cli.cols,
            parallel_tasks_per_thread: cli.parallel_tasks_per_thread,
            elapsed: format!("{elapsed:?}"),
        },
        result: OutputResult {
            max_count: state.max_count,
            results: state.results,
            shifts: &state.shifts,
            target_results: state.target_results,
            target_shifts: &state.target_shifts,
        },
    };
    serde_json::to_writer_pretty(&mut writer, &output)?;
    writeln!(writer)?;

    info!("HLSearch 終了");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cli() -> Cli {
        Cli {
            depth: 1,
            mode: SearchMode::Sequential,
            max_depth: 249,
            target: 1,
            cols: 4,
            output: PathBuf::from("shift_path.txt"),
            checkpoint_interval: 100_000,
            parallel_tasks_per_thread: 4,
        }
    }

    #[test]
    fn cli_validation_accepts_valid_configuration() {
        assert!(test_cli().validate(3).is_ok());
    }

    #[test]
    fn cli_validation_rejects_invalid_configuration() {
        let mut cli = test_cli();
        cli.depth = 0;
        assert!(cli.validate(3).is_err());
        cli = test_cli();
        cli.cols = 0;
        assert!(cli.validate(3).is_err());
        cli = test_cli();
        cli.target = 5;
        assert_eq!(
            cli.validate(3).unwrap_err(),
            "target (5) cannot exceed cols (4)"
        );
        cli = test_cli();
        cli.checkpoint_interval = 0;
        assert_eq!(
            cli.validate(3).unwrap_err(),
            "checkpoint-interval must be at least 1"
        );
        cli = test_cli();
        cli.parallel_tasks_per_thread = 0;
        assert_eq!(
            cli.validate(3).unwrap_err(),
            "parallel-tasks-per-thread must be at least 1"
        );
    }

    #[test]
    fn cli_validation_rejects_depth_above_available_primes() {
        let mut cli = test_cli();
        cli.depth = 4;
        assert_eq!(
            cli.validate(3).unwrap_err(),
            "depth (4) cannot exceed available primes (3)"
        );
    }
}
