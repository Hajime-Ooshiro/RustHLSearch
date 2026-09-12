use crate::bitmask::BitMask;
use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(feature = "cuda")]
use cudarc::driver::{CudaContext, LaunchConfig, PushKernelArg};
#[cfg(feature = "cuda")]
use cudarc::nvrtc::{compile_ptx_with_opts, CompileOptions};

#[cfg(feature = "cuda")]
const CUDA_KERNEL: &str = r#"
extern "C" __global__ void batch_and_popcount(
    const unsigned long long* masks,
    const unsigned int* candidate_masks,
    unsigned int* counts,
    unsigned int words,
    unsigned int depth,
    unsigned int candidate_count
) {
    const unsigned int candidate = blockIdx.x * blockDim.x + threadIdx.x;
    if (candidate >= candidate_count) {
        return;
    }

    unsigned int count = 0;
    for (unsigned int word = 0; word < words; ++word) {
        unsigned long long value = ~0ULL;
        for (unsigned int level = 0; level < depth; ++level) {
            const unsigned int mask = candidate_masks[candidate * depth + level];
            value &= masks[mask * words + word];
        }
        count += __popcll(value);
    }
    counts[candidate] = count;
}
"#;

/// 基底行の生成と補集合シフトテーブルの作成
pub fn build_shift_table(primes: &[usize], cols: usize) -> Vec<Vec<BitMask>> {
    let mut shift_table = Vec::with_capacity(primes.len());

    for &p in primes {
        let mut complement_shifts = Vec::with_capacity(p);
        for k in 0..p {
            let mut mask = BitMask::new_ones(cols);
            for col in (k..cols).step_by(p) {
                mask.set(col, false);
            }
            complement_shifts.push(mask);
        }
        shift_table.push(complement_shifts);
    }

    shift_table
}

#[derive(Deserialize, Serialize)]
struct Frame {
    level: usize,
    next_idx: usize,
}

#[derive(Deserialize, Serialize)]
struct StackFrame {
    level: usize,
    next_idx: usize,
}

#[derive(Deserialize, Serialize)]
struct Checkpoint {
    depth: usize,
    primes: Vec<usize>,
    max_depth: usize,
    target: usize,
    cols: usize,
    stack: Vec<StackFrame>,
    key: Vec<usize>,
    max_count: usize,
    results: usize,
    shifts: Vec<Vec<usize>>,
    target_results: usize,
    target_shifts: Vec<Vec<usize>>,
    node_count: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SearchMode {
    Sequential,
    Parallel,
    Cuda,
}

#[derive(Clone, Default)]
pub struct SharedResults {
    pub max_count: usize,
    pub results: usize,
    pub shifts: Vec<Vec<usize>>,
    pub target_results: usize,
    pub target_shifts: Vec<Vec<usize>>,
}

struct ParallelResults {
    max_count: AtomicUsize,
    results: Mutex<SharedResults>,
}

impl ParallelResults {
    fn observe_best(&self, count: usize) -> bool {
        loop {
            let current = self.max_count.load(Ordering::Relaxed);
            if count < current {
                return false;
            }
            if count == current {
                return true;
            }
            if self
                .max_count
                .compare_exchange_weak(current, count, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }

    fn merge(&self, local: SharedResults) {
        let max_count = self.max_count.load(Ordering::Relaxed);
        let mut results = self.results.lock().unwrap();

        if results.max_count < max_count {
            results.max_count = max_count;
            results.results = 0;
            results.shifts.clear();
        }
        if local.max_count == max_count {
            results.results += local.results;
            results.shifts.extend(local.shifts);
        }
        results.target_results += local.target_results;
        results.target_shifts.extend(local.target_shifts);
    }

    fn snapshot(&self) -> SharedResults {
        let mut results = self.results.lock().unwrap().clone();
        results.max_count = self.max_count.load(Ordering::Relaxed);
        results
    }
}

impl SharedResults {
    fn record_best(&mut self, count: usize, key: &[usize]) {
        if count > self.max_count {
            self.max_count = count;
            self.results = 1;
            self.shifts.clear();
            self.shifts.push(key.to_vec());
        } else if count == self.max_count {
            self.results += 1;
            self.shifts.push(key.to_vec());
        }
    }
}

#[derive(Clone)]
struct WorkItem {
    key: Vec<usize>,
    base_mask: BitMask,
}

#[cfg(any(feature = "cuda", test))]
struct BoundedBatchPaths<'a> {
    params: &'a [Vec<usize>],
    positions: Vec<usize>,
    exhausted: bool,
}

#[cfg(any(feature = "cuda", test))]
impl<'a> BoundedBatchPaths<'a> {
    fn new(params: &'a [Vec<usize>]) -> Self {
        Self {
            params,
            positions: params
                .iter()
                .map(|candidates| candidates.len() - 1)
                .collect(),
            exhausted: params.is_empty(),
        }
    }

