use chrono::Local;
use clap::{Parser, ValueEnum};
use indicatif::{ProgressBar, ProgressStyle};
use log::{info, LevelFilter};
use simple_logger::SimpleLogger;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;
use rayon::prelude::*;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// 出力ファイルパスにタイムスタンプ (YYYYMMDD_HHMMSS) を挿入する
/// 例: "shift_path.txt" -> "shift_path_20260831_153000.txt"
/// 拡張子が無い場合は末尾にそのまま付加する
fn with_timestamp(path: &PathBuf) -> PathBuf {
    let timestamp = Local::now().format("%Y%m%d_%H%M%S").to_string();

    let parent = path.parent();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    let ext = path.extension().and_then(|s| s.to_str());

    let new_name = match ext {
        Some(ext) => format!("{}_{}.{}", stem, timestamp, ext),
        None => format!("{}_{}", stem, timestamp),
    };

    match parent {
        Some(p) if !p.as_os_str().is_empty() => p.join(new_name),
        _ => PathBuf::from(new_name),
    }
}

/// 素数生成（エラトステネスの篩）
pub fn generate_primes(limit: usize) -> Vec<usize> {
    if limit < 2 {
        return Vec::new();
    }
    let mut is_prime = vec![true; limit + 1];
    is_prime[0] = false;
    is_prime[1] = false;

    let sqrt_limit = (limit as f64).sqrt() as usize;
    for p in 2..=sqrt_limit {
        if is_prime[p] {
            let mut step = p * p;
            while step <= limit {
                is_prime[step] = false;
                step += p;
            }
        }
    }

    is_prime
        .iter()
        .enumerate()
        .filter_map(|(n, &p)| if p { Some(n) } else { None })
        .collect()
}

/// BitVec による高速なビットマスク操作構造体
#[derive(Clone, Debug)]
pub struct BitMask {
    data: Vec<u64>,
    size: usize,
}

impl BitMask {
    pub fn new_ones(size: usize) -> Self {
        let num_words = (size + 63) / 64;
        let mut data = vec![u64::MAX; num_words];
        // 不要な余りビットをクリア
        if size % 64 != 0 {
            let remainder = size % 64;
            data[num_words - 1] = (1u64 << remainder) - 1;
        }
        BitMask { data, size }
    }

    pub fn new_zeros(size: usize) -> Self {
        let num_words = (size + 63) / 64;
        BitMask {
            data: vec![0; num_words],
            size,
        }
    }

    /// bitwise AND
    #[inline]
    pub fn bitand(&self, rhs: &Self) -> Self {
        let data = self
            .data
            .iter()
            .zip(rhs.data.iter())
            .map(|(&a, &b)| a & b)
            .collect();
        BitMask {
            data,
            size: self.size,
        }
    }

    /// 1 (true) のビット数をカウント (popcount)
    #[inline]
    pub fn count_ones(&self) -> usize {
        self.data.iter().map(|&w| w.count_ones() as usize).sum()
    }

    /// 指定したインデックスのビットをセット
    #[inline]
    pub fn set(&mut self, idx: usize, val: bool) {
        if idx >= self.size {
            return;
        }
        let word = idx / 64;
        let bit = idx % 64;
        if val {
            self.data[word] |= 1u64 << bit;
        } else {
            self.data[word] &= !(1u64 << bit);
        }
    }
}

/// 基底行の生成と補集合シフトテーブルの作成
pub fn build_shift_table(primes: &[usize], cols: usize) -> Vec<Vec<BitMask>> {
    let mut shift_table = Vec::with_capacity(primes.len());

    for &p in primes {
        let mut complement_shifts = Vec::with_capacity(p);
        for k in 0..p {
            // ~shift_array(base_row, k) に相当するビットマスクを直接構築
            let mut mask = BitMask::new_ones(cols);
            for col in 0..cols {
                let idx = col + 1; // 1-indexed (idx % p == 1)
                if col >= k {
                    let orig_idx = idx - k;
                    if orig_idx % p == 1 {
                        mask.set(col, false); // NOT演算
                    }
                }
            }
            complement_shifts.push(mask);
        }
        shift_table.push(complement_shifts);
    }

    shift_table
}

