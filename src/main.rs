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
const CHECKPOINT_BACKUP_PATH: &str = "checkpoint.bak";

#[derive(Serialize)]
struct OutputFile<'a> {
    config: OutputConfig<'a>,
    result: OutputResult,
}

#[derive(Serialize)]
struct OutputConfig<'a> {
    mode: &'a str,
    depth: usize,
    cols: usize,
    elapsed: String,
}

#[derive(Serialize)]
struct OutputResult {
    max_count: usize,
    results: usize,
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

    #[arg(long, default_value_t = 3159, help = "列数 (長さ)")]
    pub cols: usize,

    #[arg(short, long, default_value = "result.json", help = "出力ファイルパス")]
    pub output: PathBuf,

    #[arg(
        long,
        default_value_t = 100_000,
        help = "チェックポイント保存周期 (ノード数)"
    )]
    pub checkpoint_interval: u64,
}

impl Cli {
    fn validate(&self, available_primes: usize) -> Result<(), String> {
        if self.depth == 0 {
            return Err("depth must be at least 1".to_string());
        }
        if self.cols == 0 {
            return Err("cols must be at least 1".to_string());
        }
        if self.checkpoint_interval == 0 {
            return Err("checkpoint-interval must be at least 1".to_string());
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

    let primes = all_primes;

    info!("HLSearch (Rust) 開始");
    info!(
        "設定: mode={:?} depth={} primes={}",
        cli.mode,
        cli.depth,
        primes.len()
    );

    let start_time = Instant::now();
    let shift_table = build_shift_table(&primes[..cli.depth], cli.cols);
    let mut state = State::new(primes, cli.cols, shift_table);
    state.checkpoint_interval = cli.checkpoint_interval;

    match cli.mode {
        SearchMode::Sequential => {
            let checkpoint_path = Path::new(CHECKPOINT_PATH);
            let backup_path = Path::new(CHECKPOINT_BACKUP_PATH);
            let resume_path = if checkpoint_path.exists() {
                Some(checkpoint_path)
            } else {
                backup_path.exists().then_some(backup_path)
            };
            state.search_with_checkpoint(cli.depth, Some(checkpoint_path), resume_path)?;
            let searched_path = with_timestamp(Path::new("searched.json"), cli.depth);
            std::fs::rename(checkpoint_path, &searched_path)?;
            if backup_path.exists() {
                std::fs::remove_file(backup_path)?;
            }
            info!(
                "チェックポイントを探索済みファイルへ変更: {}",
                searched_path.display()
            );
        }
        SearchMode::Parallel => {
            if Path::new(CHECKPOINT_PATH).exists() || Path::new(CHECKPOINT_BACKUP_PATH).exists() {
                return Err(
                    "checkpoint.json または checkpoint.bak は sequential モードでのみ再開できます。--mode sequential を指定してください"
                        .into(),
                );
            }
            let result = state.search_parallel(cli.depth);
            state.max_count = result.max_count;
            state.results = result.results;
            state.shifts = result.shifts;
        }
    }

    let elapsed = start_time.elapsed();
    info!("探索時間: {:?}", elapsed);
    info!("最大値: {}", state.max_count);
    info!("該当件数: {}", state.results);

    let output_dir = cli.output.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(output_dir)?;

    let shift_path = with_timestamp(&output_dir.join("shift_path.txt"), cli.depth);
    let shift_file = File::create(&shift_path)?;
    let mut shift_writer = BufWriter::new(shift_file);
    for shifts in &state.shifts {
        serde_json::to_writer(&mut shift_writer, shifts)?;
        writeln!(shift_writer)?;
    }
    info!("シフトパス出力ファイル: {}", shift_path.display());

    let result_path = with_timestamp(&output_dir.join("result.json"), cli.depth);
    let result_file = File::create(&result_path)?;
    let mut result_writer = BufWriter::new(result_file);
    info!("探索結果出力ファイル: {}", result_path.display());

    let output = OutputFile {
        config: OutputConfig {
            mode: match cli.mode {
                SearchMode::Sequential => "sequential",
                SearchMode::Parallel => "parallel",
            },
            depth: cli.depth,
            cols: cli.cols,
            elapsed: format!("{elapsed:?}"),
        },
        result: OutputResult {
            max_count: state.max_count,
            results: state.results,
        },
    };
    serde_json::to_writer_pretty(&mut result_writer, &output)?;
    writeln!(result_writer)?;

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
            cols: 4,
            output: PathBuf::from("shift_path.txt"),
            checkpoint_interval: 100_000,
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
        cli.checkpoint_interval = 0;
        assert!(cli.validate(3).is_err());
    }

    #[test]
    fn cli_validation_accepts_depth_equal_to_available_primes() {
        let mut cli = test_cli();
        cli.depth = 3;
        assert!(cli.validate(3).is_ok());
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