    fn next_path(&mut self) -> Option<Vec<usize>> {
        if self.exhausted {
            return None;
        }

        let path = self
            .params
            .iter()
            .zip(&self.positions)
            .map(|(candidates, &position)| candidates[position])
            .collect();

        for level in (0..self.positions.len()).rev() {
            if self.positions[level] > 0 {
                self.positions[level] -= 1;
                return Some(path);
            }
            self.positions[level] = self.params[level].len() - 1;
        }
        self.exhausted = true;
        Some(path)
    }
}

pub struct State {
    pub primes: Vec<usize>,
    pub params: Vec<Vec<usize>>,
    pub max_depth: usize,
    pub target: usize,
    pub key: Vec<usize>,
    pub zero_mask: BitMask,
    pub max_count: usize,
    pub results: usize,
    pub shifts: Vec<Vec<usize>>,
    pub target_results: usize,
    pub target_shifts: Vec<Vec<usize>>,
    pub node_count: u64,
    pub checkpoint_interval: u64,
    pub parallel_tasks_per_thread: usize,
    shift_table: Vec<Vec<BitMask>>,
}

impl State {
    pub fn new(
        primes: Vec<usize>,
        cols: usize,
        shift_table: Vec<Vec<BitMask>>,
    ) -> Result<Self, String> {
        if primes.is_empty() {
            return Err("primes must contain at least one value".to_string());
        }
        if cols == 0 {
            return Err("cols must be at least 1".to_string());
        }
        if shift_table.len() != primes.len() {
            return Err(format!(
                "shift_table level count ({}) must match primes count ({})",
                shift_table.len(),
                primes.len()
            ));
        }
        for (level, (&prime, shifts)) in primes.iter().zip(&shift_table).enumerate() {
            if prime < 2 {
                return Err(format!("prime at level {level} must be at least 2"));
            }
            if shifts.len() != prime {
                return Err(format!(
                    "shift_table at level {level} contains {} shifts; expected {prime}",
                    shifts.len()
                ));
            }
            if shifts.iter().any(|mask| mask.size() != cols) {
                return Err(format!(
                    "shift_table at level {level} contains a mask with a different column count"
                ));
            }
        }

        let params = primes
            .iter()
            .map(|&prime| (prime / 2..prime).collect())
            .collect();

        Ok(State {
            primes,
            params,
            max_depth: 249,
            target: 447,
            key: Vec::new(),
            zero_mask: BitMask::new_ones(cols),
            max_count: 0,
            results: 0,
            shifts: Vec::new(),
            target_results: 0,
            target_shifts: Vec::new(),
            node_count: 0,
            checkpoint_interval: 100_000,
            parallel_tasks_per_thread: 4,
            shift_table,
        })
    }

