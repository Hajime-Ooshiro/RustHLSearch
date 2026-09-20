use crate::bitmask::BitMask;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
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

#[derive(Clone)]
struct Frame {
    level: usize,
    base_mask: BitMask,
    next_idx: usize,
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
    pub max_depth: usize,
    pub key: Vec<usize>,
    pub zero_mask: BitMask,
    pub max_count: usize,
    pub results: usize,
    pub shifts: Vec<Vec<usize>>,
    pub node_count: u64,
    shift_table: Vec<Vec<BitMask>>,
}

impl State {
    pub fn new(primes: Vec<usize>, cols: usize, shift_table: Vec<Vec<BitMask>>) -> Self {
        State {
            primes,
            max_depth: 249,
            key: Vec::new(),
            zero_mask: BitMask::new_ones(cols),
            max_count: 0,
            results: 0,
            shifts: Vec::new(),
            node_count: 0,
            shift_table,
        }
    }

    pub fn search(&mut self, depth: usize) {
        let pb = progress_bar();
        let mut stack = vec![Frame {
            level: 0,
            base_mask: self.zero_mask.clone(),
            next_idx: self.primes[0],
        }];

        while let Some(frame) = stack.last_mut() {
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
            }

            if count + (depth - level) < self.max_count {
                self.key.pop();
                continue;
            }

            if count < self.max_count {
                self.key.pop();
                continue;
            }

            if level + 1 >= depth {
                if depth == self.max_depth {
                    if count > self.max_count {
                        self.max_count = count;
                        self.shifts.clear();
                        self.shifts.push(self.key.clone());
                        self.results = 1;
                    } else if count == self.max_count {
                        self.shifts.push(self.key.clone());
                        self.results = self.shifts.len();
                    }
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

        self.results = self.shifts.len();
        pb.finish_with_message("探索完了");
    }

    pub fn search_parallel(&self, depth: usize) -> SharedResults {
        let max_count = Arc::new(AtomicUsize::new(0));
        let results = Arc::new(AtomicUsize::new(0));
        let shifts = Arc::new(Mutex::new(Vec::<Vec<usize>>::new()));
        let node_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pb = progress_bar();

        let p0 = self.primes[0];
        (0..p0).into_par_iter().rev().for_each(|i| {
            let mut key = vec![i];
            let base_mask = self.zero_mask.bitand(&self.shift_table[0][i]);
            let count = base_mask.count_ones();

            if count < max_count.load(Ordering::Relaxed) {
                return;
            }

            if depth == 1 {
                let current_best = max_count.load(Ordering::Relaxed);
                if count > current_best {
                    max_count.store(count, Ordering::Relaxed);
                    let mut lock = shifts.lock().unwrap();
                    lock.clear();
                    lock.push(key.clone());
                    results.store(1, Ordering::Relaxed);
                } else if count == current_best {
                    let mut lock = shifts.lock().unwrap();
                    lock.push(key.clone());
                    results.store(lock.len(), Ordering::Relaxed);
                }
                return;
            }

            let mut stack = vec![Frame {
                level: 1,
                base_mask,
                next_idx: self.primes[1],
            }];

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
                }

                if count + (depth - level) < max_count.load(Ordering::Relaxed) {
                    key.pop();
                    continue;
                }

                if c_count < max_count.load(Ordering::Relaxed) {
                    key.pop();
                    continue;
                }

                if level + 1 >= depth {
                    if depth == self.max_depth {
                        let current_best = max_count.load(Ordering::Relaxed);
                        if c_count > current_best {
                            max_count.store(c_count, Ordering::Relaxed);
                            let mut lock = shifts.lock().unwrap();
                            lock.clear();
                            lock.push(key.clone());
                            results.store(1, Ordering::Relaxed);
                        } else if c_count == current_best {
                            let mut lock = shifts.lock().unwrap();
                            lock.push(key.clone());
                            results.store(lock.len(), Ordering::Relaxed);
                        }
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
        let mut sequential = State::new(primes.clone(), cols, table.clone());
        sequential.max_depth = 2;
        sequential.search(2);
        let mut parallel = State::new(primes.clone(), cols, table);
        parallel.max_depth = 2;
        let result = parallel.search_parallel(2);

        assert!(sequential.max_count > 0);
        assert!(result.max_count > 0);
        assert!(result.results > 0);
        assert!(result.shifts[0].len() >= 2);
        for (level, &shift) in result.shifts[0].iter().enumerate() {
            assert!(shift < primes[level]);
        }
    }
}
