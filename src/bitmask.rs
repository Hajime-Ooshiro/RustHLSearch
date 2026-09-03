/// BitVec による高速なビットマスク操作構造体
#[derive(Clone, Debug)]
pub struct BitMask {
    data: Vec<u64>,
    size: usize,
}

impl BitMask {
    pub fn new_ones(size: usize) -> Self {
        let num_words = size.div_ceil(64);
        let mut data = vec![u64::MAX; num_words];
        if !size.is_multiple_of(64) {
            let remainder = size % 64;
            data[num_words - 1] = (1u64 << remainder) - 1;
        }
        BitMask { data, size }
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
