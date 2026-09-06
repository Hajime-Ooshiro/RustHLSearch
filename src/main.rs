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
const SEARCHED_PATH: &str = "searched.json";

#[derive(Serialize)]
struct OutputFile<'a> {
    config: OutputConfig<'a>,
    result: OutputResult<'a>,
}

#[derive(Serialize)]
struct OutputConfig<'a> {
    mode: &'a str,
    depth: usize,
    limit: usize,
    max_depth: usize,
    target: usize,
    cols: usize,
    primes_count: &'a str,
    elapsed: String,
}

#[derive(Serialize)]
struct OutputResult<'a> {
    max_count: usize,
    results: usize,
    shifts: &'a [Vec<usize>],
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

    #[arg(short, long, default_value_t = 447, help = "枝刈り下限値")]
    pub limit: usize,

    #[arg(long, default_value_t = 249, help = "打ち切り判定用 max-depth")]
    pub max_depth: usize,

    #[arg(short, long, default_value_t = 447, help = "打ち切り目標値")]
    pub target: usize,

    #[arg(long, default_value_t = 3159, help = "列数 (長さ)")]
    pub cols: usize,

    #[arg(long, help = "使用する素数の個数制限")]
    pub primes_count: Option<usize>,

    #[arg(
        short,
        long,
        default_value = "shift_path.json",
        help = "出力ファイルパス"
    )]
    pub output: PathBuf,
}

impl Cli {
    fn validate(&self, available_primes: usize) -> Result<(), String> {
        if self.depth == 0 {
            return Err("depth must be at least 1".to_string());
        }
        if self.cols == 0 {
            return Err("cols must be at least 1".to_string());
        }
        if self.limit > self.cols {
            return Err(format!(
                "limit ({}) cannot exceed cols ({})",
                self.limit, self.cols
            ));
        }
        if let Some(primes_count) = self.primes_count {
            if primes_count == 0 {
                return Err("primes-count must be at least 1".to_string());
            }
            if primes_count > available_primes {
                return Err(format!(
                    "primes-count ({}) cannot exceed available primes ({})",
                    primes_count, available_primes
                ));
            }
            if self.depth > primes_count {
                return Err(format!(
                    "depth ({}) cannot exceed primes-count ({})",
                    self.depth, primes_count
                ));
            }
        } else if self.depth > available_primes {
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

    let primes = match cli.primes_count {
        Some(cnt) => all_primes[..cnt].to_vec(),
        None => all_primes,
    };

    info!("HLSearch (Rust) 開始");
    info!(
        "設定: mode={:?} depth={} limit={} max_depth={} target={} primes_count={}",
        cli.mode,
        cli.depth,
        cli.limit,
        cli.max_depth,
        cli.target,
        primes.len()
    );

    let start_time = Instant::now();
    let shift_table = build_shift_table(&primes[..cli.depth], cli.cols);
    let mut state = State::new(primes, cli.limit, cli.cols, shift_table);
    state.max_depth = cli.max_depth;
    state.target = cli.target;

    match cli.mode {
        SearchMode::Sequential => {
            let checkpoint_path = Path::new(CHECKPOINT_PATH);
            let resume_path = checkpoint_path.exists().then_some(checkpoint_path);
            state.search_with_checkpoint(cli.depth, Some(checkpoint_path), resume_path)?;
            std::fs::rename(checkpoint_path, SEARCHED_PATH)?;
            info!(
                "チェックポイントを探索済みファイルへ変更: {}",
                SEARCHED_PATH
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

    let primes_count = cli
        .primes_count
        .map(|count| count.to_string())
        .unwrap_or_else(|| "all".to_string());
    let output = OutputFile {
        config: OutputConfig {
            mode: match cli.mode {
                SearchMode::Sequential => "sequential",
                SearchMode::Parallel => "parallel",
            },
            depth: cli.depth,
            limit: cli.limit,
            max_depth: cli.max_depth,
            target: cli.target,
            cols: cli.cols,
            primes_count: &primes_count,
            elapsed: format!("{elapsed:?}"),
        },
        result: OutputResult {
            max_count: state.max_count,
            results: state.results,
            shifts: &state.shifts,
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
            limit: 1,
            max_depth: 249,
            target: 447,
            cols: 4,
            primes_count: None,
            output: PathBuf::from("shift_path.txt"),
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
        cli.limit = 5;
        assert!(cli.validate(3).is_err());
        cli = test_cli();
        cli.primes_count = Some(4);
        assert!(cli.validate(3).is_err());
    }
}