    pub fn search_with_checkpoint(
        &mut self,
        depth: usize,
        checkpoint_path: Option<&Path>,
        resume_path: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let pb = progress_bar();
        let mut stack = if let Some(path) = resume_path {
            let checkpoint: Checkpoint = serde_json::from_reader(std::fs::File::open(path)?)?;
            if checkpoint.depth != depth
                || checkpoint.primes != self.primes
                || checkpoint.max_depth != self.max_depth
                || checkpoint.target != self.target
                || checkpoint.cols != self.zero_mask.size()
            {
                return Err(format!(
                    "チェックポイントの探索設定が現在の設定と一致しません (depth={}, max_depth={}, target={}, cols={})",
                    checkpoint.depth, checkpoint.max_depth, checkpoint.target, checkpoint.cols
                )
                .into());
            }
            self.key = checkpoint.key.clone();
            self.max_count = checkpoint.max_count;
            self.results = checkpoint.results;
            self.shifts = checkpoint.shifts;
            self.target_results = checkpoint.target_results;
            self.target_shifts = checkpoint.target_shifts;
            self.node_count = checkpoint.node_count;
            info!(
                "チェックポイントから探索を再開しました (nodes={})",
                checkpoint.node_count
            );

            self.rebuild_stack_and_masks(&checkpoint.stack)?
        } else {
            vec![Frame {
                level: 0,
                next_idx: self.params[0].len(),
            }]
        };
        let mut masks = self.rebuild_masks(depth);
        let mut checkpoint_due = false;

        while !stack.is_empty() {
            if checkpoint_due {
                if let Some(path) = checkpoint_path {
                    self.write_checkpoint(path, depth, &stack)?;
                }
                checkpoint_due = false;
            }
            let frame = stack.last_mut().expect("探索スタックが空です");
            if frame.next_idx == 0 {
                stack.pop();
                if stack.last().is_some() {
                    self.key.pop();
                }
                continue;
            }

            frame.next_idx -= 1;
            let level = frame.level;
            let i = self.params[level][frame.next_idx];
            self.key.push(i);
            self.node_count += 1;

            let (base_masks, node_masks) = masks.split_at_mut(level + 1);
            let count =
                node_masks[0].bitand_into_count(&base_masks[level], &self.shift_table[level][i]);

            if self.node_count.is_multiple_of(self.checkpoint_interval) {
                pb.set_position(self.node_count);
                pb.set_message(format!(
                    "best: {} | hits: {} | depth: {}",
                    self.max_count,
                    self.results,
                    self.key.len()
                ));
                checkpoint_due = true;
            }

            if should_prune(count, self.max_count, self.target, depth == self.max_depth) {
                self.key.pop();
                continue;
            }

            if level + 1 >= depth {
                if count > self.max_count {
                    self.max_count = count;
                    self.results = 1;
                    self.shifts.clear();
                    self.shifts.push(self.key.clone());
                } else if count == self.max_count {
                    self.results += 1;
                    self.shifts.push(self.key.clone());
                }
                if depth == self.max_depth && count == self.target {
                    self.target_results += 1;
                    self.target_shifts.push(self.key.clone());
                }
                self.key.pop();
                continue;
            }

            stack.push(Frame {
                level: level + 1,
                next_idx: self.params[level + 1].len(),
            });
        }
        pb.finish_with_message("探索完了");
        if let Some(path) = checkpoint_path {
            self.write_checkpoint(path, depth, &stack)?;
        }
        Ok(())
    }

    fn write_checkpoint(
        &self,
        path: &Path,
        depth: usize,
        stack: &[Frame],
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let temporary_path = path.with_extension("tmp");
        let stack_frames = stack
            .iter()
            .map(|f| StackFrame {
                level: f.level,
                next_idx: f.next_idx,
            })
            .collect();
        let checkpoint = Checkpoint {
            depth,
            primes: self.primes.clone(),
            max_depth: self.max_depth,
            target: self.target,
            cols: self.zero_mask.size(),
            stack: stack_frames,
            key: self.key.clone(),
            max_count: self.max_count,
            results: self.results,
            shifts: self.shifts.clone(),
            target_results: self.target_results,
            target_shifts: self.target_shifts.clone(),
            node_count: self.node_count,
        };
        let file = fs::File::create(&temporary_path)?;
        serde_json::to_writer_pretty(file, &checkpoint)?;
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(temporary_path, path)?;
        Ok(())
    }

    fn rebuild_stack_and_masks(
        &mut self,
        saved_stack: &[StackFrame],
    ) -> Result<Vec<Frame>, Box<dyn std::error::Error>> {
        let mut stack = Vec::new();

        for frame in saved_stack {
            if frame.level >= self.primes.len() {
                return Err("Invalid stack frame level".into());
            }
            stack.push(Frame {
                level: frame.level,
                next_idx: frame.next_idx,
            });
        }

        Ok(stack)
    }

    fn rebuild_masks(&self, depth: usize) -> Vec<BitMask> {
        let mut masks = vec![self.zero_mask.clone(); depth + 1];
        for (level, &shift_idx) in self.key.iter().enumerate() {
            let (base_masks, node_masks) = masks.split_at_mut(level + 1);
            node_masks[0]
                .bitand_into_count(&base_masks[level], &self.shift_table[level][shift_idx]);
        }
        masks
    }

