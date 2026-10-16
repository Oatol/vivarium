//! 材质流动规则引擎（M1）。
//!
//! ## 这一层要解决的两个问题
//!
//! 1. **规则**——沙怎么塌、水怎么流、气怎么浮、火怎么烧。
//!    规则本身**不在这个文件里**：它们全部来自 [`crate::material`] 的
//!    属性表（`move_rule` / `density` / `flammability`）与反应表（[`react`]）。
//!    这里只负责"按表执行"，于是加一种材质不需要动这里的任何一行。
//! 2. **顺序**——哪些格子先算，会改变结果。SPEC 第 2.4 节要求自下而上扫描，
//!    并且跳过静止分块。这一层必须保证"跳块扫描"与"全网格扫描"给出
//!    **逐格相同**的结果，否则性能优化会悄悄改变世界，而你只会在
//!    某个存档复现不出来的时候才发现。
//!
//! ## 为什么每格随机数必须位置派生
//!
//! 见 [`Rng::at`] 的文档。一句话：共享的顺序 RNG 会让"处理顺序"变成
//! 世界状态的一部分，于是并行化与跳块扫描都成了无法验证的赌博。
//! 位置派生随机数把顺序从模拟里彻底剔除了出去。

use crate::material::{ignite_chance, react, MaterialKind, MoveRule, FIRE_LIFETIME};
use crate::rng::Rng;
use crate::world::{World, CHUNK_SIZE};

/// 位置派生随机数的"用途标签"。
///
/// 同一格在同一 tick 里会做多个独立决策，共用一个随机数会让它们强相关。
/// 每个用途一个 `salt`，互不干扰。
mod salt {
    /// 粉末往左还是往右滑。
    pub const POWDER_SIDE: u32 = 1;
    /// 液体往左还是往右找平。
    pub const LIQUID_SIDE: u32 = 2;
    /// 气体 / 火往左还是往右扩散。
    pub const GAS_SIDE: u32 = 3;
    /// 点燃判定。四个邻格用 `IGNITE + 0..3` 区分。
    pub const IGNITE: u32 = 16;
}

/// 四邻域偏移。
const NEIGHBORS_4: [(isize, isize); 4] = [(0, -1), (-1, 0), (1, 0), (0, 1)];

/// 液体找平 / 气体扩散时，一次最多横向探查多少格。
///
/// 这个数字同时决定"水面摊平得有多快"和"气体扩散得有多快"。
/// 太小则水面会停在阶梯状甚至 45° 斜坡上；太大则一个 tick 里
/// 液体能横穿小半张图，看起来像瞬移。
const LATERAL_REACH: isize = 8;

/// "本 tick 已经移动过"标记。
///
/// ## 为什么必须有它
///
/// 自下而上扫描解决的是**向下**的运动：格子往下走会进入已经扫过的行，不会再被访问。
/// 但气体与火是**向上**走的，它们会进入还没扫过的行，于是同一格在一个 tick 里
/// 可以被连续推动多次——气体一 tick 就窜到世界顶端，看起来像瞬移。
///
/// ## 为什么用 tick 戳而不是布尔数组
///
/// 布尔数组每个 tick 都要清零一遍，那是 512² 格子的额外写带宽。
/// 改成"戳"之后完全不需要清零：`stamps[i] == 当前 tick` 就是"已移动"。
/// 代价是 u32 的戳在 42 亿 tick 后会绕回——按 60 tick/s 连续跑是两年多，
/// 而且真到那时也只是**某一 tick 全体被判为已移动**，世界停一拍，不会崩。
pub struct MovedMask {
    stamps: Vec<u32>,
}

impl MovedMask {
    /// 新建一个覆盖 `len` 格的掩码，初始状态为"全部未移动"。
    pub fn new(len: usize) -> Self {
        Self {
            stamps: vec![u32::MAX; len],
        }
    }

    /// 掩码覆盖的格子数。
    #[inline]
    pub fn len(&self) -> usize {
        self.stamps.len()
    }

    /// 掩码是否为空。
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.stamps.is_empty()
    }

    /// 调整覆盖长度。长度不变时什么都不做（热路径上每 tick 都会调一次）。
    pub fn resize(&mut self, len: usize) {
        if self.stamps.len() != len {
            *self = Self::new(len);
        }
    }

    /// 标记第 `idx` 格在本 tick 已移动。
    #[inline]
    pub fn mark(&mut self, idx: usize, tick: u64) {
        self.stamps[idx] = tick as u32;
    }

    /// 第 `idx` 格在本 tick 是否已经移动过。
    #[inline]
    pub fn moved(&self, idx: usize, tick: u64) -> bool {
        self.stamps[idx] == tick as u32
    }
}

impl Default for MovedMask {
    /// 空掩码。真正的尺寸在第一次 `step` 时按世界大小分配。
    fn default() -> Self {
        Self::new(0)
    }
}

impl std::fmt::Debug for MovedMask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MovedMask")
            .field("len", &self.stamps.len())
            .finish()
    }
}

