use crate::bitmask::BitMask;
use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
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
    fn record_best(&self, count: usize, key: &[usize]) {
        loop {
            let current = self.max_count.load(Ordering::Relaxed);
            if count < current {
                return;
            }
            if count == current {
                let mut results = self.results.lock().unwrap();
                if self.max_count.load(Ordering::Relaxed) == count {
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

pub struct State {
    pub primes: Vec<usize>,
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
    shift_table: Vec<Vec<BitMask>>,
}

impl State {
    pub fn new(primes: Vec<usize>, cols: usize, shift_table: Vec<Vec<BitMask>>) -> Self {
        State {
            primes,
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
            shift_table,
        }
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
                next_idx: self.primes[0],
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
            let i = frame.next_idx;
            let level = frame.level;
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

            if count < self.max_count && count < self.target {
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
                next_idx: self.primes[level + 1],
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

        let split_depth = self.parallel_split_depth(depth);
        let work_items = self.parallel_work_items(split_depth);
        work_items.into_par_iter().for_each(|work_item| {
            let mut key = work_item.key;
            let mut masks = vec![self.zero_mask.clone(); depth + 1];
            masks[split_depth] = work_item.base_mask;
            let count = masks[split_depth].count_ones();
            if split_depth == depth {
                results.record_best(count, &key);
                if depth == self.max_depth && count == self.target {
                    let mut shared = results.results.lock().unwrap();
                    shared.target_results += 1;
                    shared.target_shifts.push(key);
                }
                return;
            }

            let mut stack = vec![Frame {
                level: split_depth,
                next_idx: self.primes[split_depth],
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
                let idx = frame.next_idx;
                let level = frame.level;
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

                if c_count < results.max_count.load(Ordering::Relaxed) && c_count < self.target {
                    key.pop();
                    continue;
                }

                if level + 1 >= depth {
                    results.record_best(c_count, &key);
                    if depth == self.max_depth && c_count == self.target {
                        let mut shared = results.results.lock().unwrap();
                        shared.target_results += 1;
                        shared.target_shifts.push(key.clone());
                    }
                    key.pop();
                    continue;
                }

                stack.push(Frame {
                    level: level + 1,
                    next_idx: self.primes[level + 1],
                });
            }
            node_count.fetch_add(local_nodes, Ordering::Relaxed);
        });

        pb.finish_with_message("探索完了");
        results.snapshot()
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
    fn sequential_and_parallel_search_find_maximum_results() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.search_with_checkpoint(2, None, None).unwrap();
        let parallel = State::new(primes.clone(), cols, table);
        let result = parallel.search_parallel(2);

        assert_eq!(sequential.max_count, 2);
        assert_eq!(sequential.results, 2);
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
    fn sequential_and_parallel_search_record_all_maximum_leaves() {
        let primes = vec![2];
        let cols = 4;
        let table = build_shift_table(&primes, cols);

        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.search_with_checkpoint(1, None, None).unwrap();

        let parallel = State::new(primes, cols, table);
        let result = parallel.search_parallel(1);

        assert_eq!(sequential.max_count, 2);
        assert_eq!(sequential.results, 2);
        assert_eq!(sequential.shifts, vec![vec![1], vec![0]]);
        assert_eq!(result.max_count, 2);
        assert_eq!(result.results, 2);
        assert_eq!(result.shifts.len(), 2);
    }

    #[test]
    fn target_results_include_paths_below_the_maximum() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);

        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.max_depth = 2;
        sequential.target = 1;
        sequential.search_with_checkpoint(2, None, None).unwrap();

        let mut parallel = State::new(primes, cols, table);
        parallel.max_depth = 2;
        parallel.target = 1;
        let result = parallel.search_parallel(2);

        assert_eq!(sequential.max_count, 2);
        assert_eq!(sequential.results, 2);
        assert_eq!(sequential.target_results, 4);
        assert_eq!(sequential.target_shifts.len(), 4);
        assert_eq!(result.max_count, 2);
        assert_eq!(result.results, 2);
        assert_eq!(result.target_results, 4);
        assert_eq!(result.target_shifts.len(), 4);
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
    fn rebuild_stack_and_masks_recovers_search_position() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut state = State::new(primes.clone(), cols, table);

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
        let mut state = State::new(primes.clone(), cols, table);

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
