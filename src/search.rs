use crate::bitmask::BitMask;
use log::{debug, info};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 基底行の生成と補集合シフトテーブルの作成
pub fn build_shift_table(primes: &[usize], cols: usize) -> Vec<Vec<BitMask>> {
    let mut shift_table = Vec::with_capacity(primes.len());

    for &p in primes {
        let mut complement_shifts = Vec::with_capacity(p);
        for k in 0..p {
            let mut mask = BitMask::new_ones(cols);
            for col in 0..cols {
                let idx = col + 1;
                if col >= k {
                    let orig_idx = idx - k;
                    if orig_idx % p == 1 {
                        mask.set(col, false);
                    }
                }
            }
            complement_shifts.push(mask);
        }
        shift_table.push(complement_shifts);
    }

    shift_table
}

#[derive(Clone, Deserialize, Serialize)]
struct Frame {
    level: usize,
    next_idx: usize,
}

fn default_checkpoint_mode() -> SearchMode {
    SearchMode::Sequential
}

#[derive(Clone, Deserialize, Serialize)]
struct ParallelInProgress {
    work_index: usize,
    stack: Vec<Frame>,
    key: Vec<usize>,
}

#[derive(Deserialize, Serialize)]
struct Checkpoint {
    #[serde(default = "default_checkpoint_mode")]
    mode: SearchMode,
    depth: usize,
    primes: Vec<usize>,
    cols: usize,
    stack: Vec<Frame>,
    key: Vec<usize>,
    max_count: usize,
    results: usize,
    shifts: Vec<Vec<usize>>,
    node_count: u64,
    #[serde(default)]
    split_depth: usize,
    #[serde(default)]
    completed: Vec<usize>,
    #[serde(default)]
    in_progress: Vec<ParallelInProgress>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, clap::ValueEnum, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchMode {
    Sequential,
    #[default]
    Parallel,
}

#[derive(Clone, Debug, Default)]
pub struct SharedResults {
    pub max_count: usize,
    pub results: usize,
    pub shifts: Vec<Vec<usize>>,
}

struct ParallelResults {
    max_count: AtomicUsize,
    results: Mutex<SharedResults>,
}

impl ParallelResults {
    fn record_best(&self, count: usize, key: &[usize]) {
        loop {
            let current = self.max_count.load(Ordering::Relaxed);
            if count < current {
                return;
            }
            if count == current {
                let mut results = self.results.lock().unwrap();
                if self.max_count.load(Ordering::Relaxed) == count
                    && !results.shifts.iter().any(|existing| existing.as_slice() == key)
                {
                    results.results += 1;
                    results.shifts.push(key.to_vec());
                }
                return;
            }
            if self
                .max_count
                .compare_exchange_weak(current, count, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                let mut results = self.results.lock().unwrap();
                if self.max_count.load(Ordering::Relaxed) == count {
                    results.results = 1;
                    results.shifts.clear();
                    results.shifts.push(key.to_vec());
                }
                return;
            }
        }
    }

    fn snapshot(&self) -> SharedResults {
        let mut results = self.results.lock().unwrap().clone();
        results.max_count = self.max_count.load(Ordering::Relaxed);
        results
    }
}

#[derive(Clone)]
struct WorkItem {
    key: Vec<usize>,
    base_mask: BitMask,
}

struct ParallelProgress {
    completed: HashSet<usize>,
    in_progress: HashMap<usize, ParallelInProgress>,
}

struct ParallelSearchCtx<'a> {
    depth: usize,
    split_depth: usize,
    results: &'a ParallelResults,
    node_count: &'a AtomicU64,
    progress: &'a Mutex<ParallelProgress>,
    checkpoint_path: Option<&'a Path>,
    checkpoint_lock: Mutex<()>,
    last_checkpoint_nodes: AtomicU64,
}

struct ParallelJob {
    index: usize,
    work_item: WorkItem,
    resume: Option<(Vec<Frame>, Vec<usize>)>,
}