/// 一次 tick 的统计。性能断言看的是这里的数字，不是网格尺寸。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StepStats {
    /// 本 tick 实际检查过的格子数。
    pub visited: usize,
    /// 本 tick 发生过移动的格子数。
    pub moved: usize,
    /// 本 tick 参与扫描的分块数。
    pub active_chunks: usize,
    /// 本 tick 发生反应（燃烧、灭火）的格子数。
    pub reacted: usize,
}

/// 材质流动模拟器。
///
/// 自身只持有跨 tick 的辅助状态（已移动掩码、活跃分块快照），
/// 世界的全部内容都在 [`World`] 里——所以存档、哈希、渲染看到的始终是同一份数据。
#[derive(Debug, Default)]
pub struct MaterialSim {
    moved: MovedMask,
    active: Vec<bool>,
    stats: StepStats,
}

impl MaterialSim {
    /// 新建模拟器。掩码会在第一次 [`MaterialSim::step`] 时按世界尺寸分配。
    pub fn new() -> Self {
        Self {
            moved: MovedMask::new(0),
            active: Vec::new(),
            stats: StepStats::default(),
        }
    }

    /// 上一次 tick 的统计。
    #[inline]
    pub fn stats(&self) -> StepStats {
        self.stats
    }

    /// 推进一个 tick。
    ///
    /// 协议（和 [`World::clear_dirty`] / [`World::advance_tick`] 配套）：
    ///
    /// 1. 快照上一 tick 留下的脏块集合——那是本 tick 需要检查的区域；
    /// 2. 清空脏标记，于是本 tick 产生的变化重新累积，留给下一 tick；
    /// 3. 自下而上、逐块扫描；
    /// 4. 推进 tick 计数。
    ///
    /// 第 1 步必须在第 2 步之前，这就是 [`World::advance_tick`] 不再顺手清脏标记的原因。
    pub fn step(&mut self, world: &mut World) {
        let tick = world.tick();
        self.moved.resize(world.cells().len());

        // --- 1 & 2：快照活跃分块，然后清空脏标记 -------------------------
        let cols = world.chunk_cols();
        let rows = world.chunk_rows();
        self.active.clear();
        self.active.resize(cols * rows, false);
        let mut active_chunks = 0usize;
        for cy in 0..rows {
            for cx in 0..cols {
                if world.is_chunk_dirty(cx, cy) {
                    self.active[cy * cols + cx] = true;
                    active_chunks += 1;
                }
            }
        }
        world.clear_dirty();

        self.stats = StepStats {
            active_chunks,
            ..StepStats::default()
        };

        // --- 3：自下而上扫描 ---------------------------------------------
        // 分块行从下往上，块内行也从下往上，合起来正好是严格的自下而上。
        for cy in (0..rows).rev() {
            for cx in 0..cols {
                if !self.active[cy * cols + cx] {
                    continue;
                }
                self.step_chunk(world, cx, cy, tick);
            }
        }

        // --- 4 ------------------------------------------------------------
        world.advance_tick();
    }

    /// 扫描一个分块。
    fn step_chunk(&mut self, world: &mut World, cx: usize, cy: usize, tick: u64) {
        let x0 = cx * CHUNK_SIZE;
        let y0 = cy * CHUNK_SIZE;
        for y in (y0..y0 + CHUNK_SIZE).rev() {
            // 行内方向逐行交替。
            //
            // 不交替的话，一行里所有粉末都朝同一侧滑，整块沙会像被风吹一样横移——
            // 那是纯粹的扫描顺序伪影，不是物理。交替之后左右偏移在相邻行之间抵消。
            if y % 2 == 0 {
                for x in x0..x0 + CHUNK_SIZE {
                    self.visit(world, x, y, tick);
                }
            } else {
                for x in (x0..x0 + CHUNK_SIZE).rev() {
                    self.visit(world, x, y, tick);
                }
            }
        }
    }

    /// 处理单格。
    fn visit(&mut self, world: &mut World, x: usize, y: usize, tick: u64) {
        let idx = world.index_of(x, y);
        if self.moved.moved(idx, tick) {
            return;
        }
        let kind = world.get_unchecked(x, y);
        if kind == MaterialKind::Empty {
            return;
        }
        self.stats.visited += 1;

        // 1) 反应。火遇到水会当场熄灭，所以之后要重新读一次材质。
        if self.apply_reactions(world, x, y, tick) {
            return;
        }
        if world.get_unchecked(x, y) == MaterialKind::Empty {
            return;
        }

        // 2) 火的寿命。
        if world.get_unchecked(x, y) == MaterialKind::Fire && !self.tick_fire(world, x, y) {
            return;
        }

        // 3) 移动。
        let kind = world.get_unchecked(x, y);
        if kind.is_mobile() {
            self.try_move(
                world,
                Mover {
                    x: x as isize,
                    y: y as isize,
                    idx,
                    kind,
                    tick,
                },
            );
        }
    }