    pub fn search_parallel(&self, depth: usize) -> SharedResults {
        let results = Arc::new(ParallelResults {
            max_count: AtomicUsize::new(0),
            results: Mutex::new(SharedResults::default()),
        });
        let node_count = Arc::new(AtomicU64::new(0));
        let pb = progress_bar();

        let target_tasks =
            rayon::current_num_threads().saturating_mul(self.parallel_tasks_per_thread);
        let split_depth = self.parallel_split_depth(depth, target_tasks);
        let work_items = self.parallel_work_items(split_depth);
        work_items.into_par_iter().for_each(|work_item| {
            let mut key = work_item.key;
            let mut masks = vec![self.zero_mask.clone(); depth + 1];
            masks[split_depth] = work_item.base_mask;
            let count = masks[split_depth].count_ones();
            let mut local = SharedResults::default();
            if split_depth == depth {
                if results.observe_best(count) {
                    local.record_best(count, &key);
                }
                if depth == self.max_depth && count == self.target {
                    local.target_results = 1;
                    local.target_shifts.push(key);
                }
                results.merge(local);
                return;
            }

            if should_prune(
                count,
                results.max_count.load(Ordering::Relaxed),
                self.target,
                depth == self.max_depth,
            ) {
                results.merge(local);
                return;
            }
            let mut stack = vec![Frame {
                level: split_depth,
                next_idx: self.params[split_depth].len(),
            }];
            let mut local_nodes = 0_u64;

            while let Some(frame) = stack.last_mut() {
                if frame.next_idx == 0 {
                    stack.pop();
                    if stack.last().is_some() {
                        key.pop();
                    }
                    continue;
                }

                frame.next_idx -= 1;
                let level = frame.level;
                let idx = self.params[level][frame.next_idx];
                key.push(idx);
                local_nodes += 1;
                let (base_masks, node_masks) = masks.split_at_mut(level + 1);
                let c_count = node_masks[0]
                    .bitand_into_count(&base_masks[level], &self.shift_table[level][idx]);

                if local_nodes == self.checkpoint_interval {
                    let n = node_count.fetch_add(local_nodes, Ordering::Relaxed) + local_nodes;
                    local_nodes = 0;
                    pb.set_position(n);
                    let shared = results.snapshot();
                    pb.set_message(format!(
                        "best: {} | hits: {} | depth: {}",
                        shared.max_count,
                        shared.results,
                        key.len()
                    ));
                }

                if should_prune(
                    c_count,
                    results.max_count.load(Ordering::Relaxed),
                    self.target,
                    depth == self.max_depth,
                ) {
                    key.pop();
                    continue;
                }

                if level + 1 >= depth {
                    if results.observe_best(c_count) {
                        local.record_best(c_count, &key);
                    }
                    if depth == self.max_depth && c_count == self.target {
                        local.target_results += 1;
                        local.target_shifts.push(key.clone());
                    }
                    key.pop();
                    continue;
                }

                stack.push(Frame {
                    level: level + 1,
                    next_idx: self.params[level + 1].len(),
                });
            }
            node_count.fetch_add(local_nodes, Ordering::Relaxed);
            results.merge(local);
        });

        pb.finish_with_message("探索完了");
        results.snapshot()
    }