pub struct State {
    pub primes: Vec<usize>,
    pub key: Vec<usize>,
    pub zero_mask: BitMask,
    pub max_count: usize,
    pub results: usize,
    pub shifts: Vec<Vec<usize>>,
    pub node_count: u64,
    pub checkpoint_interval: u64,
    shift_table: Vec<Vec<BitMask>>,
}

impl State {
    pub fn new(primes: Vec<usize>, cols: usize, shift_table: Vec<Vec<BitMask>>) -> Self {
        State {
            primes,
            key: Vec::new(),
            zero_mask: BitMask::new_ones(cols),
            max_count: 0,
            results: 0,
            shifts: Vec::new(),
            node_count: 0,
            checkpoint_interval: 100_000,
            shift_table,
        }
    }

    fn aggregate_leaf_result(&mut self, count: usize, key: &[usize]) {
        if count > self.max_count {
            self.max_count = count;
            self.results = 1;
            self.shifts.clear();
            self.shifts.push(key.to_vec());
            debug!("best level={} key={:?} count={}", key.len() - 1, key, count);
        } else if count == self.max_count {
            self.results += 1;
            self.shifts.push(key.to_vec());
            debug!("best level={} key={:?} count={}", key.len() - 1, key, count);
        }
    }

    pub fn search_with_checkpoint(
        &mut self,
        depth: usize,
        checkpoint_path: Option<&Path>,
        resume_path: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut stack = if let Some(path) = resume_path {
            let checkpoint = Self::load_checkpoint(path)?;
            if checkpoint.mode != SearchMode::Sequential {
                return Err(
                    "並列モードのチェックポイントは --mode parallel でのみ再開できます".into(),
                );
            }
            self.apply_checkpoint_config(&checkpoint, depth)?;
            self.key = checkpoint.key.clone();
            self.max_count = checkpoint.max_count;
            self.results = checkpoint.results;
            self.shifts = checkpoint.shifts;
            self.node_count = checkpoint.node_count;
            info!(
                "チェックポイントから探索を再開しました (nodes={})",
                checkpoint.node_count
            );

            self.restore_stack(&checkpoint.stack)?
        } else {
            vec![Frame {
                level: 0,
                next_idx: self.primes[0],
            }]
        };
        let mut masks = self.rebuild_masks(depth, &self.key);
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
            let i = frame.next_idx;
            let level = frame.level;
            self.key.push(i);
            self.node_count += 1;

            let (base_masks, node_masks) = masks.split_at_mut(level + 1);
            let count =
                node_masks[0].bitand_into_count(&base_masks[level], &self.shift_table[level][i]);

            if self.node_count.is_multiple_of(self.checkpoint_interval) {
                info!(
                    "探索経過: nodes={} best={} hits={} depth={}",
                    self.node_count,
                    self.max_count,
                    self.results,
                    self.key.len()
                );
                checkpoint_due = true;
            }

            if count < self.max_count {
                self.key.pop();
                continue;
            }

            if level + 1 >= depth {
                let key = self.key.clone();
                self.aggregate_leaf_result(count, &key);
                self.key.pop();
                continue;
            }

            stack.push(Frame {
                level: level + 1,
                next_idx: self.primes[level + 1],
            });
        }
        info!("探索完了 (nodes={})", self.node_count);
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
        self.write_checkpoint_file(
            path,
            &Checkpoint {
                mode: SearchMode::Sequential,
                depth,
                primes: self.primes.clone(),
                cols: self.zero_mask.size(),
                stack: stack.to_vec(),
                key: self.key.clone(),
                max_count: self.max_count,
                results: self.results,
                shifts: self.shifts.clone(),
                node_count: self.node_count,
                split_depth: 0,
                completed: Vec::new(),
                in_progress: Vec::new(),
            },
        )
    }

    fn write_checkpoint_file(
        &self,
        path: &Path,
        checkpoint: &Checkpoint,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let temporary_path = path.with_extension("tmp");
        let file = fs::File::create(&temporary_path)?;
        serde_json::to_writer_pretty(file, checkpoint)?;
        if path.exists() {
            let backup_path = path.with_extension("bak");
            if backup_path.exists() {
                fs::remove_file(&backup_path)?;
            }
            fs::rename(path, backup_path)?;
        }
        fs::rename(temporary_path, path)?;
        Ok(())
    }

    fn load_checkpoint(path: &Path) -> Result<Checkpoint, Box<dyn std::error::Error>> {
        Ok(serde_json::from_reader(std::fs::File::open(path)?)?)
    }

    fn apply_checkpoint_config(
        &self,
        checkpoint: &Checkpoint,
        depth: usize,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if checkpoint.depth != depth
            || checkpoint.primes != self.primes
            || checkpoint.cols != self.zero_mask.size()
        {
            return Err(format!(
                "チェックポイントの探索設定が現在の設定と一致しません (depth={}, cols={})",
                checkpoint.depth, checkpoint.cols
            )
            .into());
        }
        Ok(())
    }

    fn restore_stack(
        &mut self,
        saved_stack: &[Frame],
    ) -> Result<Vec<Frame>, Box<dyn std::error::Error>> {
        let mut stack = Vec::new();

        for frame in saved_stack {
            if frame.level >= self.primes.len() {
                return Err("Invalid stack frame level".into());
            }
            stack.push(Frame { ..*frame });
        }

        Ok(stack)
    }

    fn rebuild_masks(&self, depth: usize, key: &[usize]) -> Vec<BitMask> {
        let mut masks = vec![self.zero_mask.clone(); depth + 1];
        for (level, &shift_idx) in key.iter().enumerate() {
            let (base_masks, node_masks) = masks.split_at_mut(level + 1);
            node_masks[0]
                .bitand_into_count(&base_masks[level], &self.shift_table[level][shift_idx]);
        }
        masks
    }

    pub fn search_parallel(
        &self,
        depth: usize,
        checkpoint_path: Option<&Path>,
        resume_path: Option<&Path>,
    ) -> Result<SharedResults, Box<dyn std::error::Error>> {
        let results = Arc::new(ParallelResults {
            max_count: AtomicUsize::new(0),
            results: Mutex::new(SharedResults::default()),
        });
        let node_count = Arc::new(AtomicU64::new(0));
        let progress = Mutex::new(ParallelProgress {
            completed: HashSet::new(),
            in_progress: HashMap::new(),
        });

        let mut split_depth = self.parallel_split_depth(depth);
        if let Some(path) = resume_path {
            let checkpoint = Self::load_checkpoint(path)?;
            if checkpoint.mode != SearchMode::Parallel {
                return Err(
                    "逐次モードのチェックポイントは --mode sequential でのみ再開できます".into(),
                );
            }
            self.apply_checkpoint_config(&checkpoint, depth)?;
            if checkpoint.split_depth > depth {
                return Err("チェックポイントの split_depth が depth を超えています".into());
            }
            split_depth = checkpoint.split_depth;
            node_count.store(checkpoint.node_count, Ordering::Relaxed);
            results
                .max_count
                .store(checkpoint.max_count, Ordering::Relaxed);
            *results.results.lock().unwrap() = SharedResults {
                max_count: checkpoint.max_count,
                results: checkpoint.results,
                shifts: checkpoint.shifts.clone(),
            };
            {
                let mut progress = progress.lock().unwrap();
                progress.completed = checkpoint.completed.iter().copied().collect();
                progress.in_progress = checkpoint
                    .in_progress
                    .iter()
                    .cloned()
                    .map(|item| (item.work_index, item))
                    .collect();
            }
            info!(
                "チェックポイントから並列探索を再開しました (nodes={})",
                checkpoint.node_count
            );
        }

        let work_items = self.parallel_work_items(split_depth);
        let jobs = {
            let progress = progress.lock().unwrap();
            self.parallel_jobs(&work_items, &progress)?
        };

        let ctx = ParallelSearchCtx {
            depth,
            split_depth,
            results: results.as_ref(),
            node_count: node_count.as_ref(),
            progress: &progress,
            checkpoint_path,
            checkpoint_lock: Mutex::new(()),
            last_checkpoint_nodes: AtomicU64::new(node_count.load(Ordering::Relaxed)),
        };

        jobs.into_par_iter()
            .try_for_each(|job| self.run_parallel_job(job, &ctx))?;

        info!(
            "並列探索完了 (nodes={})",
            node_count.load(Ordering::Relaxed)
        );
        if ctx.checkpoint_path.is_some() {
            self.write_parallel_checkpoint(&ctx, true, false)?;
        }
        Ok(results.snapshot())
    }

    fn parallel_jobs(
        &self,
        work_items: &[WorkItem],
        progress: &ParallelProgress,
    ) -> Result<Vec<ParallelJob>, Box<dyn std::error::Error>> {
        let mut jobs = Vec::new();
        for (index, item) in progress.in_progress.iter() {
            if *index >= work_items.len() {
                return Err("チェックポイントの work_index が範囲外です".into());
            }
            if progress.completed.contains(index) {
                return Err(
                    "チェックポイントで完了済みと実行中の work_index が重複しています".into(),
                );
            }
            jobs.push(ParallelJob {
                index: *index,
                work_item: work_items[*index].clone(),
                resume: Some((item.stack.clone(), item.key.clone())),
            });
        }
        for (index, work_item) in work_items.iter().enumerate() {
            if progress.completed.contains(&index) || progress.in_progress.contains_key(&index) {
                continue;
            }
            jobs.push(ParallelJob {
                index,
                work_item: work_item.clone(),
                resume: None,
            });
        }
        Ok(jobs)
    }

    fn run_parallel_job(
        &self,
        job: ParallelJob,
        ctx: &ParallelSearchCtx<'_>,
    ) -> Result<(), String> {
        let ParallelJob {
            index,
            work_item,
            resume,
        } = job;
        let depth = ctx.depth;

        let (mut key, mut masks, mut stack) = if let Some((saved_stack, saved_key)) = resume {
            let masks = self.rebuild_masks(depth, &saved_key);
            (saved_key, masks, saved_stack)
        } else {
            let mut masks = vec![self.zero_mask.clone(); depth + 1];
            masks[ctx.split_depth] = work_item.base_mask;
            let key = work_item.key;
            if ctx.split_depth == depth {
                ctx.results
                    .record_best(masks[ctx.split_depth].count_ones(), &key);
                self.finish_parallel_job(index, ctx)?;
                return Ok(());
            }
            let stack = vec![Frame {
                level: ctx.split_depth,
                next_idx: self.primes[ctx.split_depth],
            }];
            (key, masks, stack)
        };

        if ctx.split_depth == depth {
            self.finish_parallel_job(index, ctx)?;
            return Ok(());
        }

        self.store_parallel_progress(index, &stack, &key, ctx);
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
            let idx = frame.next_idx;
            let level = frame.level;
            key.push(idx);
            local_nodes += 1;
            let (base_masks, node_masks) = masks.split_at_mut(level + 1);
            let c_count = node_masks[0]
                .bitand_into_count(&base_masks[level], &self.shift_table[level][idx]);

            if local_nodes == self.checkpoint_interval {
                let n = ctx.node_count.fetch_add(local_nodes, Ordering::Relaxed) + local_nodes;
                local_nodes = 0;
                let shared = ctx.results.snapshot();
                info!(
                    "探索経過: nodes={} best={} hits={} depth={}",
                    n,
                    shared.max_count,
                    shared.results,
                    key.len()
                );
                self.store_parallel_progress(index, &stack, &key, ctx);
                self.write_parallel_checkpoint(ctx, false, true)?;
            }

            if c_count < ctx.results.max_count.load(Ordering::Relaxed) {
                key.pop();
                continue;
            }

            if level + 1 >= depth {
                ctx.results.record_best(c_count, &key);
                key.pop();
                continue;
            }

            stack.push(Frame {
                level: level + 1,
                next_idx: self.primes[level + 1],
            });
        }

        ctx.node_count.fetch_add(local_nodes, Ordering::Relaxed);
        self.finish_parallel_job(index, ctx)
    }

    fn store_parallel_progress(
        &self,
        work_index: usize,
        stack: &[Frame],
        key: &[usize],
        ctx: &ParallelSearchCtx<'_>,
    ) {
        ctx.progress.lock().unwrap().in_progress.insert(
            work_index,
            ParallelInProgress {
                work_index,
                stack: stack.to_vec(),
                key: key.to_vec(),
            },
        );
    }

    fn finish_parallel_job(
        &self,
        work_index: usize,
        ctx: &ParallelSearchCtx<'_>,
    ) -> Result<(), String> {
        {
            let mut progress = ctx.progress.lock().unwrap();
            progress.in_progress.remove(&work_index);
            progress.completed.insert(work_index);
        }
        self.write_parallel_checkpoint(ctx, false, false)
    }

    fn write_parallel_checkpoint(
        &self,
        ctx: &ParallelSearchCtx<'_>,
        force_lock: bool,
        require_interval: bool,
    ) -> Result<(), String> {
        let Some(path) = ctx.checkpoint_path else {
            return Ok(());
        };
        let n = ctx.node_count.load(Ordering::Relaxed);
        let last = ctx.last_checkpoint_nodes.load(Ordering::Relaxed);
        if require_interval && n.saturating_sub(last) < self.checkpoint_interval {
            return Ok(());
        }
        let _guard = if force_lock {
            ctx.checkpoint_lock.lock().unwrap()
        } else {
            match ctx.checkpoint_lock.try_lock() {
                Ok(guard) => guard,
                Err(_) => return Ok(()),
            }
        };
        let n = ctx.node_count.load(Ordering::Relaxed);
        let last = ctx.last_checkpoint_nodes.load(Ordering::Relaxed);
        if require_interval && n.saturating_sub(last) < self.checkpoint_interval {
            return Ok(());
        }

        let (completed, in_progress) = {
            let progress = ctx.progress.lock().unwrap();
            let mut completed: Vec<_> = progress.completed.iter().copied().collect();
            completed.sort_unstable();
            let mut in_progress: Vec<_> = progress.in_progress.values().cloned().collect();
            in_progress.sort_by_key(|item| item.work_index);
            (completed, in_progress)
        };
        let shared = ctx.results.snapshot();
        self.write_checkpoint_file(
            path,
            &Checkpoint {
                mode: SearchMode::Parallel,
                depth: ctx.depth,
                primes: self.primes.clone(),
                cols: self.zero_mask.size(),
                stack: Vec::new(),
                key: Vec::new(),
                max_count: shared.max_count,
                results: shared.results,
                shifts: shared.shifts,
                node_count: n,
                split_depth: ctx.split_depth,
                completed,
                in_progress,
            },
        )
        .map_err(|err| err.to_string())?;
        ctx.last_checkpoint_nodes.store(n, Ordering::Relaxed);
        Ok(())
    }

    fn parallel_split_depth(&self, depth: usize) -> usize {
        let target_tasks = rayon::current_num_threads() * 4;
        let mut task_count: usize = 1;
        for level in 0..depth {
            task_count = task_count.saturating_mul(self.primes[level]);
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
            let mut next_items = Vec::with_capacity(work_items.len() * self.primes[level]);
            for item in work_items {
                for shift in (0..self.primes[level]).rev() {
                    let mut base_mask = self.zero_mask.clone();
                    let _ = base_mask
                        .bitand_into_count(&item.base_mask, &self.shift_table[level][shift]);
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

#[cfg(test)]
mod tests {
    use super::{build_shift_table, State};

    #[test]
    fn build_shift_table_creates_expected_complement_masks() {
        let table = build_shift_table(&[2], 6);
        assert_eq!(table.len(), 1);
        assert_eq!(table[0].len(), 2);
        assert_eq!(table[0][0].count_ones(), 3);
        assert_eq!(table[0][1].count_ones(), 3);
    }

    #[test]
    fn sequential_and_parallel_search_find_valid_results() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.search_with_checkpoint(2, None, None).unwrap();
        let parallel = State::new(primes.clone(), cols, table);
        let result = parallel.search_parallel(2, None, None).unwrap();

        assert!(sequential.results > 0);
        assert_eq!(result.max_count, sequential.max_count);
        assert_eq!(result.results, sequential.results);
        assert_eq!(result.shifts.len(), result.results);
        for shifts in &result.shifts {
            assert_eq!(shifts.len(), 2);
            for (level, &shift) in shifts.iter().enumerate() {
                assert!(shift < primes[level]);
            }
        }
    }

    #[test]
    fn sequential_and_parallel_search_record_all_best_leaves() {
        let primes = vec![2];
        let cols = 4;
        let table = build_shift_table(&primes, cols);

        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.search_with_checkpoint(1, None, None).unwrap();

        let parallel = State::new(primes, cols, table);
        let result = parallel.search_parallel(1, None, None).unwrap();

        assert_eq!(sequential.max_count, 2);
        assert_eq!(sequential.results, 2);
        assert_eq!(sequential.shifts, vec![vec![1], vec![0]]);
        assert_eq!(result.max_count, 2);
        assert_eq!(result.results, 2);
        assert_eq!(result.shifts.len(), 2);
    }

    #[test]
    fn checkpoint_interval_defaults_to_100k() {
        let table = build_shift_table(&[2], 4);
        let state = State::new(vec![2], 4, table);
        assert_eq!(state.checkpoint_interval, 100_000);
    }

    #[test]
    fn checkpoint_interval_can_be_set() {
        let table = build_shift_table(&[2], 4);
        let mut state = State::new(vec![2], 4, table);
        state.checkpoint_interval = 5_000;
        assert_eq!(state.checkpoint_interval, 5_000);
    }

    #[test]
    fn restore_stack_recovers_search_position() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut state = State::new(primes.clone(), cols, table);

        state.key = vec![1, 0];
        state.node_count = 10;
        state.max_count = 2;
        state.results = 0;

        let saved_stack = vec![super::Frame {
            level: 1,
            next_idx: 2,
        }];

        let rebuilt_stack = state.restore_stack(&saved_stack).unwrap();

        assert_eq!(rebuilt_stack.len(), 1);
        assert_eq!(rebuilt_stack[0].level, 1);
        assert_eq!(rebuilt_stack[0].next_idx, 2);
        assert_eq!(rebuilt_stack[0].level, 1);
    }

    #[test]
    fn stack_frame_serialization_is_lightweight() {
        use serde_json;
        let frame = super::Frame {
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
            mode: super::SearchMode::Sequential,
            depth: 2,
            primes: vec![2, 3],
            cols: 4,
            stack: vec![super::Frame {
                level: 0,
                next_idx: 1,
            }],
            key: vec![1],
            max_count: 2,
            results: 0,
            shifts: vec![],
            node_count: 100,
            split_depth: 0,
            completed: vec![],
            in_progress: vec![],
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
    fn checkpoint_write_and_resume_round_trip() {
        let primes = vec![2];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let path = std::env::temp_dir().join(format!(
            "hlsearch-checkpoint-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let saved = State::new(primes.clone(), cols, table.clone());
        let stack = vec![super::Frame {
            level: 0,
            next_idx: 2,
        }];
        saved.write_checkpoint(&path, 1, &stack).unwrap();

        let mut resumed = State::new(primes, cols, table);
        resumed
            .search_with_checkpoint(1, Some(&path), Some(&path))
            .unwrap();

        let backup_path = path.with_extension("bak");
        assert!(backup_path.exists());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(backup_path).unwrap();
    }

    #[test]
    fn multiple_keys_rebuild_to_correct_masks() {
        let primes = vec![2, 3, 5];
        let cols = 8;
        let table = build_shift_table(&primes, cols);
        let mut state = State::new(primes.clone(), cols, table);

        state.key = vec![1, 2, 1];

        let saved_stack = vec![super::Frame {
            level: 2,
            next_idx: 3,
        }];

        let rebuilt = state.restore_stack(&saved_stack).unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].level, 2);

        let masks = state.rebuild_masks(3, &state.key);
        assert_eq!(masks[3].count_ones(), 2);
    }

    fn unique_checkpoint_path(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hlsearch-{label}-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn parallel_checkpoint_write_and_resume_round_trip() {
        let primes = vec![2, 3, 5];
        let cols = 8;
        let depth = 3;
        let table = build_shift_table(&primes, cols);
        let path = unique_checkpoint_path("parallel-checkpoint");

        let mut state = State::new(primes.clone(), cols, table.clone());
        state.checkpoint_interval = 1;
        let expected = state
            .search_parallel(depth, Some(&path), None)
            .unwrap();

        let resumed = State::new(primes, cols, table);
        let result = resumed
            .search_parallel(depth, Some(&path), Some(&path))
            .unwrap();

        assert_eq!(result.max_count, expected.max_count);
        assert_eq!(result.results, expected.results);
        assert_eq!(result.shifts.len(), expected.results);

        let backup_path = path.with_extension("bak");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(backup_path);
    }

    #[test]
    fn parallel_resume_skips_completed_work_items() {
        let primes = vec![2, 3, 5];
        let cols = 8;
        let depth = 3;
        let table = build_shift_table(&primes, cols);
        let state = State::new(primes.clone(), cols, table.clone());
        let split_depth = state.parallel_split_depth(depth);
        let work_items = state.parallel_work_items(split_depth);
        assert!(!work_items.is_empty());

        let expected = State::new(primes.clone(), cols, table.clone())
            .search_parallel(depth, None, None)
            .unwrap();

        let path = unique_checkpoint_path("parallel-completed");
        let completed: Vec<usize> = (0..work_items.len()).collect();
        state
            .write_checkpoint_file(
                &path,
                &super::Checkpoint {
                    mode: super::SearchMode::Parallel,
                    depth,
                    primes: primes.clone(),
                    cols,
                    stack: Vec::new(),
                    key: Vec::new(),
                    max_count: expected.max_count,
                    results: expected.results,
                    shifts: expected.shifts.clone(),
                    node_count: 42,
                    split_depth,
                    completed,
                    in_progress: Vec::new(),
                },
            )
            .unwrap();

        let resumed = State::new(primes, cols, table);
        let result = resumed
            .search_parallel(depth, Some(&path), Some(&path))
            .unwrap();
        assert_eq!(result.max_count, expected.max_count);
        assert_eq!(result.results, expected.results);
        assert_eq!(result.shifts, expected.shifts);

        let backup_path = path.with_extension("bak");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(backup_path);
    }

    #[test]
    fn parallel_search_rejects_sequential_checkpoint() {
        let primes = vec![2];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let path = unique_checkpoint_path("sequential-for-parallel");
        let saved = State::new(primes.clone(), cols, table.clone());
        saved
            .write_checkpoint(
                &path,
                1,
                &[super::Frame {
                    level: 0,
                    next_idx: 2,
                }],
            )
            .unwrap();

        let resumed = State::new(primes, cols, table);
        let err = resumed
            .search_parallel(1, None, Some(&path))
            .unwrap_err()
            .to_string();
        assert!(err.contains("sequential"));

        let backup_path = path.with_extension("bak");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(backup_path);
    }

    #[test]
    fn sequential_search_rejects_parallel_checkpoint() {
        let primes = vec![2, 3];
        let cols = 4;
        let depth = 2;
        let table = build_shift_table(&primes, cols);
        let path = unique_checkpoint_path("parallel-for-sequential");
        let state = State::new(primes.clone(), cols, table.clone());
        state
            .write_checkpoint_file(
                &path,
                &super::Checkpoint {
                    mode: super::SearchMode::Parallel,
                    depth,
                    primes: primes.clone(),
                    cols,
                    stack: Vec::new(),
                    key: Vec::new(),
                    max_count: 0,
                    results: 0,
                    shifts: Vec::new(),
                    node_count: 0,
                    split_depth: 1,
                    completed: Vec::new(),
                    in_progress: Vec::new(),
                },
            )
            .unwrap();

        let mut resumed = State::new(primes, cols, table);
        let err = resumed
            .search_with_checkpoint(depth, None, Some(&path))
            .unwrap_err()
            .to_string();
        assert!(err.contains("parallel"));

        let _ = std::fs::remove_file(&path);
    }
}