    /// 检查四邻，命中反应表就执行。返回是否发生了反应。
    fn apply_reactions(&mut self, world: &mut World, x: usize, y: usize, tick: u64) -> bool {
        let kind = world.get_unchecked(x, y);
        // 只有流体参与反应。石与沙是惰性的，这一步直接跳过，
        // 让绝大多数格子少走一圈邻域查询。
        if !kind.is_fluid() {
            return false;
        }
        let (xi, yi) = (x as isize, y as isize);
        for (i, (dx, dy)) in NEIGHBORS_4.iter().enumerate() {
            let (nx, ny) = (xi + dx, yi + dy);
            if !world.in_bounds(nx, ny) {
                continue;
            }
            let other = world.get(nx, ny);
            let Some((into_self, into_other)) = react(kind, other) else {
                continue;
            };

            // 点燃可燃物是概率事件；其余反应（水浇灭火）立即生效。
            if kind == MaterialKind::Fire && other.is_flammable() {
                let chance = ignite_chance(other);
                if !Rng::at_percent(x, y, tick, salt::IGNITE + i as u32, chance) {
                    continue;
                }
            }

            // 火 × 气 的产物是 (火, 火)：自己保持不变，所以不写自己，
            // 免得把这一簇火刚烧掉一半的寿命又刷新满。
            if into_self != kind {
                world.set(xi, yi, into_self);
            }
            if into_other != other {
                world.set(nx, ny, into_other);
                if into_other == MaterialKind::Fire {
                    world.set_flags(nx, ny, FIRE_LIFETIME);
                }
            }
            self.stats.reacted += 1;
            return true;
        }
        false
    }

    /// 火焰的寿命倒计时。返回 `false` 表示火已经熄灭。
    fn tick_fire(&mut self, world: &mut World, x: usize, y: usize) -> bool {
        let (xi, yi) = (x as isize, y as isize);
        let life = world.flags(xi, yi);
        if life == 0 {
            // 刚生成的火焰。材质层写材质时会把标记位清成 0，
            // 而"活着但寿命为 0"这种状态不存在（见下），所以 0 就代表"还没初始化"。
            // 放在这里而不是每个创建点，是为了让"造火"这件事只有一个地方要记着。
            world.set_flags(xi, yi, FIRE_LIFETIME);
            return true;
        }
        if life == 1 {
            world.set(xi, yi, MaterialKind::Empty);
            return false;
        }
        world.set_flags(xi, yi, life - 1);
        true
    }

    /// 按材质的移动规则尝试挪一格。
    fn try_move(&mut self, world: &mut World, m: Mover) {
        let rule = m.kind.move_rule();
        // 重力方向：粉末与液体向下，气体与火向上。
        let dir: isize = match rule {
            MoveRule::Falls | MoveRule::Flows => 1,
            MoveRule::Rises => -1,
            MoveRule::Static => return,
        };
        let side_salt = match rule {
            MoveRule::Falls => salt::POWDER_SIDE,
            MoveRule::Flows => salt::LIQUID_SIDE,
            _ => salt::GAS_SIDE,
        };
        // 先往哪一侧试，由位置派生随机数决定。两个方向都会试到，
        // 随机只影响顺序——所以"能不能动"与随机数无关，跳块扫描才安全。
        let side = Rng::at_sign(m.x as usize, m.y as usize, m.tick, side_salt);

        // 1) 顺重力直走。
        if self.try_step_to(world, m, m.x, m.y + dir) {
            return;
        }

        // 2) 斜着走。粉末只靠这一步堆积，于是自然形成休止角。
        for s in [side, -side] {
            if self.try_step_to(world, m, m.x + s, m.y + dir) {
                return;
            }
        }

        // 3) 液体找平 / 气体扩散。
        //    粉末**没有**这一步——这正是沙堆能立住、水却会摊平的原因。
        if matches!(rule, MoveRule::Flows | MoveRule::Rises) {
            for s in [side, -side] {
                if self.try_spread(world, m, s, dir) {
                    return;
                }
            }
        }
    }

    /// 横向找平：朝 `s` 方向探查最多 [`LATERAL_REACH`] 格，找到第一个
    /// "脚下能让开"的位置就挪过去。
    ///
    /// ## 为什么不是"旁边空着就走"
    ///
    /// 最直觉的写法是"侧邻可让路就平移一格"。它会**永远抖动**：
    /// 相邻两列水位 2 和 1 时，高的那列把顶上那格甩给邻居，
    /// 邻居变高之后又甩回来，两个格子一来一回，世界永远静不下来。
    /// 后果不是"看起来有点晃"，而是分块脏标记**永远清不空**——
    /// 整个跳块优化直接失效，SPEC 第 2.4 节的性能前提就没了。
    ///
    /// ## 加上"脚下能让开"之后
    ///
    /// 每次横向移动都紧跟着一次下落，于是液面只能停在"相邻列高度差 ≤ 1"。
    /// 这正是离散网格上"平"的定义。而且一次探查到 8 格，
    /// 意味着水面能从斜坡顶上越过去、落到坡脚——不然水面会停在 45° 斜坡上
    /// （只剩斜向滑动），那看起来一点都不像水。
    fn try_spread(&mut self, world: &mut World, m: Mover, s: isize, dir: isize) -> bool {
        // 重力方向的下一格落在世界之外 ⇒ 这一层已经到头了，没地方可去。
        //
        // 这一条**必须显式写出来**：`World::get` 对越界坐标返回 `Empty`，
        // 而空位对任何材质都是"可以让开"的。少了这个检查，贴在世界顶端的那层
        // 气会认为自己还能继续上浮，于是沿着天花板左右滑动、永不停歇——
        // 分块脏标记永远清不空，SPEC 第 2.4 节的性能前提直接失效。
        // 真实地貌里这一层气是气泡浮上来形成的，所以这个 bug 只会在
        // 跑够多 tick 之后才显形。
        if !world.in_bounds(m.x, m.y + dir) {
            return false;
        }

        for k in 1..=LATERAL_REACH {
            let nx = m.x + s * k;
            if !world.in_bounds(nx, m.y) {
                return false;
            }
            // 路径上撞到不能让路的材质，这一侧就到头了。
            if !world.get(nx, m.y).displaceable_by(m.kind) {
                return false;
            }
            // 脚下能让开 ⇒ 挪过去之后下一 tick 会自己掉下去。
            if world.get(nx, m.y + dir).displaceable_by(m.kind) {
                return self.try_step_to(world, m, nx, m.y);
            }
        }
        false
    }