/// スタックフレーム構造体（非再帰 DFS 用）
#[derive(Clone)]
struct Frame {
    level: usize,
    base_mask: BitMask,
    next_idx: usize,
    next_p: usize,
}

/// 探索モード（逐次 / 並列）
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum SearchMode {
    /// 単一スレッドの非再帰 DFS（進捗バー表示あり）
    Sequential,
    /// Rayon による並列 DFS（デフォルト）
    Parallel,
}

/// CLI 引数定義 (clap)
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

    #[arg(short, long, default_value = "shift_path.txt", help = "出力ファイルパス")]
    pub output: PathBuf,
}

/// スレッド間で共有する最良結果データ
#[derive(Default)]
pub struct SharedResults {
    pub max_count: usize,
    pub results: usize,
    pub shifts: Vec<Vec<usize>>,
}

pub struct State {
    pub primes: Vec<usize>,
    pub depth: usize,
    pub limit: usize,
    pub max_depth: usize,
    pub target: usize,
    pub cols: usize,

    pub key: Vec<usize>,
    pub zero_mask: BitMask,
    pub max_count: usize,
    pub results: usize,
    pub shifts: Vec<Vec<usize>>,
    pub node_count: u64,
    shift_table: Vec<Vec<BitMask>>,
}

impl State {
    pub fn new(
        primes: Vec<usize>,
        depth: usize,
        limit: usize,
        max_depth: usize,
        target: usize,
        cols: usize,
        shift_table: Vec<Vec<BitMask>>,
    ) -> Self {
        let zero_mask = BitMask::new_ones(cols);
        State {
            primes,
            depth,
            limit,
            max_depth,
            target,
            cols,
            key: Vec::new(),
            zero_mask,
            max_count: 0,
            results: 0,
            shifts: Vec::new(),
            node_count: 0,
            shift_table,
        }
    }

