use crate::bitmask::BitMask;
use indicatif::{ProgressBar, ProgressStyle};
use log::info;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    base_mask: BitMask,
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
    limit: usize,
    cols: usize,
    stack: Vec<StackFrame>,
    key: Vec<usize>,
    max_count: usize,
    results: usize,
    shifts: Vec<Vec<usize>>,
    node_count: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum SearchMode {
    Sequential,
    Parallel,
}

#[derive(Default)]
pub struct SharedResults {
    pub max_count: usize,
    pub results: usize,
    pub shifts: Vec<Vec<usize>>,
}

pub struct State {
    pub primes: Vec<usize>,
    pub limit: usize,
    pub max_depth: usize,
    pub target: usize,
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
        limit: usize,
        cols: usize,
        shift_table: Vec<Vec<BitMask>>,
    ) -> Self {
        State {
            primes,
            limit,
            max_depth: 249,
            target: 447,
            key: Vec::new(),
            zero_mask: BitMask::new_ones(cols),
            max_count: 0,
            results: 0,
            shifts: Vec::new(),
            node_count: 0,
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
                || checkpoint.limit != self.limit
                || checkpoint.cols != self.zero_mask.size()
            {
                return Err(format!(
                    "チェックポイントの探索設定が現在の設定と一致しません (depth={}, limit={}, cols={})",
                    checkpoint.depth, checkpoint.limit, checkpoint.cols
                )
                .into());
            }
            self.key = checkpoint.key.clone();
            self.max_count = checkpoint.max_count;
            self.results = checkpoint.results;
            self.shifts = checkpoint.shifts;
            self.node_count = checkpoint.node_count;
            info!(
                "チェックポイントから探索を再開しました (nodes={})",
                checkpoint.node_count
            );

            self.rebuild_stack_and_masks(&checkpoint.stack)?
        } else {
            vec![Frame {
                level: 0,
                base_mask: self.zero_mask.clone(),
                next_idx: self.primes[0],
            }]
        };
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
                if let Some(parent) = stack.last() {
                    self.key.pop();
                    self.zero_mask = parent.base_mask.clone();
                }
                continue;
            }

            frame.next_idx -= 1;
            let i = frame.next_idx;
            let level = frame.level;
            let base_mask = frame.base_mask.clone();
            self.key.push(i);
            self.node_count += 1;

            let node_mask = base_mask.bitand(&self.shift_table[level][i]);
            let count = node_mask.count_ones();

            if self.node_count.is_multiple_of(10_000) {
                pb.set_position(self.node_count);
                pb.set_message(format!(
                    "best: {} | hits: {} | depth: {}",
                    self.max_count,
                    self.results,
                    self.key.len()
                ));
                info!(
                    "探索経過: nodes={} best={} hits={} depth={}",
                    self.node_count,
                    self.max_count,
                    self.results,
                    self.key.len()
                );
                checkpoint_due = true;
            }

            if count < self.limit {
                self.key.pop();
                continue;
            }

            if count < self.max_count {
                self.key.pop();
                continue;
            }

            if level + 1 >= depth {
                if depth == self.max_depth && count == self.target {
                    self.results += 1;
                    self.shifts.push(self.key.clone());
                    info!("target level={} key={:?} count={}", level, self.key, count);
                    self.key.pop();
                    break;
                }
                if count > self.max_count {
                    self.max_count = count;
                    self.shifts.clear();
                    self.shifts.push(self.key.clone());
                    info!("best level={} key={:?} count={}", level, self.key, count);
                } else if count == self.max_count {
                    self.shifts.push(self.key.clone());
                    info!("best level={} key={:?} count={}", level, self.key, count);
                }
                self.key.pop();
                continue;
            }

            self.zero_mask = node_mask.clone();
            stack.push(Frame {
                level: level + 1,
                base_mask: node_mask,
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
            limit: self.limit,
            cols: self.zero_mask.size(),
            stack: stack_frames,
            key: self.key.clone(),
            max_count: self.max_count,
            results: self.results,
            shifts: self.shifts.clone(),
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

        let mut masks = vec![self.zero_mask.clone()];
        for (level, &shift_idx) in self.key.iter().enumerate() {
            let new_mask = masks[level].bitand(&self.shift_table[level][shift_idx]);
            masks.push(new_mask);
        }

        for frame in saved_stack {
            let base_mask = masks
                .get(frame.level)
                .cloned()
                .ok_or("Invalid stack frame level")?;
            stack.push(Frame {
                level: frame.level,
                base_mask,
                next_idx: frame.next_idx,
            });
        }

        self.zero_mask = masks
            .last()
            .cloned()
            .unwrap_or_else(|| self.zero_mask.clone());

        Ok(stack)
    }

    pub fn search_parallel(&self, depth: usize) -> SharedResults {
        let max_count = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(AtomicUsize::new(0));
        let shifts = Arc::new(Mutex::new(Vec::<Vec<usize>>::new()));
        let node_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let pb = progress_bar();

        let p0 = self.primes[0];
        (0..p0).into_par_iter().rev().for_each(|i| {
            if stop.load(Ordering::Relaxed) {
                return;
            }

            let mut key = vec![i];
            let base_mask = self.zero_mask.bitand(&self.shift_table[0][i]);
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
                next_idx: self.primes[1],
            }];

            while let Some(frame) = stack.last_mut() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
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
                let current_base = frame.base_mask.clone();
                key.push(idx);
                let n = node_count.fetch_add(1, Ordering::Relaxed) + 1;
                let node_mask = current_base.bitand(&self.shift_table[level][idx]);
                let c_count = node_mask.count_ones();

                if n.is_multiple_of(10_000) {
                    pb.set_position(n);
                    pb.set_message(format!(
                        "best: {} | hits: {} | depth: {}",
                        max_count.load(Ordering::Relaxed),
                        results.load(Ordering::Relaxed),
                        key.len()
                    ));
                    info!(
                        "探索経過: nodes={} best={} hits={} depth={}",
                        n,
                        max_count.load(Ordering::Relaxed),
                        results.load(Ordering::Relaxed),
                        key.len()
                    );
                }

                if c_count < self.limit {
                    key.pop();
                    continue;
                }

                if c_count < max_count.load(Ordering::Relaxed) {
                    key.pop();
                    continue;
                }

                if level + 1 >= depth {
                    if depth == self.max_depth {
                        if c_count == self.target {
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
                    if c_count > max_count.load(Ordering::Relaxed) {
                        max_count.store(c_count, Ordering::Relaxed);
                        shifts.lock().unwrap().clear();
                        shifts.lock().unwrap().push(key.clone());
                        info!("best level={} key={:?} count={}", level, key, c_count);
                    } else if c_count == max_count.load(Ordering::Relaxed) {
                        results.fetch_add(1, Ordering::Relaxed);
                        shifts.lock().unwrap().push(key.clone());
                        info!("best level={} key={:?} count={}", level, key, c_count);
                    }
                    key.pop();
                    continue;
                }

                stack.push(Frame {
                    level: level + 1,
                    base_mask: node_mask,
                    next_idx: self.primes[level + 1],
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
    fn sequential_and_parallel_search_find_valid_results() {
        let primes = vec![2, 3];
        let cols = 4;
        let table = build_shift_table(&primes, cols);
        let mut sequential = State::new(primes.clone(), 1, cols, table.clone());
        sequential.max_depth = 2;
        sequential.target = 1;
        sequential.search_with_checkpoint(2, None, None).unwrap();
        let mut parallel = State::new(primes.clone(), 1, cols, table);
        parallel.max_depth = 2;
        parallel.target = 1;
        let result = parallel.search_parallel(2);

        assert_eq!(sequential.results, 1);
        assert!(result.results > 0);
        assert_eq!(result.shifts.len(), result.results);
        for shifts in &result.shifts {
            assert_eq!(shifts.len(), 2);
            for (level, &shift) in shifts.iter().enumerate() {
                assert!(shift < primes[level]);
            }
        }
    }
}