    #[cfg(feature = "cuda")]
    pub fn search_cuda_bounded(&mut self, depth: usize, batch_size: usize) -> Result<(), String> {
        if depth == 0 || depth > self.params.len() {
            return Err(format!(
                "CUDA search depth ({depth}) must be between 1 and {}",
                self.params.len()
            ));
        }
        if batch_size
            .checked_mul(depth)
            .and_then(|size| u32::try_from(size).ok())
            .is_none()
        {
            return Err("CUDA batch-size multiplied by depth must not exceed u32::MAX".to_string());
        }
        let words = self.zero_mask.words().len();
        let (host_masks, mask_offsets) = self.cuda_masks();
        u32::try_from(host_masks.len())
            .map_err(|_| "CUDA search supports at most u32::MAX mask words".to_string())?;
        let words_u32 = u32::try_from(words)
            .map_err(|_| "CUDA search supports at most u32::MAX mask words".to_string())?;
        let depth_u32 = u32::try_from(depth)
            .map_err(|_| "CUDA search supports at most u32::MAX levels".to_string())?;

        let context = CudaContext::new(0)
            .map_err(|error| format!("CUDA device 0 の初期化に失敗しました: {error:?}"))?;
        let stream = context.default_stream();
        let ptx = compile_ptx_with_opts(
            CUDA_KERNEL,
            CompileOptions {
                arch: Some("compute_86"),
                ..Default::default()
            },
        )
        .map_err(|error| format!("CUDA カーネルのコンパイルに失敗しました: {error:?}"))?;
        let module = context
            .load_module(ptx)
            .map_err(|error| format!("CUDA モジュールのロードに失敗しました: {error:?}"))?;
        let function = module
            .load_function("batch_and_popcount")
            .map_err(|error| format!("CUDA カーネルの取得に失敗しました: {error:?}"))?;
        let device_masks = stream
            .clone_htod(&host_masks)
            .map_err(|error| format!("CUDA マスク転送に失敗しました: {error:?}"))?;
        let mut paths = BoundedBatchPaths::new(&self.params[..depth]);
        let pb = progress_bar();

        loop {
            let mut batch_paths = Vec::with_capacity(batch_size);
            let mut candidate_masks = Vec::with_capacity(batch_size.saturating_mul(depth));
            for _ in 0..batch_size {
                let Some(path) = paths.next_path() else {
                    break;
                };
                for (level, &shift) in path.iter().enumerate() {
                    let mask = mask_offsets[level] + shift;
                    candidate_masks.push(u32::try_from(mask).map_err(|_| {
                        "CUDA search supports at most u32::MAX shift masks".to_string()
                    })?);
                }
                batch_paths.push(path);
            }
            if batch_paths.is_empty() {
                break;
            }

            let candidate_count = u32::try_from(batch_paths.len())
                .map_err(|_| "CUDA batch-size must not exceed u32::MAX".to_string())?;
            let device_candidates = stream
                .clone_htod(&candidate_masks)
                .map_err(|error| format!("CUDA 候補転送に失敗しました: {error:?}"))?;
            let mut device_counts = stream
                .alloc_zeros::<u32>(batch_paths.len())
                .map_err(|error| format!("CUDA 結果バッファの確保に失敗しました: {error:?}"))?;

            let mut launch = stream.launch_builder(&function);
            launch.arg(&device_masks);
            launch.arg(&device_candidates);
            launch.arg(&mut device_counts);
            launch.arg(&words_u32);
            launch.arg(&depth_u32);
            launch.arg(&candidate_count);
            unsafe {
                launch
                    .launch(LaunchConfig::for_num_elems(candidate_count))
                    .map_err(|error| format!("CUDA カーネル実行に失敗しました: {error:?}"))?;
            }
            let counts = stream
                .clone_dtoh(&device_counts)
                .map_err(|error| format!("CUDA 結果転送に失敗しました: {error:?}"))?;

            for (path, count) in batch_paths.iter().zip(counts) {
                let count = count as usize;
                if count > self.max_count {
                    self.max_count = count;
                    self.results = 1;
                    self.shifts.clear();
                    self.shifts.push(path.clone());
                } else if count == self.max_count {
                    self.results += 1;
                    self.shifts.push(path.clone());
                }
                if depth == self.max_depth && count == self.target {
                    self.target_results += 1;
                    self.target_shifts.push(path.clone());
                }
            }
            self.node_count += batch_paths.len() as u64;
            pb.set_position(self.node_count);
            pb.set_message(format!(
                "best: {} | hits: {} | batch: {}",
                self.max_count,
                self.results,
                batch_paths.len()
            ));
        }

        pb.finish_with_message("CUDA バッチ探索完了");
        Ok(())
    }

    #[cfg(not(feature = "cuda"))]
    pub fn search_cuda_bounded(&mut self, _depth: usize, _batch_size: usize) -> Result<(), String> {
        Err(
            "CUDA モードには CUDA 機能を有効にしてください: cargo run --features cuda -- --mode cuda"
                .to_string(),
        )
    }

    #[cfg(feature = "cuda")]
    fn cuda_masks(&self) -> (Vec<u64>, Vec<usize>) {
        let mut masks = Vec::new();
        let mut offsets = Vec::with_capacity(self.shift_table.len());
        for shifts in &self.shift_table {
            offsets.push(masks.len() / self.zero_mask.words().len());
            for mask in shifts {
                masks.extend_from_slice(mask.words());
            }
        }
        (masks, offsets)
    }

