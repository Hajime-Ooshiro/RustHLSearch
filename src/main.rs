mod bitmask;
mod output;
mod primes;
mod search;

use clap::Parser;
use log::{info, LevelFilter};
use output::with_timestamp;
use primes::generate_primes;
use search::{build_shift_table, SearchMode, State};
use simple_logger::SimpleLogger;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

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
        default_value = "shift_path.txt",
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

    match cli.mode {
        SearchMode::Sequential => state.search(cli.depth),
        SearchMode::Parallel => {
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

    writeln!(writer, "# ---- config ----")?;
    writeln!(writer, "mode:{:?}", cli.mode)?;
    writeln!(writer, "depth:{}", cli.depth)?;
    writeln!(writer, "limit:{}", cli.limit)?;
    writeln!(writer, "max_depth:{}", cli.max_depth)?;
    writeln!(writer, "target:{}", cli.target)?;
    writeln!(writer, "cols:{}", cli.cols)?;
    writeln!(
        writer,
        "primes_count:{}",
        cli.primes_count
            .map(|c| c.to_string())
            .unwrap_or_else(|| "all".to_string())
    )?;
    writeln!(writer, "elapsed:{:?}", elapsed)?;
    writeln!(writer, "# ---- result ----")?;
    writeln!(writer, "max_count:{}", state.max_count)?;
    writeln!(writer, "results:{}", state.results)?;
    for shift in &state.shifts {
        writeln!(writer, "{:?}", shift)?;
    }

    info!("HLSearch 終了");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitmask::BitMask;
    use std::path::Path;

    #[test]
    fn generate_primes_handles_small_limits() {
        assert_eq!(generate_primes(0), Vec::<usize>::new());
        assert_eq!(generate_primes(1), Vec::<usize>::new());
        assert_eq!(generate_primes(2), vec![2]);
        assert_eq!(generate_primes(10), vec![2, 3, 5, 7]);
    }

    #[test]
    fn bitmask_tracks_logical_size_and_popcount() {
        let mut mask = BitMask::new_ones(65);
        for idx in 0..65 {
            mask.set(idx, false);
        }
        mask.set(0, true);
        mask.set(64, true);
        mask.set(65, true);
        assert_eq!(mask.count_ones(), 2);
        mask.set(0, false);
        assert_eq!(mask.count_ones(), 1);
    }

    #[test]
    fn build_shift_table_creates_expected_complement_masks() {
        let table = build_shift_table(&[2], 6);
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].len(), 2);
        assert_eq!(table[0][0].count_ones(), 3);
        assert_eq!(table[0][1].count_ones(), 3);
    }

    #[test]
    fn timestamped_path_preserves_parent_and_extension() {
        let path = with_timestamp(Path::new("results/shift_path.txt"));
        let file_name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(path.parent(), Some(Path::new("results")));
        assert!(file_name.starts_with("shift_path_"));
        assert!(file_name.ends_with(".txt"));
    }

    #[test]
    fn sequential_and_parallel_search_find_valid_results() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut sequential = State::new(primes.clone(), 1, cols, table.clone());
        sequential.search(2);
        let parallel = State::new(primes.clone(), 1, cols, table);
        let result = parallel.search_parallel(2);

        assert_eq!(sequential.results, 1);
        assert_eq!(result.results, 1);
        assert_eq!(result.shifts.len(), 1);
        assert_eq!(result.shifts[0].len(), 2);
        for (level, &shift) in result.shifts[0].iter().enumerate() {
            assert!(shift < primes[level]);
        }
    }

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
