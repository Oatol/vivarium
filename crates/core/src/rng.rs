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

    /// 内部状态 `(state, inc)`。
    ///
    /// 存档要把发生器的**当前**位置写进去，而不是种子：读档之后继续跑，
    /// 必须和"从没存过档"逐 tick 一致。只存种子会回到序列开头，
    /// 于是同一段随机数被用第二次——那种偏差在画面上完全看不出来，
    /// 只会让"读档后的世界和存档时不一样"变成一件说不清的事。
    #[inline]
    pub const fn state(&self) -> (u64, u64) {
        (self.state, self.inc)
    }

    /// 从 [`Rng::state`] 的值恢复。
    ///
    /// `inc` 必须是奇数（PCG 的要求：偶数会让状态永远停在偶数侧）。
    /// 传进来的值不合法时退回 `inc = 1`，而不是断言——
    /// 这是**读档**路径，一个坏文件不该让进程 panic。
    #[inline]
    pub fn set_state(&mut self, state: u64, inc: u64) {
        self.inc = if inc % 2 == 1 { inc } else { 1 };
        self.state = state;
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

    // -----------------------------------------------------------------------
    // 位置派生随机数
    // -----------------------------------------------------------------------

    /// 位置派生的伪随机数：只依赖 `(x, y, tick, salt)`，**与处理顺序无关**。
    ///
    /// ## 为什么不能用上面那个顺序推进的 RNG
    ///
    /// 材质层每 tick 要为成千上万个格子做随机决策（往左还是往右滑、
    /// 这一格点不点得着）。如果这些决策来自一个共享的、顺序推进的 RNG，
    /// 那么"哪些格子先被处理"就会改变结果——于是**并行化不再是性能优化，
    /// 而是一次无法验证的赌博**，跳过分块扫描也一样。
    ///
    /// 换成位置派生的随机数之后，每格的决策只由它的坐标和 tick 决定：
    ///
    /// - 单线程扫描与棋盘式并行扫描必须给出**逐格相同**的结果，可以直接断言
    /// - 跳过静止分块与全网格扫描必须给出**逐格相同**的结果，可以直接断言
    /// - 不需要为每格维护 RNG 状态，也不需要任何同步
    ///
    /// ## `salt` 是干什么的
    ///
    /// 同一格在同一 tick 里往往要做好几个独立决策（往哪边滑、斜不斜、
    /// 点不点得着）。不加区分的话它们会取到同一个数，行为立刻退化成强相关——
    /// 最典型的表现是"凡是往右滑的格子必然同时点着了火"。
    ///
    /// 用不同的 `salt` 把它们拆开。
    #[inline]
    pub fn at(x: usize, y: usize, tick: u64, salt: u32) -> u32 {
        // 三个维度各自乘一个大奇数再**相加**。用加法而不是异或：
        // 异或下 (x, y) 与 (y, x) 会得到同一个种子，整个世界沿对角线对称。
        let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h = h.wrapping_add((y as u64).wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
        h = h.wrapping_add(tick.wrapping_mul(0x1656_67B1_9E37_79F9));
        h = h.wrapping_add((salt as u64).wrapping_mul(0x27D4_EB2F_1656_67C5));
        // splitmix64 的收尾混合：雪崩性好，相邻输入给出完全无关的输出。
        h ^= h >> 30;
        h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= h >> 27;
        h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 31;
        h as u32
    }

    /// 位置派生的 `[0, n)` 整数。`n == 0` 时返回 0。
    #[inline]
    pub fn at_below(x: usize, y: usize, tick: u64, salt: u32, n: u32) -> u32 {
        if n == 0 {
            0
        } else {
            Self::at(x, y, tick, salt) % n
        }
    }

    /// 位置派生的百分比判定：以 `percent` 的概率返回 `true`。
    #[inline]
    pub fn at_percent(x: usize, y: usize, tick: u64, salt: u32, percent: u32) -> bool {
        percent > 0 && Self::at_below(x, y, tick, salt, 100) < percent
    }

    /// 位置派生的左右偏好：`-1` 或 `+1`。
    #[inline]
    pub fn at_sign(x: usize, y: usize, tick: u64, salt: u32) -> isize {
        if Self::at(x, y, tick, salt) & 1 == 0 {
            -1
        } else {
            1
        }
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

    // -----------------------------------------------------------------------
    // 位置派生随机数
    // -----------------------------------------------------------------------

    /// 采样区域：128×128 = 16384 格，足够让统计断言稳定，又不至于拖慢测试。
    const SAMPLE_W: usize = 128;
    const SAMPLE_H: usize = 128;
    const SAMPLE_N: usize = SAMPLE_W * SAMPLE_H;

    fn for_each_sample<F: FnMut(usize, usize)>(mut f: F) {
        for y in 0..SAMPLE_H {
            for x in 0..SAMPLE_W {
                f(x, y);
            }
        }
    }

    #[test]
    fn at_is_deterministic() {
        for (x, y) in [(0, 0), (1, 0), (0, 1), (63, 63), (127, 127)] {
            assert_eq!(Rng::at(x, y, 7, 3), Rng::at(x, y, 7, 3));
        }
    }

    /// 四个维度里任何一个变了，输出都该变。
    ///
    /// 这条不是形式主义——如果 `salt` 没进哈希，材质层所有独立决策就会退化成同一个，
    /// 表现为"往右滑的格子必然同时被点着"。测试要能在那种情况下红。
    #[test]
    fn at_varies_with_every_dimension() {
        let base = Rng::at(10, 20, 30, 40);
        assert_ne!(base, Rng::at(11, 20, 30, 40), "x ignored");
        assert_ne!(base, Rng::at(10, 21, 30, 40), "y ignored");
        assert_ne!(base, Rng::at(10, 20, 31, 40), "tick ignored");
        assert_ne!(base, Rng::at(10, 20, 30, 41), "salt ignored");
    }

    /// 位置派生随机数在网格上必须真的像噪声，而不是某个低频函数。
    ///
    /// 一个手滑写成 `x * y` 或漏掉收尾混合的实现很容易通过前面几条测试，
    /// 但会在这里露馅：输出沿行/列呈明显梯度，桶计数严重偏离均匀。
    #[test]
    fn at_is_uniform_over_the_grid() {
        const BUCKETS: usize = 8;
        let mut hist = [0usize; BUCKETS];
        for_each_sample(|x, y| {
            // 取高 3 位分桶，低位在取模运算里更容易有偏差。
            hist[(Rng::at(x, y, 0, 0) >> 29) as usize] += 1;
        });
        let expected = SAMPLE_N as f64 / BUCKETS as f64;
        for (i, &n) in hist.iter().enumerate() {
            let dev = (n as f64 - expected).abs() / expected;
            assert!(
                dev < 0.1,
                "bucket {i}: {n} vs expected {expected:.0} ({:.1}% off)",
                dev * 100.0
            );
        }
    }

    /// 相邻格之间不能有可见相关性——这是"像噪声"的另一种说法。
    ///
    /// 取每一格与它右邻居的最低位，两者应当约一半时候相同。
    #[test]
    fn at_adjacent_cells_are_uncorrelated() {
        let mut same = 0usize;
        let mut total = 0usize;
        for y in 0..SAMPLE_H {
            for x in 0..SAMPLE_W - 1 {
                same += usize::from((Rng::at(x, y, 0, 0) ^ Rng::at(x + 1, y, 0, 0)) & 1 == 0);
                total += 1;
            }
        }
        let rate = same as f64 / total as f64;
        assert!(
            (0.47..0.53).contains(&rate),
            "adjacent bit agreement {rate:.3}"
        );
    }

    /// `(x, y)` 与 `(y, x)` 必须给出不同结果。
    ///
    /// 如果三个维度用异或而不是相加混合，整张世界会沿对角线镜像对称——
    /// 在地貌生成里表现为"岛的分布沿对角线重复"，一眼能看出来但很难定位。
    #[test]
    fn at_is_not_symmetric_under_xy_swap() {
        let mut collisions = 0usize;
        for_each_sample(|x, y| {
            if x != y && Rng::at(x, y, 0, 0) == Rng::at(y, x, 0, 0) {
                collisions += 1;
            }
        });
        // 真正的 32 位哈希在 1.6 万次里撞一两次属正常，成规模就是实现错了。
        assert!(collisions <= 4, "{collisions} diagonal collisions");
    }

    /// 不同 `salt` 之间也要互不相关。
    #[test]
    fn at_salts_are_uncorrelated() {
        let mut same = 0usize;
        for_each_sample(|x, y| {
            same += usize::from((Rng::at(x, y, 0, 0) ^ Rng::at(x, y, 0, 1)) & 1 == 0);
        });
        let rate = same as f64 / SAMPLE_N as f64;
        assert!((0.47..0.53).contains(&rate), "salt bit agreement {rate:.3}");
    }

    /// 不同 tick 之间也要互不相关。
    #[test]
    fn at_ticks_are_uncorrelated() {
        let mut same = 0usize;
        for_each_sample(|x, y| {
            same += usize::from((Rng::at(x, y, 0, 0) ^ Rng::at(x, y, 1, 0)) & 1 == 0);
        });
        let rate = same as f64 / SAMPLE_N as f64;
        assert!((0.47..0.53).contains(&rate), "tick bit agreement {rate:.3}");
    }

    #[test]
    fn at_below_respects_bound() {
        for n in [1u32, 2, 3, 7, 100, 1000] {
            for_each_sample(|x, y| {
                let v = Rng::at_below(x, y, 5, 9, n);
                assert!(v < n, "at_below gave {v} for bound {n}");
            });
        }
        assert_eq!(Rng::at_below(3, 4, 5, 6, 0), 0, "zero bound must not panic");
    }

    #[test]
    fn at_below_hits_every_value() {
        // 小范围取值必须铺满，否则"随机挑一边滑"会永远只挑同一边。
        let mut seen = [false; 4];
        for_each_sample(|x, y| {
            seen[Rng::at_below(x, y, 0, 0, 4) as usize] = true;
        });
        assert!(
            seen.iter().all(|&s| s),
            "at_below(4) skipped a value: {seen:?}"
        );
    }

    #[test]
    fn at_percent_matches_requested_rate() {
        for percent in [5u32, 25, 50, 90] {
            let mut hits = 0usize;
            for_each_sample(|x, y| {
                hits += usize::from(Rng::at_percent(x, y, 0, 0, percent));
            });
            let rate = hits as f64 / SAMPLE_N as f64;
            let want = percent as f64 / 100.0;
            assert!(
                (rate - want).abs() < 0.02,
                "percent {percent}: got {rate:.3}, want ~{want:.3}"
            );
        }
    }

    #[test]
    fn at_percent_edges_are_absolute() {
        for_each_sample(|x, y| {
            assert!(!Rng::at_percent(x, y, 0, 0, 0), "0% must never fire");
            assert!(Rng::at_percent(x, y, 0, 0, 100), "100% must always fire");
        });
    }

    #[test]
    fn at_sign_is_balanced_and_only_returns_pm_one() {
        let mut left = 0usize;
        for_each_sample(|x, y| match Rng::at_sign(x, y, 0, 0) {
            -1 => left += 1,
            1 => {}
            other => panic!("at_sign returned {other}"),
        });
        let rate = left as f64 / SAMPLE_N as f64;
        assert!((0.47..0.53).contains(&rate), "left rate {rate:.3}");
    }

    /// 同一格在同一 tick 的多个决策，用不同 salt 拆开后必须真的独立。
    ///
    /// 具体到材质层：`salt=SLIDE_DIR` 决定往哪边滑，`salt=IGNITE` 决定点不点得着。
    /// 若两者相关，世界会出现"向右滑的沙子同时烧起来"这种一眼假的现象。
    #[test]
    fn different_decisions_on_one_cell_are_independent() {
        let mut both = 0usize; // 往右滑 且 点着
        let mut right = 0usize; // 往右滑
        let mut lit = 0usize; // 点着
        for_each_sample(|x, y| {
            let r = Rng::at_sign(x, y, 0, 0) > 0;
            let l = Rng::at_percent(x, y, 0, 1, 50);
            both += usize::from(r && l);
            right += usize::from(r);
            lit += usize::from(l);
        });
        let p_r = right as f64 / SAMPLE_N as f64;
        let p_l = lit as f64 / SAMPLE_N as f64;
        let p_both = both as f64 / SAMPLE_N as f64;
        let expected = p_r * p_l;
        assert!(
            (p_both - expected).abs() < 0.02,
            "decisions correlate: P(both)={p_both:.3}, P(right)*P(lit)={expected:.3}"
        );
    }
}
