//! 确定性伪随机数发生器。
//!
//! SPEC 要求固定种子、禁用系统随机，以保证可复现、可回放、可分享种子。
//! 这里手写一个 PCG-XSH-RR 变体：零依赖，且**永远不会因为第三方库升级而改变输出序列**。
//!
//! 最后一点比看起来重要——如果 PRNG 的实现在版本间变了，你三个月后重跑同一个种子
//! 会得到不同的世界，所有历史存档和实验记录都会失效。

/// PCG-XSH-RR 32 位输出发生器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng {
    state: u64,
    inc: u64,
}

const MULT: u64 = 6_364_136_223_846_793_005;

impl Rng {
    /// 用给定种子创建发生器。同一种子永远产生同一序列。
    pub fn new(seed: u64) -> Self {
        let mut rng = Self {
            state: 0,
            inc: (seed << 1) | 1,
        };
        // 标准的 PCG 初始化流程：先空转两次，让 state 充分混合。
        rng.next_u32();
        rng.state = rng.state.wrapping_add(seed);
        rng.next_u32();
        rng
    }

    /// 下一个 32 位随机数。
    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(MULT).wrapping_add(self.inc);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        xorshifted.rotate_right(rot)
    }

    /// 下一个 64 位随机数（由两次 32 位拼接）。
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let hi = self.next_u32() as u64;
        let lo = self.next_u32() as u64;
        (hi << 32) | lo
    }

    /// `[0, 1)` 区间的 `f32`。
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        // 取高 24 位，正好是 f32 的尾数精度，避免低位偏差。
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }

    /// `[lo, hi)` 区间的整数。`hi <= lo` 时返回 `lo`。
    #[inline]
    pub fn gen_range_u32(&mut self, lo: u32, hi: u32) -> u32 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u32() % (hi - lo)
    }

    /// `[lo, hi)` 区间的 `usize`。`hi <= lo` 时返回 `lo`。
    #[inline]
    pub fn gen_range_usize(&mut self, lo: usize, hi: usize) -> usize {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u64() % (hi - lo) as u64) as usize
    }

    /// 以给定概率返回 `true`（`p` 会被夹到 `[0, 1]`）。
    #[inline]
    pub fn chance(&mut self, p: f32) -> bool {
        self.next_f32() < p
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u32(), b.next_u32());
        }
    }

    #[test]
    fn different_seed_different_sequence() {
        let mut a = Rng::new(1);
        let mut b = Rng::new(2);
        let sa: Vec<u32> = (0..64).map(|_| a.next_u32()).collect();
        let sb: Vec<u32> = (0..64).map(|_| b.next_u32()).collect();
        assert_ne!(sa, sb);
    }

    #[test]
    fn range_is_respected() {
        let mut rng = Rng::new(7);
        for _ in 0..10_000 {
            let v = rng.gen_range_u32(10, 20);
            assert!((10..20).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn f32_in_unit_interval() {
        let mut rng = Rng::new(99);
        for _ in 0..10_000 {
            let v = rng.next_f32();
            assert!((0.0..1.0).contains(&v), "out of range: {v}");
        }
    }
}