    /// 尝试把 `m` 挪到 `(to_x, to_y)`。
    fn try_step_to(&mut self, world: &mut World, m: Mover, to_x: isize, to_y: isize) -> bool {
        if !world.in_bounds(to_x, to_y) {
            return false;
        }
        if !world.get(to_x, to_y).displaceable_by(m.kind) {
            return false;
        }
        if !world.swap(m.x, m.y, to_x, to_y) {
            return false;
        }
        // 源格与目标格都要标"已移动"：
        // 目标格是本格自己（防止一 tick 内被连续推动）；
        // 源格是刚被挤过来的那格材质（它被推开了，不该在同 tick 里再动一次）。
        self.moved
            .mark(world.index_of(to_x as usize, to_y as usize), m.tick);
        self.moved.mark(m.idx, m.tick);
        self.stats.moved += 1;
        true
    }
}

/// 一次移动尝试的主角：谁（[`Mover::kind`]）在哪儿、于哪个 tick 想动。
///
/// 打包成一个结构体而不是散成七个参数，是因为移动判定有两条路径
/// （直走 / 斜走、横向找平），两条都要完整的主角信息。
/// 散着传的时候每加一个字段就要改两处签名，而且极易错位——
/// 两个相邻的 `isize` 参数传反了编译器一句话都不会说。
#[derive(Debug, Clone, Copy)]
struct Mover {
    x: isize,
    y: isize,
    /// `(x, y)` 在网格里的扁平下标。
    idx: usize,
    kind: MaterialKind,
    tick: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worldgen::sea_and_islands;

    /// 每列里某种材质的格子数。
    fn column_counts(w: &World, kind: MaterialKind) -> Vec<usize> {
        let mut out = vec![0usize; w.width()];
        for y in 0..w.height() {
            for (x, slot) in out.iter_mut().enumerate() {
                if w.get_unchecked(x, y) == kind {
                    *slot += 1;
                }
            }
        }
        out
    }

    /// 找到某种材质的第一个位置（自下而上）。
    fn find_kind(w: &World, kind: MaterialKind) -> Option<(usize, usize)> {
        for y in (0..w.height()).rev() {
            for x in 0..w.width() {
                if w.get_unchecked(x, y) == kind {
                    return Some((x, y));
                }
            }
        }
        None
    }

    /// 一个带石地板的世界。
    fn floored(width: usize, height: usize) -> World {
        let mut w = World::new(width, height).unwrap();
        w.fill_rect(0, height as isize - 1, width, 1, MaterialKind::Stone);
        w
    }

    // -----------------------------------------------------------------------
    // 移动
    // -----------------------------------------------------------------------

    #[test]
    fn sand_falls_until_it_lands() {
        let mut w = World::new(64, 64).unwrap();
        w.set(10, 10, MaterialKind::Sand);
        let mut sim = MaterialSim::new();
        for _ in 0..200 {
            sim.step(&mut w);
        }
        assert_eq!(
            w.get(10, 63),
            MaterialKind::Sand,
            "sand should rest on the floor"
        );
        assert_eq!(w.count_of(MaterialKind::Sand), 1);
    }