    /// スタックによる非再帰 DFS 探索実行
    pub fn search(&mut self, depth: usize) {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] nodes: {human_pos} ({per_sec}) {msg}")
                .unwrap(),
        );

        let mut stack = vec![Frame {
            level: 0,
            base_mask: self.zero_mask.clone(),
            next_idx: 0,
            next_p: self.primes[0],
        }];

        while let Some(frame) = stack.last_mut() {
            if frame.next_idx >= frame.next_p {
                stack.pop();
                if let Some(parent) = stack.last() {
                    self.key.pop();
                    self.zero_mask = parent.base_mask.clone();
                }
                continue;
            }

            let i = frame.next_idx;
            let level = frame.level;
            let base_mask = frame.base_mask.clone();
            frame.next_idx += 1;

            self.key.push(i);
            self.node_count += 1;

            let row_complement = &self.shift_table[level][i];
            let node_mask = base_mask.bitand(row_complement);
            let count = node_mask.count_ones();

            if self.node_count % 10_000 == 0 {
                pb.set_position(self.node_count);
                pb.set_message(format!(
                    "best: {} | hits: {} | depth: {}",
                    self.max_count, self.results, self.key.len()
                ));
            }

            // 枝刈り
            if count < self.limit {
                self.key.pop();
                continue;
            }

            // 葉ノード処理（現在ノードの count == limit なら記録して探索全体を終了）
            if level + 1 >= depth {
                if count > self.max_count {
                    self.max_count = count;
                }
                if count == self.limit {
                    self.results += 1;
                    self.shifts.push(self.key.clone());
                    info!("target level={} key={:?} count={}", level, self.key, count);
                    self.key.pop();
                    break;
                }
                self.key.pop();
                continue;
            }

            // 子階層への展開
            self.zero_mask = node_mask.clone();
            let next_p_child = self.primes[level + 1];
            stack.push(Frame {
                level: level + 1,
                base_mask: node_mask,
                next_idx: 0,
                next_p: next_p_child,
            });
        }

        pb.finish_with_message("探索完了");
    }

    /// Rayon による並列 DFS 探索実行
    pub fn search_parallel(&self, depth: usize) -> SharedResults {
        let num_threads = rayon::current_num_threads();
        info!("並列探索スレッド数: {}", num_threads);

        let max_count = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(AtomicUsize::new(0));
        let shifts = Arc::new(Mutex::new(Vec::<Vec<usize>>::new()));
        let node_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        // 進捗バー（indicatif の ProgressBar は内部で共有状態を持つため
        // 複数スレッドから安全に更新できる）
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.green} [{elapsed_precise}] nodes: {human_pos} ({per_sec}) {msg}")
                .unwrap(),
        );

        let p0 = self.primes[0];

        (0..p0).into_par_iter().for_each(|i| {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            let mut key = vec![i];
            let row_complement = &self.shift_table[0][i];
            let base_mask = self.zero_mask.bitand(row_complement);
            let count = base_mask.count_ones();

            if count < self.limit {
                return;
            }

            if depth == 1 {
                max_count.fetch_max(count, Ordering::Relaxed);
                if count == self.limit && !stop.swap(true, Ordering::Relaxed) {
                    results.fetch_add(1, Ordering::Relaxed);
                    shifts.lock().unwrap().push(key.clone());
                    info!("target level=0 key={:?} count={}", key, count);
                }
                return;
            }

            let mut stack = vec![Frame {
                level: 1,
                base_mask,
                next_idx: 0,
                next_p: self.primes[1],
            }];

            while let Some(frame) = stack.last_mut() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }

                if frame.next_idx >= frame.next_p {
                    stack.pop();
                    if stack.last().is_some() {
                        key.pop();
                    }
                    continue;
                }

                let idx = frame.next_idx;
                let level = frame.level;
                let current_base = frame.base_mask.clone();
                frame.next_idx += 1;

                key.push(idx);
                let n = node_count.fetch_add(1, Ordering::Relaxed) + 1;

                let complement = &self.shift_table[level][idx];
                let node_mask = current_base.bitand(complement);
                let c_count = node_mask.count_ones();

                if n % 10_000 == 0 {
                    pb.set_position(n);
                    pb.set_message(format!(
                        "best: {} | hits: {} | depth: {}",
                        max_count.load(Ordering::Relaxed),
                        results.load(Ordering::Relaxed),
                        key.len()
                    ));
                }

                if c_count < self.limit {
                    key.pop();
                    continue;
                }

                if level + 1 >= depth {
                    max_count.fetch_max(c_count, Ordering::Relaxed);
                    if c_count == self.limit && !stop.swap(true, Ordering::Relaxed) {
                        results.fetch_add(1, Ordering::Relaxed);
                        shifts.lock().unwrap().push(key.clone());
                        info!("target level={} key={:?} count={}", level, key, c_count);
                    }
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    key.pop();
                    continue;
                }

                let next_p_child = self.primes[level + 1];
                stack.push(Frame {
                    level: level + 1,
                    base_mask: node_mask,
                    next_idx: 0,
                    next_p: next_p_child,
                });
            }
        });

        pb.finish_with_message("探索完了");

        let final_shifts = shifts.lock().unwrap();
        SharedResults {
            max_count: max_count.load(Ordering::Relaxed),
            results: results.load(Ordering::Relaxed),
            shifts: final_shifts.clone(),
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    SimpleLogger::new()
        .with_level(LevelFilter::Info)
        .init()?;

    let cli = Cli::parse();

    let all_primes = generate_primes(1579);
    let primes = match cli.primes_count {
        Some(cnt) => all_primes[..cnt].to_vec(),
        None => all_primes,
    };

    if cli.depth > primes.len() {
        eprintln!(
            "エラー: depth ({}) が指定可能な素数の数 ({}) を超えています",
            cli.depth,
            primes.len()
        );
        std::process::exit(1);
    }

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

    let mut state = State::new(
        primes,
        cli.depth,
        cli.limit,
        cli.max_depth,
        cli.target,
        cli.cols,
        shift_table,
    );

    match cli.mode {
        SearchMode::Sequential => {
            info!("探索モード: sequential (search)");
            state.search(cli.depth);
        }
        SearchMode::Parallel => {
            info!("探索モード: parallel (search_parallel)");
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

    // 結果の出力（ファイル名にタイムスタンプを付与）
    let output_path = with_timestamp(&cli.output);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(&output_path)?;
    let mut writer = BufWriter::new(file);
    info!("出力ファイル: {}", output_path.display());

    // 実行時の設定を出力ファイルの先頭に記録
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