    fn parallel_split_depth(&self, depth: usize, target_tasks: usize) -> usize {
        let mut task_count: usize = 1;
        for level in 0..depth {
            task_count = task_count.saturating_mul(self.params[level].len());
            if task_count >= target_tasks {
                return level + 1;
            }
        }
        depth
    }

    fn parallel_work_items(&self, split_depth: usize) -> Vec<WorkItem> {
        let mut work_items = vec![WorkItem {
            key: Vec::with_capacity(split_depth),
            base_mask: self.zero_mask.clone(),
        }];

        for level in 0..split_depth {
            let mut next_items = Vec::with_capacity(work_items.len() * self.params[level].len());
            for item in work_items {
                for &shift in self.params[level].iter().rev() {
                    let mut base_mask = self.zero_mask.clone();
                    base_mask.bitand_into_count(&item.base_mask, &self.shift_table[level][shift]);
                    let mut key = item.key.clone();
                    key.push(shift);
                    next_items.push(WorkItem { key, base_mask });
                }
            }
            work_items = next_items;
        }

        work_items
    }
}

#[inline]
fn should_prune(count: usize, max_count: usize, target: usize, records_target: bool) -> bool {
    count < max_count && (!records_target || count < target)
}

fn progress_bar() -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} [{elapsed_precise}] nodes: {human_pos} ({per_sec}) {msg}")
            .unwrap(),
    );
    pb
}

#[cfg(test)]
mod tests {
    use super::{build_shift_table, should_prune, State};

    #[test]
    fn target_only_prevents_pruning_when_target_results_are_recorded() {
        assert!(should_prune(2, 3, 1, false));
        assert!(should_prune(2, 3, 4, true));
        assert!(!should_prune(2, 3, 1, true));
        assert!(!should_prune(3, 3, 4, true));
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
    fn sequential_and_parallel_search_find_maximum_results() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut sequential = State::new(primes.clone(), cols, table.clone()).unwrap();
        sequential.search_with_checkpoint(2, None, None).unwrap();
        let parallel = State::new(primes.clone(), cols, table).unwrap();
        let result = parallel.search_parallel(2);

        assert_eq!(sequential.max_count, 2);
        assert_eq!(sequential.results, 1);
        assert_eq!(result.max_count, sequential.max_count);
        assert_eq!(result.results, sequential.results);
        assert_eq!(result.shifts.len(), result.results);
        for shifts in &result.shifts {
            assert_eq!(shifts.len(), 2);
            for (level, &shift) in shifts.iter().enumerate() {
                assert!(parallel.params[level].contains(&shift));
            }
        }
    }

    #[test]
    fn sequential_and_parallel_search_record_all_maximum_leaves() {
        let primes = vec![2];
        let cols = 4;
        let table = build_shift_table(&primes, cols);

        let mut sequential = State::new(primes.clone(), cols, table.clone()).unwrap();
        sequential.search_with_checkpoint(1, None, None).unwrap();

        let parallel = State::new(primes, cols, table).unwrap();
        let result = parallel.search_parallel(1);

        assert_eq!(sequential.max_count, 2);
        assert_eq!(sequential.results, 1);
        assert_eq!(sequential.shifts, vec![vec![1]]);
        assert_eq!(result.max_count, 2);
        assert_eq!(result.results, 1);
        assert_eq!(result.shifts, vec![vec![1]]);
    }