    #[test]
    fn sand_rests_on_stone() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 32, 64, 32, MaterialKind::Stone);
        w.set(10, 31, MaterialKind::Sand);
        let mut sim = MaterialSim::new();
        for _ in 0..100 {
            sim.step(&mut w);
        }
        assert_eq!(w.get(10, 31), MaterialKind::Sand);
        assert_eq!(w.count_of(MaterialKind::Sand), 1);
    }

    /// 沙柱必须自己塌成休止角：相邻列高度差不超过 1。
    ///
    /// 这条是"物理看起来对不对"的核心断言。写成 ≤1 而不是"看起来像座山"，
    /// 是因为它可判定：45° 休止角在离散网格上的精确表述就是相邻列差 ≤ 1。
    #[test]
    fn sand_column_collapses_to_a_repose_angle() {
        let mut w = floored(128, 64);
        w.fill_rect(64, 43, 1, 20, MaterialKind::Sand);
        let mut sim = MaterialSim::new();
        for _ in 0..4000 {
            sim.step(&mut w);
        }

        let h = column_counts(&w, MaterialKind::Sand);
        assert_eq!(h.iter().sum::<usize>(), 20, "sand was created or destroyed");
        for x in 0..w.width() - 1 {
            let d = (h[x] as isize - h[x + 1] as isize).abs();
            assert!(d <= 1, "columns {x} and {} differ by {d}: {h:?}", x + 1);
        }
        // 而且它真的摊开了，不是还杵在原地。
        let wet = h.iter().filter(|&&c| c > 0).count();
        assert!(wet >= 5, "pile did not spread, only {wet} columns wide");
    }

    #[test]
    fn sand_sinks_through_water() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 40, 64, 24, MaterialKind::Stone);
        w.fill_rect(0, 10, 64, 30, MaterialKind::Water);
        w.set(30, 11, MaterialKind::Sand);
        let mut sim = MaterialSim::new();
        for _ in 0..200 {
            sim.step(&mut w);
        }
        let (_, y) = find_kind(&w, MaterialKind::Sand).expect("sand vanished");
        assert!(
            y >= 35,
            "sand should sink to the sea floor, stopped at y={y}"
        );
    }

    /// 水面必须找平：有水的地方，各列高度差不超过 1。
    #[test]
    fn water_levels_out() {
        let mut w = floored(128, 64);
        w.fill_rect(60, 58, 5, 5, MaterialKind::Water);
        let mut sim = MaterialSim::new();
        for _ in 0..4000 {
            sim.step(&mut w);
        }

        let h = column_counts(&w, MaterialKind::Water);
        assert_eq!(
            h.iter().sum::<usize>(),
            25,
            "water was created or destroyed"
        );
        let wet: Vec<usize> = h.iter().copied().filter(|&c| c > 0).collect();
        let hi = *wet.iter().max().unwrap();
        let lo = *wet.iter().min().unwrap();
        assert!(hi - lo <= 1, "water surface is not flat: {h:?}");
        assert!(
            wet.len() >= 20,
            "water did not spread: {} columns",
            wet.len()
        );
    }

    /// 一个已经装满、液面平整的水池必须**完全静止**。
    #[test]
    fn a_full_flat_pool_does_not_move() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 48, 64, 16, MaterialKind::Stone);
        w.fill_rect(0, 8, 64, 40, MaterialKind::Water);
        let before = w.hash();
        let mut sim = MaterialSim::new();
        for _ in 0..200 {
            sim.step(&mut w);
        }
        assert_eq!(w.hash(), before, "a flat full pool must not move at all");
    }

    /// 贴在世界顶端的一层气不能抖。
    ///
    /// 回归测试。曾经的写法把"世界之外"当成了可以让开的空位，
    /// 于是顶层气认为自己还能上浮，沿着天花板左右滑动不止——
    /// 世界永远静不下来，分块跳过形同虚设。
    /// 真实地貌里这层气是气泡浮上来形成的，所以只在跑够多 tick 之后才显形。
    #[test]
    fn gas_resting_on_the_world_ceiling_does_not_jitter() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 0, 64, 1, MaterialKind::Gas);
        let before = w.hash();
        let mut sim = MaterialSim::new();
        for _ in 0..50 {
            sim.step(&mut w);
        }
        assert_eq!(w.hash(), before, "顶上一层气来回抖了");
        assert_eq!(sim.stats().moved, 0);
    }

    /// 贴着世界底面的一层水同理。
    #[test]
    fn water_on_the_world_floor_does_not_jitter() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 63, 64, 1, MaterialKind::Water);
        let before = w.hash();
        let mut sim = MaterialSim::new();
        for _ in 0..50 {
            sim.step(&mut w);
        }
        assert_eq!(w.hash(), before, "底上一层水来回抖了");
    }

    /// 气泡上浮。注意驱动力是**上面的水沉下来**，不是气体自己往上挤——
    /// 这正是"只有更重的材质能挤开更轻的"这一条规则的推论，见 `displaceable_by`。
    #[test]
    fn gas_rises_through_water() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 32, 64, 32, MaterialKind::Stone);
        w.fill_rect(0, 16, 64, 16, MaterialKind::Water);
        w.set(20, 30, MaterialKind::Gas);
        let mut sim = MaterialSim::new();
        for _ in 0..400 {
            sim.step(&mut w);
        }
        let (_, y) = find_kind(&w, MaterialKind::Gas).expect("gas vanished");
        assert!(y < 16, "gas is still trapped in the water at y={y}");
    }

    /// 一格气体在一个 tick 里最多上浮一格。
    ///
    /// 没有"已移动"掩码时，向上走的材质会在同一 tick 里被反复推动，
    /// 一 tick 就窜到世界顶端。
    #[test]
    fn gas_moves_at_most_one_cell_per_tick() {
        let mut w = World::new(64, 64).unwrap();
        w.set(32, 63, MaterialKind::Gas);
        let mut sim = MaterialSim::new();
        sim.step(&mut w);
        assert_eq!(
            w.get(32, 62),
            MaterialKind::Gas,
            "gas should rise exactly one cell"
        );
        assert_eq!(w.get(32, 63), MaterialKind::Empty);
    }

    #[test]
    fn nothing_escapes_the_world() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 32, 64, 32, MaterialKind::Stone);
        w.fill_rect(0, 20, 64, 12, MaterialKind::Water);
        w.set(5, 10, MaterialKind::Gas);
        w.set(40, 12, MaterialKind::Sand);
        let before = w.census();
        let mut sim = MaterialSim::new();
        for _ in 0..500 {
            sim.step(&mut w);
        }
        assert_eq!(w.census(), before, "material leaked out of the world");
    }

    // -----------------------------------------------------------------------
    // 火
    // -----------------------------------------------------------------------

    #[test]
    fn fire_burns_out() {
        let mut w = floored(64, 64);
        w.set(10, 31, MaterialKind::Fire);
        let mut sim = MaterialSim::new();
        for _ in 0..(FIRE_LIFETIME as usize * 4) {
            sim.step(&mut w);
        }
        assert_eq!(
            w.count_of(MaterialKind::Fire),
            0,
            "fire must not live forever"
        );
    }

    #[test]
    fn water_puts_out_fire() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 40, 64, 24, MaterialKind::Stone);
        w.fill_rect(0, 30, 64, 10, MaterialKind::Water);
        w.set(20, 29, MaterialKind::Fire);
        let mut sim = MaterialSim::new();
        sim.step(&mut w);
        assert_eq!(
            w.count_of(MaterialKind::Fire),
            0,
            "fire next to water must go out"
        );
        assert_eq!(
            w.count_of(MaterialKind::Water),
            640,
            "water is not consumed"
        );
    }

    #[test]
    fn fire_spreads_into_gas() {
        let mut w = World::new(64, 64).unwrap();
        // 一个封闭腔：上下都是石，中间一段气体，点一簇火。
        w.fill_rect(0, 40, 64, 24, MaterialKind::Stone);
        w.fill_rect(0, 20, 64, 20, MaterialKind::Stone);
        w.fill_rect(10, 30, 20, 10, MaterialKind::Gas);
        w.set(20, 35, MaterialKind::Fire);
        let before = w.count_of(MaterialKind::Gas);

        let mut sim = MaterialSim::new();
        for _ in 0..60 {
            sim.step(&mut w);
        }
        let after = w.count_of(MaterialKind::Gas);
        assert!(after < before, "fire consumed no gas ({before} → {after})");
    }

    // -----------------------------------------------------------------------
    // 守恒与确定性
    // -----------------------------------------------------------------------

    /// 没有反应的场景里，材质总量必须逐种守恒。
    ///
    /// 用复制实现的移动会在这里红：沙落进水里会把那格水凭空吞掉。
    #[test]
    fn movement_conserves_every_material() {
        let mut w = World::new(128, 128).unwrap();
        w.fill_rect(0, 100, 128, 28, MaterialKind::Stone);
        w.fill_rect(0, 90, 128, 10, MaterialKind::Water);
        w.fill_rect(40, 60, 24, 40, MaterialKind::Sand);
        w.fill_rect(100, 96, 6, 6, MaterialKind::Remains);
        w.fill_rect(10, 95, 4, 4, MaterialKind::Gas);
        let before = w.census();

        let mut sim = MaterialSim::new();
        for _ in 0..2000 {
            sim.step(&mut w);
        }
        assert_eq!(w.census(), before);
    }

    /// **本里程碑最重要的断言**：跳过静止分块，与每个 tick 全网格扫描，
    /// 必须给出逐格相同的世界。
    ///
    /// 这条一旦成立，跳块就只是一个纯粹的加速器，而不是"会悄悄改变世界的东西"。
    /// 它同时钉住了三件事：1 格晕影够用、位置派生随机数真的与顺序无关、
    /// "已移动"掩码没有引入顺序依赖。
    #[test]
    fn skipping_static_chunks_matches_a_full_scan() {
        let build = || {
            let mut w = World::new(128, 128).unwrap();
            w.fill_rect(0, 100, 128, 28, MaterialKind::Stone);
            w.fill_rect(0, 90, 128, 10, MaterialKind::Water);
            w.fill_rect(40, 60, 24, 40, MaterialKind::Sand);
            w.fill_rect(100, 96, 6, 6, MaterialKind::Remains);
            w.fill_rect(10, 95, 4, 4, MaterialKind::Gas);
            w.set(64, 59, MaterialKind::Fire);
            w
        };
        let mut skipping = build();
        let mut full = build();
        let mut sim_skip = MaterialSim::new();
        let mut sim_full = MaterialSim::new();

        for t in 0..400 {
            sim_skip.step(&mut skipping);
            // 全扫模式：每 tick 之前把整张网格标脏。
            full.mark_all_dirty();
            sim_full.step(&mut full);
            assert_eq!(skipping.hash(), full.hash(), "跳块与全扫在第 {t} tick 分叉");
        }
    }

    /// SPEC 第 15 节的验收锚点：10,000 tick 之后同种子哈希仍一致。
    #[test]
    fn same_seed_still_matches_after_ten_thousand_ticks() {
        let build = || {
            let mut w = World::new(128, 128).unwrap();
            sea_and_islands(&mut w, &mut Rng::new(2026));
            w
        };
        let mut a = build();
        let mut b = build();
        assert_eq!(a.hash(), b.hash(), "同种子生成的初始世界就不一致");

        let mut sim_a = MaterialSim::new();
        let mut sim_b = MaterialSim::new();
        for t in 0..10_000 {
            sim_a.step(&mut a);
            sim_b.step(&mut b);
            assert_eq!(a.hash(), b.hash(), "第 {t} tick 分叉");
        }
    }

    /// 世界平静下来之后，分块脏标记必须能**清空**。
    ///
    /// 这条是 SPEC 第 2.4 节"性能取决于活跃格子比例"的物理前提：
    /// 如果世界永远静不下来，跳块扫描就永远跳不掉东西，
    /// 网格一大帧率立刻崩，而你在小地图上测不出来。
    #[test]
    fn a_settled_world_stops_being_scanned() {
        let mut w = floored(128, 64);
        w.fill_rect(50, 58, 8, 5, MaterialKind::Water);
        w.fill_rect(80, 55, 6, 8, MaterialKind::Sand);

        let mut sim = MaterialSim::new();
        let mut settled_at = None;
        for t in 0..6000 {
            sim.step(&mut w);
            if w.dirty_chunk_count() == 0 {
                settled_at = Some(t);
                break;
            }
        }
        let settled_at = settled_at.expect("世界永远静不下来——分块跳过的优化会失效");

        // 而且此后每一 tick 都保持静止，不是偶然安静一拍。
        for t in 0..50 {
            sim.step(&mut w);
            assert_eq!(
                w.dirty_chunk_count(),
                0,
                "第 {settled_at} tick 静下来之后，第 {} tick 又醒了",
                settled_at + t
            );
        }
    }

    /// **真实地貌也必须静下来。**
    ///
    /// 上面那条用的是人工搭的干净场景。真实世界里同时有沙、水、气、火、
    /// 残骸和起伏的海床，互相干扰的路径多得多——而这个 bug 恰恰只会在
    /// 真实地貌里显形：气泡浮到世界顶端那一层之后，如果"世界之外"
    /// 被当成可以让开的空位，那层气就会沿着天花板永远滑动。
    /// 人工场景里没有这一层，测试全绿，游戏里帧率却永远下不来。
    #[test]
    fn the_generated_ocean_also_settles() {
        let mut w = World::new(256, 256).unwrap();
        sea_and_islands(&mut w, &mut Rng::new(42));

        let mut sim = MaterialSim::new();
        let mut settled_at = None;
        for t in 0..10_000 {
            sim.step(&mut w);
            if w.dirty_chunk_count() == 0 {
                settled_at = Some(t);
                break;
            }
        }
        let settled_at = settled_at.expect("海洋 + 岛屿世界永远静不下来");

        for t in 0..100 {
            sim.step(&mut w);
            assert_eq!(
                w.dirty_chunk_count(),
                0,
                "第 {settled_at} tick 静下来之后，第 {} tick 又醒了",
                settled_at + t
            );
        }
    }

    /// 真实地貌上材质逐种守恒，唯一的转化是**火烧完变空**。
    ///
    /// 火是唯一的例外，而且它只会减少——这是 SPEC 第 2.4 节
    /// "材质层的燃烧单独记账、不做能量账"的直接体现。
    #[test]
    fn the_generated_ocean_conserves_everything_but_fire() {
        let mut w = World::new(256, 256).unwrap();
        sea_and_islands(&mut w, &mut Rng::new(7));
        let before = w.census();

        let mut sim = MaterialSim::new();
        for _ in 0..3000 {
            sim.step(&mut w);
        }
        let after = w.census();

        let fire = MaterialKind::Fire.as_u8() as usize;
        let empty = MaterialKind::Empty.as_u8() as usize;
        assert!(after[fire] <= before[fire], "火只该烧完，不该凭空长出来");
        let burned = before[fire] - after[fire];

        for kind in MaterialKind::ALL {
            if kind == MaterialKind::Fire {
                continue; // 已经在上面单独交代过了
            }
            let i = kind.as_u8() as usize;
            let expected = before[i] + if i == empty { burned } else { 0 };
            assert_eq!(
                after[i], expected,
                "{kind} 的数量变了：{} → {}（期望 {expected}）",
                before[i], after[i]
            );
        }
    }

    /// 静止之后，扫描成本应当掉到接近零。
    #[test]
    fn settled_world_costs_nearly_nothing() {
        let mut w = floored(128, 64);
        w.fill_rect(80, 55, 6, 8, MaterialKind::Sand);
        let mut sim = MaterialSim::new();

        // 第一个 tick：整张网格都是脏的，工作量应当明显大于零。
        sim.step(&mut w);
        let busy = sim.stats();
        assert!(busy.visited > 0, "第一 tick 就该有实际工作量");
        assert!(busy.active_chunks > 0);

        for _ in 0..2000 {
            sim.step(&mut w);
        }
        sim.step(&mut w);
        let idle = sim.stats();
        assert_eq!(idle.active_chunks, 0, "静止世界不该还有活跃分块");
        assert_eq!(idle.visited, 0, "静止世界不该还被逐格检查");
        assert_eq!(idle.moved, 0);
        assert!(idle.visited < busy.visited, "静止后的工作量应当低于繁忙时");
    }

    // -----------------------------------------------------------------------
    // 已移动掩码
    // -----------------------------------------------------------------------

    #[test]
    fn moved_mask_tracks_the_current_tick_only() {
        let mut m = MovedMask::new(8);
        assert_eq!(m.len(), 8);
        assert!(!m.moved(3, 0));
        m.mark(3, 0);
        assert!(m.moved(3, 0));
        assert!(!m.moved(3, 1), "标记只在本 tick 有效");
        assert!(!m.moved(4, 0), "别的格子不受影响");
    }

    #[test]
    fn moved_mask_starts_clean_for_tick_zero() {
        // 掩码初值不能与 tick 0 撞上，否则第一 tick 整张图都会被判为"已移动"。
        let m = MovedMask::new(16);
        for i in 0..16 {
            assert!(!m.moved(i, 0));
        }
    }

    #[test]
    fn moved_mask_resize_preserves_nothing_but_is_safe() {
        let mut m = MovedMask::new(4);
        m.mark(1, 7);
        m.resize(4);
        assert!(m.moved(1, 7), "长度没变就不该重建");
        m.resize(9);
        assert_eq!(m.len(), 9);
        assert!(!m.moved(1, 7), "长度变了要重建，旧标记必须失效");
    }

    /// 诊断工具：真实地貌跑完之后，材质都跑到哪儿去了。
    ///
    /// ```text
    /// cargo test -p vivarium-core --release diagnose -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "诊断工具，手动运行"]
    fn diagnose_material_flow() {
        const W: usize = 512;
        let mut w = World::new(W, W).unwrap();
        let layout = sea_and_islands(&mut w, &mut Rng::new(42));

        let count_above = |world: &World, kind: MaterialKind| {
            let mut n = 0usize;
            for x in 0..world.width() {
                for y in 0..layout.waterline {
                    if world.get_unchecked(x, y) == kind {
                        n += 1;
                    }
                }
            }
            n
        };

        println!("水面线 y={}  海床线 y={}", layout.waterline, layout.seabed);

        // 露出水面的陆地有多少列、分成几段（几段 = 几座彼此分离的岛）
        let land: Vec<bool> = layout
            .surface
            .iter()
            .map(|&s| s <= layout.waterline as isize)
            .collect();
        let runs = land
            .iter()
            .fold((0usize, false), |(n, prev), &l| {
                if l && !prev {
                    (n + 1, true)
                } else {
                    (n, l)
                }
            })
            .0;
        println!(
            "露出水面的陆地 {} 列（{:.0}%），分成 {runs} 段",
            land.iter().filter(|&&l| l).count(),
            land.iter().filter(|&&l| l).count() as f64 / land.len() as f64 * 100.0,
        );
        println!(
            "初始  沙（水上/水下） {} / {}   骸 {} / {}",
            count_above(&w, MaterialKind::Sand),
            w.count_of(MaterialKind::Sand) - count_above(&w, MaterialKind::Sand),
            count_above(&w, MaterialKind::Remains),
            w.count_of(MaterialKind::Remains) - count_above(&w, MaterialKind::Remains),
        );

        let mut sim = MaterialSim::new();
        let mut settled = None;
        for t in 0..20_000 {
            sim.step(&mut w);
            if w.dirty_chunk_count() == 0 {
                settled = Some(t);
                break;
            }
        }
        println!("静止于第 {:?} tick", settled);

        let sand = w.count_of(MaterialKind::Sand);
        let remains = w.count_of(MaterialKind::Remains);
        println!(
            "最终  沙（水上/水下） {} / {}   骸 {} / {}",
            count_above(&w, MaterialKind::Sand),
            sand - count_above(&w, MaterialKind::Sand),
            count_above(&w, MaterialKind::Remains),
            remains - count_above(&w, MaterialKind::Remains),
        );

        // 全图缩略（每 8×8 格取一格），一眼看清整体形状
        println!("\n全图缩略（每 8×8 取一格；# 石  . 沙  ~ 水  % 骸  空格 空）:");
        for y in (0..W).step_by(8) {
            let row: String = (0..W)
                .step_by(8)
                .map(|x| w.get_unchecked(x, y).glyph())
                .collect();
            println!("{row}");
        }

        // 水面线附近的剖面，看看沙堆成了什么形状
        println!("\n水面线附近剖面（# 石  . 沙  ~ 水  % 骸）:");
        let x0 = 200usize;
        for y in (layout.waterline.saturating_sub(12))..(layout.waterline + 40) {
            let row: String = (x0..x0 + 110)
                .map(|x| w.get_unchecked(x, y).glyph())
                .collect();
            println!("y={y:>3} {row}");
        }
    }
}