    #[test]
    fn target_results_include_paths_below_the_maximum() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);

        let mut sequential = State::new(primes.clone(), cols, table.clone()).unwrap();
        sequential.max_depth = 2;
        sequential.target = 1;
        sequential.search_with_checkpoint(2, None, None).unwrap();

        let mut parallel = State::new(primes, cols, table).unwrap();
        parallel.max_depth = 2;
        parallel.target = 1;
        let result = parallel.search_parallel(2);

        assert_eq!(sequential.max_count, 2);
        assert_eq!(sequential.results, 1);
        assert_eq!(sequential.target_results, 1);
        assert_eq!(sequential.target_shifts.len(), 1);
        assert_eq!(result.max_count, 2);
        assert_eq!(result.results, 1);
        assert_eq!(result.target_results, 1);
        assert_eq!(result.target_shifts.len(), 1);
    }

    #[test]
    fn checkpoint_interval_defaults_to_100k() {
        let table = build_shift_table(&[2], 4);
        let state = State::new(vec![2], 4, table).unwrap();
        assert_eq!(state.checkpoint_interval, 100_000);
    }

    #[test]
    fn params_contain_ranges_from_half_to_one_before_each_prime() {
        let primes = vec![2, 3, 5, 7];
        let table = build_shift_table(&primes, 8);
        let state = State::new(primes, 8, table).unwrap();

        assert_eq!(
            state.params,
            vec![vec![1], vec![1, 2], vec![2, 3, 4], vec![3, 4, 5, 6]]
        );
    }

    #[test]
    fn bounded_batch_paths_keep_the_sequential_descending_order() {
        let candidates = vec![vec![1], vec![1, 2], vec![2, 3, 4]];
        let mut paths = super::BoundedBatchPaths::new(&candidates);
        assert_eq!(paths.next_path(), Some(vec![1, 2, 4]));
        assert_eq!(paths.next_path(), Some(vec![1, 2, 3]));
        assert_eq!(paths.next_path(), Some(vec![1, 2, 2]));
        assert_eq!(paths.next_path(), Some(vec![1, 1, 4]));
    }

    #[test]
    fn state_creation_rejects_inconsistent_shift_table() {
        let error = match State::new(vec![2], 4, Vec::new()) {
            Ok(_) => panic!("inconsistent shift table must be rejected"),
            Err(error) => error,
        };

        assert_eq!(
            error,
            "shift_table level count (0) must match primes count (1)"
        );
    }

    #[test]
    fn checkpoint_interval_can_be_set() {
        let table = build_shift_table(&[2], 4);
        let mut state = State::new(vec![2], 4, table).unwrap();
        state.checkpoint_interval = 5_000;
        assert_eq!(state.checkpoint_interval, 5_000);
    }

    #[test]
    fn rebuild_stack_and_masks_recovers_search_position() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut state = State::new(primes.clone(), cols, table).unwrap();

        state.key = vec![1, 0];
        state.node_count = 10;
        state.max_count = 2;
        state.results = 0;

        let saved_stack = vec![super::StackFrame {
            level: 1,
            next_idx: 2,
        }];

        let rebuilt_stack = state.rebuild_stack_and_masks(&saved_stack).unwrap();

        assert_eq!(rebuilt_stack.len(), 1);
        assert_eq!(rebuilt_stack[0].level, 1);
        assert_eq!(rebuilt_stack[0].next_idx, 2);
        assert_eq!(rebuilt_stack[0].level, 1);
    }

    #[test]
    fn stack_frame_serialization_is_lightweight() {
        use serde_json;
        let frame = super::StackFrame {
            level: 5,
            next_idx: 42,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(json.contains("\"level\":5"));
        assert!(json.contains("\"next_idx\":42"));
        assert!(!json.contains("base_mask"));
    }

    #[test]
    fn checkpoint_stores_only_key_level_and_next_idx() {
        use serde_json;
        let checkpoint = super::Checkpoint {
            depth: 2,
            primes: vec![2, 3],
            max_depth: 2,
            target: 1,
            cols: 4,
            stack: vec![super::StackFrame {
                level: 0,
                next_idx: 1,
            }],
            key: vec![1],
            max_count: 2,
            results: 0,
            shifts: vec![],
            target_results: 0,
            target_shifts: vec![],
            node_count: 100,
        };
        let json = serde_json::to_string_pretty(&checkpoint).unwrap();
        assert!(json.contains("depth"));
        assert!(json.contains("key"));
        assert!(json.contains("level"));
        assert!(json.contains("next_idx"));
        assert!(!json.contains("zero_mask"));
        assert!(!json.contains("base_mask"));
    }

    #[test]
    fn multiple_keys_rebuild_to_correct_masks() {
        let primes = vec![2, 3, 5];
        let cols = 8;
        let table = build_shift_table(&primes, cols);
        let mut state = State::new(primes.clone(), cols, table).unwrap();

        state.key = vec![1, 2, 1];

        let saved_stack = vec![super::StackFrame {
            level: 2,
            next_idx: 3,
        }];

        let rebuilt = state.rebuild_stack_and_masks(&saved_stack).unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].level, 2);

        let masks = state.rebuild_masks(3);
        assert_eq!(masks[3].count_ones(), 2);
    }
}
