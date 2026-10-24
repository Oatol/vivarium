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
//!
//! ## 并行（M1 收尾）
//!
//! 调度的最小单位是**一个分块**。分块按 `(cx % 2, cy % 2)` 分成 4 个棋盘式
//! 相位，同一相位里的分块在 x、y 上至少隔着一整块（128 格），而一次移动
//! 最多横跨 [`LATERAL_REACH`] = 8 格、纵跨 1 格——所以**同一相位内任意两块
//! 的可写集合必然不相交**。这条不等式（`2 * LATERAL_REACH < CHUNK_SIZE`）
//! 就是并行的全部前提，它在下面有一个编译期断言钉着。
//!
//! 更关键的是：为了让"分块"真的独立，每个分块先被拷进**私有工作副本**，
//! 在副本上跑完全部规则，再一次性写回。于是线程之间没有任何共享的可变状态——
//! 整个项目一行 `unsafe` 都不需要，而"线程数不影响结果"是**构造上成立**的，
//! 不是靠测试碰运气。代价是每块多一次拷贝（实测约占满载 tick 的 1%）。
//!
//! 单线程路径就是并行路径的一个特例（相位照分、副本照拷，只是工人只有一个），
//! 所以两条路径共用同一份规则代码，不存在"并行版悄悄改了行为"的可能。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Barrier, Mutex, RwLock};

use crate::material::{ignite_chance, react, MaterialKind, MoveRule, FIRE_LIFETIME};
use crate::rng::Rng;
use crate::world::{MaterialCell, World, CHUNK_SIZE};

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

/// **晕影必须覆盖得住一次移动的横向跨度。**
///
/// 这两条数字是同一个问题的两面：`LATERAL_REACH` 说"一次移动能跨多远"，
/// `DIRTY_HALO_X` 说"一格变了要连累多远"。前者大于后者时，跳块扫描就会
/// 漏掉一格"因为八格之外的变化而变得可动"的格子——而那不报错，只是让
/// 世界跑出另一个终态。
///
/// 它们曾经是 8 和 1，于是 512² 世界里沙块砸进浅水就能分叉（实测 tick 117）。
const _: () = assert!(LATERAL_REACH as usize <= crate::world::DIRTY_HALO_X);

/// 棋盘式相位的执行顺序。
///
/// 4 个相位按 `(cx % 2, cy % 2)` 划分：同一相位里任意两块在 x、y 上
/// 至少隔着一整块，而一次移动最多横跨 [`LATERAL_REACH`] 格、纵跨 1 格，
/// 所以同一相位内两块的可写集合不可能相交。
///
/// 顺序上**把靠下的分块行排在前面**（`cy % 2 == 1` 的两组先跑）：
/// 重力方向的移动占绝大多数，让它们先发生，落下的材质就落在"本 tick
/// 已经处理过"的区域里，不会在一个 tick 内被推动两次。
const PHASE_ORDER: [(usize, usize); 4] = [(0, 1), (1, 1), (0, 0), (1, 0)];

/// 分块 `(cx, cy)` 落在第几个相位。
#[inline]
const fn phase_index(cx: usize, cy: usize) -> usize {
    let want = (cx % 2, cy % 2);
    let mut i = 0;
    while i < PHASE_ORDER.len() {
        if PHASE_ORDER[i].0 == want.0 && PHASE_ORDER[i].1 == want.1 {
            return i;
        }
        i += 1;
    }
    // 上面四个相位已经覆盖了所有奇偶组合，走不到这里。
    unreachable!()
}

/// 并行能成立的全部前提：同一相位里两块伸向对方的"手"够不到彼此。
///
/// 两块相邻分块各自最多向对方探进 [`LATERAL_REACH`] 格，中间还有一块的话，
/// 两边的手之间至少要留出 `CHUNK_SIZE - 2 * LATERAL_REACH` 格空隙。
/// 这条一旦不成立，并行就会变成"两个线程偶尔写同一格"——
/// 一个只在特定地图上偶发、且换台机器就复现不出来的 bug。
const _: () = assert!(2 * (LATERAL_REACH as usize) < CHUNK_SIZE);

/// 自动选线程数时，每个工人一个相位至少要摊到几个分块。
///
/// 建一个线程实测约 30 µs（Windows）。如果一个相位里每个工人只领到一块，
/// 那它分担的活还抵不上建它的开销——512² 满负载时一个相位总工作量约
/// 0.75 ms，16 个工人各领一块就是每人 47 µs，而建线程要 30 µs。
/// 要求每人至少两块，512²（每相位 16 块）就正好落在 8 个工人上，
/// 实测也是这个点最快。
///
/// 这条只约束**自动**选出来的线程数。显式 `with_threads(n)` 照办，
/// 因为调参的人应该能自己压过启发式。
const MIN_CHUNKS_PER_WORKER: usize = 2;

// ---------------------------------------------------------------------------
// 批量写回时"这一格变了，哪些分块要跟着重扫"
// ---------------------------------------------------------------------------

/// 9 个相邻分块各占一位。
const OWN: u16 = 1 << 0;
const L: u16 = 1 << 1;
const R: u16 = 1 << 2;
const T: u16 = 1 << 3;
const B: u16 = 1 << 4;
const LT: u16 = 1 << 5;
const RT: u16 = 1 << 6;
const LB: u16 = 1 << 7;
const RB: u16 = 1 << 8;

/// 按"这一格贴着哪几条分块边"查表得到要标脏的分块集合。
///
/// 索引的 4 位是 左 / 右 / 上 / 下（1 / 2 / 4 / 8）。
///
/// ## 对角块为什么不能"按块"判断
///
/// 这里踩过一个坑：为了省事写成"这一块有格子贴左边 ∧ 有格子贴上边 ⇒ 标左上块"，
/// 看起来等价，其实不是——那两个格子可能是**两个不同的格子**，
/// 而逐格规则只在**同一格**同时贴着两条边时才需要标对角块。
/// 多标一个块 = 多扫一片本来该跳过的格子 = 世界跑出另一个终态。
/// 当时 134 条测试全绿，是拿真实地貌跑 `--seed 42` 和上一版比哈希才发现的
/// （`0x1abf…` 变成了 `0x1923…`，平均活跃块从 9.5 涨到 10.2）。
///
/// 所以这张表必须**逐格**查、再按位或起来。
const NEIGHBOR_MASK: [u16; 16] = [
    OWN,                                     // 内部
    OWN | L,                                 // 左
    OWN | R,                                 // 右
    OWN | L | R,                             // 左右
    OWN | T,                                 // 上
    OWN | L | T | LT,                        // 左上角
    OWN | R | T | RT,                        // 右上角
    OWN | L | R | T | LT | RT,               // 左 + 右 + 上
    OWN | B,                                 // 下
    OWN | L | B | LB,                        // 左下角
    OWN | R | B | RB,                        // 右下角
    OWN | L | R | B | LB | RB,               // 左 + 右 + 下
    OWN | T | B,                             // 上 + 下
    OWN | L | T | B | LT | LB,               // 左 + 上 + 下
    OWN | R | T | B | RT | RB,               // 右 + 上 + 下
    OWN | L | R | T | B | LT | RT | LB | RB, // 四条边都贴
];

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
    /// 本 tick 实际用了几个工人。`1` 表示走的是顺序路径。
    pub threads: usize,
}

// ---------------------------------------------------------------------------
// 分块私有工作副本
// ---------------------------------------------------------------------------

/// 跨出本块的一次写入。
///
/// 一个分块在并行处理期间**不碰任何共享状态**：本块的格子改在私有副本上，
/// 跨出本块的写入（比如边界上的水挤进邻块）先记在这里，等这一轮结束、
/// 所有线程都退出了，再顺序写回真世界。
///
/// 同一格最多留一条记录（后来的覆盖先前的），所以写回的先后顺序不影响结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outbound {
    /// 目标格在真世界里的扁平下标。
    idx: usize,
    /// 目标格在真世界里的坐标（写回时用它标脏）。
    x: usize,
    y: usize,
    cell: MaterialCell,
}

/// 本块这一 tick 的"已经动过"标记。
///
/// 同一分块内部的标记是线程局部的，只有**跨出分块**的那些（`spill`）
/// 需要汇入全局掩码——好让本 tick 还没处理到的分块看得见。
/// 已经被处理过的分块收到标记也没关系：它这一 tick 不会再被访问。
#[derive(Debug, Default)]
struct LocalMoved {
    /// 本块格子的标记，行主序，`w × h` 字节。每次装填时清零。
    bits: Vec<u8>,
    /// 跨块标记（真世界的扁平下标），写回时并入全局掩码。
    spill: Vec<usize>,
}

impl LocalMoved {
    /// 清空，准备处理下一个分块。
    ///
    /// 尺寸是**每块现问**的：世界不必是 `CHUNK_SIZE` 的整数倍，
    /// 最右一列/最下一行的分块是残块。
    fn reset(&mut self, w: usize, h: usize) {
        self.bits.clear();
        self.bits.resize(w * h, 0);
        self.spill.clear();
    }
}

/// 一个分块的私有工作副本。
///
/// 这是并行调度的最小工作单元，也是"整个项目不需要 `unsafe`"的原因：
/// 规则只在副本上跑，线程之间连一格共享数据都没有。
struct Workspace {
    x0: usize,
    y0: usize,
    /// 本块的真实宽高（格）。**不是** [`CHUNK_SIZE`]——边缘的块可能更小。
    w: usize,
    h: usize,
    /// 本块格子，行主序，`w × h` 个。
    cells: Vec<MaterialCell>,
    moved: LocalMoved,
    outbox: Vec<Outbound>,
    stats: StepStats,
}

impl Workspace {
    fn new() -> Self {
        Self {
            x0: 0,
            y0: 0,
            w: CHUNK_SIZE,
            h: CHUNK_SIZE,
            cells: Vec::new(),
            moved: LocalMoved::default(),
            outbox: Vec::new(),
            stats: StepStats::default(),
        }
    }
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("chunk", &(self.x0 / CHUNK_SIZE, self.y0 / CHUNK_SIZE))
            .field("size", &(self.w, self.h))
            .field("outbox", &self.outbox.len())
            .finish()
    }
}

/// 规则执行上下文：一个分块的私有副本 + 对真世界的**只读**访问。
///
/// 只读是关键。写入只发生在副本和 `outbox` 上，于是这个结构可以被任意多个
/// 线程同时持有，彼此之间不会有任何冲突——这就是并行不需要 `unsafe` 的全部理由。
struct Ctx<'a> {
    world: &'a World,
    /// 全局"已移动"掩码，只读——用来接收**前几个相位**留给本块的标记。
    global_moved: &'a MovedMask,
    ws: &'a mut Workspace,
}

impl Ctx<'_> {
    /// 坐标是否落在本块内。
    #[inline]
    fn in_chunk(&self, x: isize, y: isize) -> bool {
        x >= self.ws.x0 as isize
            && x < (self.ws.x0 + self.ws.w) as isize
            && y >= self.ws.y0 as isize
            && y < (self.ws.y0 + self.ws.h) as isize
    }

    /// 本块内坐标 → 副本里的偏移。
    ///
    /// 步长是 `ws.w` 而不是 `CHUNK_SIZE`：残块每行更短，用常数会让
    /// 第 2 行起全部错位——而错位的症状是"世界右边缘和底边缘有一片
    /// 莫名其妙的东西"，看起来完全不像索引问题。
    #[inline]
    fn off(&self, x: usize, y: usize) -> usize {
        (y - self.ws.y0) * self.ws.w + (x - self.ws.x0)
    }

    /// 读一格。
    ///
    /// 本块内读副本；本块外读真世界，但先看有没有已经排队的写入——
    /// 同一格可能在一个 tick 里被写过好几次（点燃、又被水浇灭），
    /// 后来读的人必须看到最后那次。
    #[inline]
    fn get(&self, x: isize, y: isize) -> MaterialKind {
        if self.in_chunk(x, y) {
            self.ws.cells[self.off(x as usize, y as usize)].kind
        } else if self.world.in_bounds(x, y) {
            let idx = self.world.index_of(x as usize, y as usize);
            match self.outbound(idx) {
                Some(c) => c.kind,
                None => self.world.cells()[idx].kind,
            }
        } else {
            MaterialKind::Empty
        }
    }

    /// 读本块内的一格，不做任何检查。
    #[inline]
    fn get_in(&self, x: usize, y: usize) -> MaterialKind {
        self.ws.cells[self.off(x, y)].kind
    }

    /// 这一格能不能被 `mover` 挤开。
    ///
    /// 占格表不进分块副本（材质层从不写它），所以直接问真世界——
    /// 生命层跑完之后才轮到材质层，一个 tick 之内它不会变。
    #[inline]
    fn yields_to(&self, x: isize, y: isize, mover: MaterialKind) -> bool {
        if self.world.is_occupied(x, y) {
            return false;
        }
        self.get(x, y).displaceable_by(mover)
    }

    /// 读整格（材质 + 标记位）。
    #[inline]
    fn cell_at(&self, x: isize, y: isize) -> MaterialCell {
        if self.in_chunk(x, y) {
            self.ws.cells[self.off(x as usize, y as usize)]
        } else if self.world.in_bounds(x, y) {
            let idx = self.world.index_of(x as usize, y as usize);
            match self.outbound(idx) {
                Some(c) => c,
                None => self.world.cells()[idx],
            }
        } else {
            MaterialCell::default()
        }
    }

    /// 写一格。语义与 [`World::set`] 完全一致：值没变就不算写，
    /// 换了材质要清空标记位。
    fn set(&mut self, x: isize, y: isize, kind: MaterialKind) -> bool {
        if self.in_chunk(x, y) {
            let off = self.off(x as usize, y as usize);
            if self.ws.cells[off].kind == kind {
                return false;
            }
            self.ws.cells[off].kind = kind;
            self.ws.cells[off].flags = 0;
            true
        } else if self.world.in_bounds(x, y) {
            if self.get(x, y) == kind {
                return false;
            }
            self.put_outbound(x, y, MaterialCell::new(kind));
            true
        } else {
            false
        }
    }

    /// 读一格的标记位。越界返回 0。
    #[inline]
    fn flags(&self, x: isize, y: isize) -> u8 {
        self.cell_at(x, y).flags
    }

    /// 写一格的标记位。语义与 [`World::set_flags`] 一致。
    fn set_flags(&mut self, x: isize, y: isize, flags: u8) -> bool {
        if self.in_chunk(x, y) {
            let off = self.off(x as usize, y as usize);
            if self.ws.cells[off].flags == flags {
                return false;
            }
            self.ws.cells[off].flags = flags;
            true
        } else if self.world.in_bounds(x, y) {
            let mut cell = self.cell_at(x, y);
            if cell.flags == flags {
                return false;
            }
            cell.flags = flags;
            self.put_outbound(x, y, cell);
            true
        } else {
            false
        }
    }

    /// 交换两格。语义与 [`World::swap`] 一致：任一端越界、或两格内容相同，
    /// 都返回 `false` 且不产生写入。
    fn swap(&mut self, x1: isize, y1: isize, x2: isize, y2: isize) -> bool {
        if !self.world.in_bounds(x1, y1) || !self.world.in_bounds(x2, y2) {
            return false;
        }
        let a = self.cell_at(x1, y1);
        let b = self.cell_at(x2, y2);
        if a == b {
            return false;
        }
        self.put_cell(x1, y1, b);
        self.put_cell(x2, y2, a);
        true
    }

    /// 整格写入。
    fn put_cell(&mut self, x: isize, y: isize, cell: MaterialCell) {
        if self.in_chunk(x, y) {
            let off = self.off(x as usize, y as usize);
            self.ws.cells[off] = cell;
        } else {
            self.put_outbound(x, y, cell);
        }
    }

    /// 记下一条跨块写入。
    fn put_outbound(&mut self, x: isize, y: isize, cell: MaterialCell) {
        let (xu, yu) = (x as usize, y as usize);
        let idx = self.world.index_of(xu, yu);
        for o in self.ws.outbox.iter_mut() {
            if o.idx == idx {
                o.cell = cell;
                return;
            }
        }
        self.ws.outbox.push(Outbound {
            idx,
            x: xu,
            y: yu,
            cell,
        });
    }

    /// 查有没有排队的跨块写入。
    #[inline]
    fn outbound(&self, idx: usize) -> Option<MaterialCell> {
        self.ws.outbox.iter().find(|o| o.idx == idx).map(|o| o.cell)
    }

    /// 本格在本 tick 是否已经动过。
    ///
    /// 本块内部的标记在副本里；跨块进来的标记（前几个相位留下的 `spill`）
    /// 在全局掩码里。两处都要看。
    #[inline]
    fn already_moved(&self, x: usize, y: usize, idx: usize, tick: u64) -> bool {
        self.ws.moved.bits[self.off(x, y)] != 0 || self.global_moved.moved(idx, tick)
    }

    /// 标记一格在本 tick 已移动。
    ///
    /// 本块内的标记留在副本里就够了——本块这一 tick 只会被处理一次。
    /// 跨出本块的标记必须传给**本 tick 还没处理到**的分块，所以要进全局掩码。
    #[inline]
    fn mark_moved(&mut self, x: isize, y: isize) {
        if self.in_chunk(x, y) {
            let off = self.off(x as usize, y as usize);
            self.ws.moved.bits[off] = 1;
        } else {
            let idx = self.world.index_of(x as usize, y as usize);
            self.ws.moved.spill.push(idx);
        }
    }
}

/// 默认工人数：本机的可用并行度。
///
/// 世界静下来之后一个分块都不会被扫，所以这个默认值不会在空闲时白占着线程。
fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// 材质流动模拟器。
///
/// 自身只持有跨 tick 的辅助状态（已移动掩码、活跃分块快照、工作区池），
/// 世界的全部内容都在 [`World`] 里——所以存档、哈希、渲染看到的始终是同一份数据。
pub struct MaterialSim {
    moved: MovedMask,
    active: Vec<bool>,
    /// 本 tick 要扫的分块，已按相位分好组。
    phases: [Vec<(usize, usize)>; PHASE_ORDER.len()],
    /// 分块私有副本的池子，跨相位、跨 tick 复用，免得每 tick 重新分配几十 KB。
    pool: Vec<Workspace>,
    /// 并行工人数上限。`1` 就是纯顺序执行。
    threads: usize,
    /// `threads` 是"自动选的"还是"人指定的"。
    ///
    /// 自动的那个还要按本 tick 的活量往下压（见 [`MIN_CHUNKS_PER_WORKER`]）；
    /// 人指定的不压——调参的人应该能自己压过启发式，否则
    /// `--threads 16` 在 512² 上会被悄悄改成 8，测出来的数字就不是他要的。
    threads_auto: bool,
    /// 上一次**真的干了活**的 tick 用了几个工人。
    ///
    /// 单独存一份是因为世界静下来之后 `step` 什么都不扫，那时的
    /// `stats.threads` 是 0——报"工人 0"读起来像配置错了。
    workers: usize,
    stats: StepStats,
}

impl Default for MaterialSim {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for MaterialSim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaterialSim")
            .field("threads", &self.threads)
            .field("threads_auto", &self.threads_auto)
            .field("pooled_workspaces", &self.pool.len())
            .field("stats", &self.stats)
            .finish()
    }
}

impl MaterialSim {
    /// 新建模拟器，工人数取本机可用并行度，再按本 tick 的活量自动收敛。
    /// 掩码与工作区会在第一次 [`MaterialSim::step`] 时按世界尺寸分配。
    pub fn new() -> Self {
        Self {
            threads: default_threads(),
            threads_auto: true,
            ..Self::with_threads(1)
        }
    }

    /// 指定并行工人数上限。`1` 就是纯顺序执行。
    ///
    /// 这是**显式**指定：不会按活量再往下压。
    pub fn with_threads(threads: usize) -> Self {
        Self {
            moved: MovedMask::new(0),
            active: Vec::new(),
            phases: [Vec::new(), Vec::new(), Vec::new(), Vec::new()],
            pool: Vec::new(),
            threads: threads.max(1),
            threads_auto: false,
            workers: 1,
            stats: StepStats::default(),
        }
    }

    /// 当前的并行工人数上限。
    #[inline]
    pub fn threads(&self) -> usize {
        self.threads
    }

    /// 上一次真正扫过分块的 tick 用了几个工人。
    ///
    /// 世界静止时 `step` 一格都不扫，这个值保持为最后一次干活时的数字——
    /// 它回答的是"这台机器上这套参数实际并行到了什么程度"。
    #[inline]
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// 改并行工人数上限。同样是显式指定，不再自动收敛。
    pub fn set_threads(&mut self, threads: usize) {
        self.threads = threads.max(1);
        self.threads_auto = false;
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
    /// 3. 按棋盘式相位逐轮扫描，相位内部并行；
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

        // --- 3：按相位扫描 -----------------------------------------------
        for phase in self.phases.iter_mut() {
            phase.clear();
        }
        if active_chunks > 0 {
            // 分块行从下往上：靠下的行排进靠前的相位，见 [`PHASE_ORDER`]。
            for cy in (0..rows).rev() {
                for cx in 0..cols {
                    if self.active[cy * cols + cx] {
                        self.phases[phase_index(cx, cy)].push((cx, cy));
                    }
                }
            }
            // 自动选的线程数还要按本 tick 的活量收敛：相位里块太少的话，
            // 多出来的工人只会白花建线程的钱。显式指定的不压。
            let largest = self.phases.iter().map(|p| p.len()).max().unwrap_or(0);
            let mut threads = self.threads.min(active_chunks);
            if self.threads_auto {
                threads = threads.min((largest / MIN_CHUNKS_PER_WORKER).max(1));
            }
            self.stats.threads = threads;
            self.workers = threads;

            if threads <= 1 {
                for p in 0..PHASE_ORDER.len() {
                    self.run_phase_serial(world, p, tick);
                }
            } else {
                self.run_phases_parallel(world, tick, threads);
            }
        }

        world.advance_tick();
    }

    /// 顺序跑一个相位。
    fn run_phase_serial(&mut self, world: &mut World, p: usize, tick: u64) {
        for i in 0..self.phases[p].len() {
            let (cx, cy) = self.phases[p][i];
            let mut ws = self.pool.pop().unwrap_or_else(Workspace::new);
            load(world, cx, cy, &mut ws);
            run_chunk(world, &self.moved, &mut ws, tick);
            apply(world, &mut self.moved, &ws, tick, &mut self.stats);
            self.pool.push(ws);
        }
    }

    /// 并行跑完 4 个相位。
    ///
    /// ## 为什么整个 tick 只建一次线程
    ///
    /// 一开始每个相位建一批线程、相位结束就 join。实测建一次线程约 30 µs，
    /// 4 个相位 × `threads - 1` 次：16 线程时一个 tick 要建 60 次线程，
    /// 光这一项就吃掉将近一半的 tick 时间——**线程越多反而越慢**，
    /// 而默认线程数正好取 `available_parallelism()`，等于默认给最差的配置。
    /// 现在线程在 tick 开始时建一次，相位之间用屏障对齐。
    ///
    /// ## 主线程怎么在相位之间拿到 `&mut World`
    ///
    /// 写回必须排他，而工人手里握着 `&World` 在读——类型系统不允许两者并存，
    /// 可实际上它们从来不在同一时刻发生。把世界和"已移动"掩码一起塞进
    /// [`RwLock`]：工人取读锁，主线程取写锁，屏障保证写锁只在所有工人都
    /// 放开读锁之后才被申请。锁本身几乎不竞争——一个工人一个相位只取一次读锁。
    fn run_phases_parallel(&mut self, world: &mut World, tick: u64, threads: usize) {
        let phases = &self.phases;
        let stats = &mut self.stats;
        let shared = RwLock::new((world, &mut self.moved));
        let barrier = Barrier::new(threads);
        let cursors: [AtomicUsize; PHASE_ORDER.len()] =
            std::array::from_fn(|_| AtomicUsize::new(0));

        // 每个工人一对自己的槽（空闲副本 / 已算完）。
        //
        // 这里原本是一把共享的 `Mutex`：64 个分块每个锁两次，8 个线程
        // 一个 tick 往同一把锁上撞 128 次。改成每工人一把之后锁从来没被争过，
        // 它退化成一个 `&mut`——只是借检查器看得见的那种。
        let mut free: Vec<Mutex<Vec<Workspace>>> =
            (0..threads).map(|_| Mutex::new(Vec::new())).collect();
        for (k, ws) in std::mem::take(&mut self.pool).into_iter().enumerate() {
            free[k % threads].get_mut().unwrap().push(ws);
        }
        let done: Vec<Mutex<Vec<Workspace>>> =
            (0..threads).map(|_| Mutex::new(Vec::new())).collect();

        let shared = &shared;
        let barrier = &barrier;
        let cursors = &cursors;
        let free = &free;
        let done = &done;

        // 一个工人在一个相位里干的活：从原子计数器领块，算完丢进自己的槽。
        // 动态领块而不是预先均分，是因为"哪一块在动"完全取决于地图，
        // 静态切分很容易让一个工人扛走所有重活。
        let work = move |i: usize, p: usize| {
            let guard = shared.read().unwrap();
            let (w, m) = (&*guard.0, &*guard.1);
            let list: &[(usize, usize)] = &phases[p];
            loop {
                let k = cursors[p].fetch_add(1, Ordering::Relaxed);
                if k >= list.len() {
                    break;
                }
                let (cx, cy) = list[k];
                let mut ws = free[i].lock().unwrap().pop().unwrap_or_else(Workspace::new);
                load(w, cx, cy, &mut ws);
                run_chunk(w, m, &mut ws, tick);
                done[i].lock().unwrap().push(ws);
            }
        };

        std::thread::scope(|s| {
            for i in 1..threads {
                s.spawn(move || {
                    for p in 0..PHASE_ORDER.len() {
                        work(i, p);
                        barrier.wait();
                        barrier.wait();
                    }
                });
            }
            // 主线程：也当工人，然后在相位之间排他地写回。
            for p in 0..PHASE_ORDER.len() {
                work(0, p);
                barrier.wait();
                {
                    let mut guard = shared.write().unwrap();
                    let (wr, mv) = &mut *guard;
                    let (w, m) = (&mut **wr, &mut **mv);
                    for (i, slot) in done.iter().enumerate() {
                        let batch: Vec<Workspace> = slot.lock().unwrap().drain(..).collect();
                        for ws in &batch {
                            apply(w, m, ws, tick, stats);
                        }
                        free[i].lock().unwrap().extend(batch);
                    }
                }
                barrier.wait();
            }
        });

        // 副本全部收回来，下一 tick 接着用。
        let mut all = Vec::new();
        for slot in free {
            all.append(&mut slot.lock().unwrap());
        }
        self.pool = all;
    }
}

/// 全部规则都在这里，跑在**一个分块的私有副本**上。
///
/// 读写都走 [`Ctx`] 的访问器而不是直接碰 [`World`]，所以同一份代码
/// 在顺序路径和并行路径上跑出来完全一样。
impl Ctx<'_> {
    /// 处理单格。
    fn visit(&mut self, x: usize, y: usize, tick: u64) {
        let idx = self.world.index_of(x, y);
        if self.already_moved(x, y, idx, tick) {
            return;
        }
        let kind = self.get_in(x, y);
        if kind == MaterialKind::Empty {
            return;
        }
        self.ws.stats.visited += 1;

        // 1) 反应。火遇到水会当场熄灭，所以之后要重新读一次材质。
        if self.apply_reactions(x, y, tick) {
            return;
        }
        if self.get_in(x, y) == MaterialKind::Empty {
            return;
        }

        // 2) 火的寿命。
        if self.get_in(x, y) == MaterialKind::Fire && !self.tick_fire(x, y) {
            return;
        }

        // 3) 移动。
        let kind = self.get_in(x, y);
        if kind.is_mobile() {
            self.try_move(Mover {
                x: x as isize,
                y: y as isize,
                kind,
                tick,
            });
        }
    }

    /// 检查四邻，命中反应表就执行。返回是否发生了反应。
    fn apply_reactions(&mut self, x: usize, y: usize, tick: u64) -> bool {
        let kind = self.get_in(x, y);
        // 只有流体参与反应。石与沙是惰性的，这一步直接跳过，
        // 让绝大多数格子少走一圈邻域查询。
        if !kind.is_fluid() {
            return false;
        }
        let (xi, yi) = (x as isize, y as isize);
        for (i, (dx, dy)) in NEIGHBORS_4.iter().enumerate() {
            let (nx, ny) = (xi + dx, yi + dy);
            if !self.world.in_bounds(nx, ny) {
                continue;
            }
            let other = self.get(nx, ny);
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
                self.set(xi, yi, into_self);
            }
            if into_other != other {
                self.set(nx, ny, into_other);
                if into_other == MaterialKind::Fire {
                    self.set_flags(nx, ny, FIRE_LIFETIME);
                }
            }
            self.ws.stats.reacted += 1;
            return true;
        }
        false
    }

    /// 火焰的寿命倒计时。返回 `false` 表示火已经熄灭。
    fn tick_fire(&mut self, x: usize, y: usize) -> bool {
        let (xi, yi) = (x as isize, y as isize);
        let life = self.flags(xi, yi);
        if life == 0 {
            // 刚生成的火焰。材质层写材质时会把标记位清成 0，
            // 而"活着但寿命为 0"这种状态不存在（见下），所以 0 就代表"还没初始化"。
            // 放在这里而不是每个创建点，是为了让"造火"这件事只有一个地方要记着。
            self.set_flags(xi, yi, FIRE_LIFETIME);
            return true;
        }
        if life == 1 {
            self.set(xi, yi, MaterialKind::Empty);
            return false;
        }
        self.set_flags(xi, yi, life - 1);
        true
    }

    /// 按材质的移动规则尝试挪一格。
    fn try_move(&mut self, m: Mover) {
        let rule = m.kind.move_rule();
        // 重力方向：粉末与液体向下，气体与火向上。
        let dir: isize = match rule {
            MoveRule::Falls | MoveRule::Flows => 1,
            MoveRule::Rises => -1,
            // 悬浮物不自己动，只被更重的东西挤开（见 `MaterialKind::displaceable_by`）。
            // 这里到不了——`visit` 用 `is_mobile` 把它挡在外面——但写出来更清楚。
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
        if self.try_step_to(m, m.x, m.y + dir) {
            return;
        }

        // 2) 斜着走。粉末只靠这一步堆积，于是自然形成休止角。
        for s in [side, -side] {
            if self.try_step_to(m, m.x + s, m.y + dir) {
                return;
            }
        }

        // 3) 液体找平 / 气体扩散。
        //    粉末**没有**这一步——这正是沙堆能立住、水却会摊平的原因。
        if matches!(rule, MoveRule::Flows | MoveRule::Rises) {
            for s in [side, -side] {
                if self.try_spread(m, s, dir) {
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
    ///
    /// ## 为什么一次最多只探 8 格
    ///
    /// 这个上限同时是**并行安全的前提**：同一相位里两块至少隔着一整块，
    /// 而 8 格够不到 64 格以外。见 [`PHASE_ORDER`] 与那个编译期断言。
    fn try_spread(&mut self, m: Mover, s: isize, dir: isize) -> bool {
        // 重力方向的下一格落在世界之外 ⇒ 这一层已经到头了，没地方可去。
        //
        // 这一条**必须显式写出来**：`World::get` 对越界坐标返回 `Empty`，
        // 而空位对任何材质都是"可以让开"的。少了这个检查，贴在世界顶端的那层
        // 气会认为自己还能继续上浮，于是沿着天花板左右滑动、永不停歇——
        // 分块脏标记永远清不空，SPEC 第 2.4 节的性能前提直接失效。
        // 真实地貌里这一层气是气泡浮上来形成的，所以这个 bug 只会在
        // 跑够多 tick 之后才显形。
        if !self.world.in_bounds(m.x, m.y + dir) {
            return false;
        }

        for k in 1..=LATERAL_REACH {
            let nx = m.x + s * k;
            if !self.world.in_bounds(nx, m.y) {
                return false;
            }
            // 路径上撞到不能让路的材质，这一侧就到头了。
            if !self.yields_to(nx, m.y, m.kind) {
                return false;
            }
            // 脚下能让开 ⇒ 挪过去之后下一 tick 会自己掉下去。
            if self.yields_to(nx, m.y + dir, m.kind) {
                return self.try_step_to(m, nx, m.y);
            }
        }
        false
    }

    /// 尝试把 `m` 挪到 `(to_x, to_y)`。
    fn try_step_to(&mut self, m: Mover, to_x: isize, to_y: isize) -> bool {
        if !self.world.in_bounds(to_x, to_y) {
            return false;
        }
        if !self.yields_to(to_x, to_y, m.kind) {
            return false;
        }
        if !self.swap(m.x, m.y, to_x, to_y) {
            return false;
        }
        // 源格与目标格都要标"已移动"：
        // 目标格是本格自己（防止一 tick 内被连续推动）；
        // 源格是刚被挤过来的那格材质（它被推开了，不该在同 tick 里再动一次）。
        self.mark_moved(to_x, to_y);
        self.mark_moved(m.x, m.y);
        self.ws.stats.moved += 1;
        true
    }
}

// ---------------------------------------------------------------------------
// 装填 / 执行 / 写回
// ---------------------------------------------------------------------------

/// 把一个分块拷进私有副本。
fn load(world: &World, cx: usize, cy: usize, ws: &mut Workspace) {
    ws.x0 = cx * CHUNK_SIZE;
    ws.y0 = cy * CHUNK_SIZE;
    ws.w = world.chunk_w(cx);
    ws.h = world.chunk_h(cy);
    ws.cells.clear();
    ws.cells.reserve(ws.w * ws.h);
    for y in ws.y0..ws.y0 + ws.h {
        let start = world.index_of(ws.x0, y);
        ws.cells
            .extend_from_slice(&world.cells()[start..start + ws.w]);
    }
    ws.moved.reset(ws.w, ws.h);
    ws.outbox.clear();
    ws.stats = StepStats::default();
}

/// 在私有副本上跑完一个分块的全部规则。
fn run_chunk(world: &World, moved: &MovedMask, ws: &mut Workspace, tick: u64) {
    let x0 = ws.x0;
    let y0 = ws.y0;
    let (w, h) = (ws.w, ws.h);
    let mut ctx = Ctx {
        world,
        global_moved: moved,
        ws,
    };
    // 块内行从下往上。
    //
    // 行内方向逐行交替：不交替的话，一行里所有粉末都朝同一侧滑，
    // 整块沙会像被风吹一样横移——那是纯粹的扫描顺序伪影，不是物理。
    // 交替之后左右偏移在相邻行之间抵消。
    for y in (y0..y0 + h).rev() {
        if y % 2 == 0 {
            for x in x0..x0 + w {
                ctx.visit(x, y, tick);
            }
        } else {
            for x in (x0..x0 + w).rev() {
                ctx.visit(x, y, tick);
            }
        }
    }
}

/// 把一个分块的结果写回真世界。
///
/// 到这里已经是单线程了。两件事必须做对：
///
/// 1. **逐格比对之后才写**。副本里绝大多数格子跟原来一样，不比对就写会把整块
///    重新标脏，下一 tick 的跳块优化立刻失效——而"世界能不能静下来"是 M1 的硬契约。
/// 2. **脏标记一次算完**。一开始这里是逐格调用 [`World::mark_dirty`]，
///    每写一格就要跑一遍 3×3 的邻格循环；满负载时每 tick 十几万次写入 × 9，
///    实测吃掉三成 tick 时间。现在改成边扫边按格查 [`NEIGHBOR_MASK`] 攒一个
///    位掩码，扫完一次性标——**脏集合与逐格标记逐位相同**，这一点有测试钉着。
fn apply(
    world: &mut World,
    moved: &mut MovedMask,
    ws: &Workspace,
    tick: u64,
    stats: &mut StepStats,
) {
    let (cx, cy) = ((ws.x0 / CHUNK_SIZE) as isize, (ws.y0 / CHUNK_SIZE) as isize);
    let mut mask: u16 = 0;

    for dy in 0..ws.h {
        let y = ws.y0 + dy;
        let row = dy * ws.w;
        // "贴上/贴下"两位逐行算一次就够。
        let v = if dy == 0 {
            4usize
        } else if dy == ws.h - 1 {
            8
        } else {
            0
        };
        for dx in 0..ws.w {
            if !world.put_cell_quiet(ws.x0 + dx, y, ws.cells[row + dx]) {
                continue;
            }
            let mut key = v;
            if dx == 0 {
                key |= 1;
            } else if dx == ws.w - 1 {
                key |= 2;
            }
            mask |= NEIGHBOR_MASK[key];
        }
    }

    if mask & OWN != 0 {
        world.mark_chunk_dirty(cx, cy);
    }
    if mask & L != 0 {
        world.mark_chunk_dirty(cx - 1, cy);
    }
    if mask & R != 0 {
        world.mark_chunk_dirty(cx + 1, cy);
    }
    if mask & T != 0 {
        world.mark_chunk_dirty(cx, cy - 1);
    }
    if mask & B != 0 {
        world.mark_chunk_dirty(cx, cy + 1);
    }
    if mask & LT != 0 {
        world.mark_chunk_dirty(cx - 1, cy - 1);
    }
    if mask & RT != 0 {
        world.mark_chunk_dirty(cx + 1, cy - 1);
    }
    if mask & LB != 0 {
        world.mark_chunk_dirty(cx - 1, cy + 1);
    }
    if mask & RB != 0 {
        world.mark_chunk_dirty(cx + 1, cy + 1);
    }

    // 跨块写入落在别的分块里，数量少（只有贴边的那几格），逐格标脏就够了。
    for o in &ws.outbox {
        if world.cells()[o.idx] != o.cell {
            world.put_cell(o.x, o.y, o.cell);
        }
    }
    for &idx in &ws.moved.spill {
        moved.mark(idx, tick);
    }
    stats.visited += ws.stats.visited;
    stats.moved += ws.stats.moved;
    stats.reacted += ws.stats.reacted;
}

/// 一次移动尝试的主角：谁（[`Mover::kind`]）在哪儿、于哪个 tick 想动。
///
/// 打包成一个结构体而不是散成五个参数，是因为移动判定有两条路径
/// （直走 / 斜走、横向找平），两条都要完整的主角信息。
/// 散着传的时候每加一个字段就要改两处签名，而且极易错位——
/// 两个相邻的 `isize` 参数传反了编译器一句话都不会说。
#[derive(Debug, Clone, Copy)]
struct Mover {
    x: isize,
    y: isize,
    kind: MaterialKind,
    tick: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::CellOwner;
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

    /// 被有机体占住的格子对材质是**实心**的，哪怕它的材质字段是 `Empty`。
    ///
    /// 这是 M2 的地基：如果水能灌进生物体内，"一格非材质即细胞"就不成立了，
    /// 之后所有关于"细胞在哪儿、吃什么、被什么挡住"的判定都会错。
    #[test]
    fn materials_cannot_flow_into_occupied_cells() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 40, 64, 24, MaterialKind::Stone);
        w.fill_rect(0, 10, 64, 30, MaterialKind::Water);

        // 在水体正中占一个 3×3 的空腔，像一条泡在水里的鱼。
        let mut occupied = Vec::new();
        for y in 24..27isize {
            for x in 30..33isize {
                w.set(x, y, MaterialKind::Empty);
                w.set_owner(x, y, CellOwner::new(1, occupied.len() as u16, 0));
                occupied.push((x, y));
            }
        }

        let mut sim = MaterialSim::new();
        for _ in 0..300 {
            sim.step(&mut w);
            for &(x, y) in &occupied {
                assert_eq!(
                    w.get(x, y),
                    MaterialKind::Empty,
                    "water leaked into the organism at ({x}, {y})"
                );
                assert!(w.is_occupied(x, y));
            }
        }

        // 水被这个洞挡住，但世界仍然必须能静下来。
        assert_eq!(
            w.dirty_chunk_count(),
            0,
            "an occupied cell must not keep the world awake"
        );
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

    // -----------------------------------------------------------------------
    // 残骸分解
    // -----------------------------------------------------------------------

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
        // 给这些残骸"能量"：`flags == 0` 的残骸会被水分解成沙（v0.9 新增的规则），
        // 而这条测试要断言的是**移动**守恒，不是"分解不守恒"。
        for y in 96..102isize {
            for x in 100..106isize {
                w.set_flags(x, y, 25);
            }
        }
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

    /// **残块世界里，跳块也必须等于全扫。**
    ///
    /// 世界不再要求 64 对齐之后，最右一列和最下一行的分块是残块
    /// （20×20 的世界只有一块 20×20 的残块）。这条测试守的是那次改动的核心：
    /// `off()` 的步长必须用 `ws.w` 而不是 `CHUNK_SIZE`——用常数的话，
    /// 残块从第 2 行起整片错位，而错位**不会报错**，只会让右边缘和底边缘
    /// 出现一片没人设计过的物质。
    ///
    /// 尺寸刻意挑成两轴都不齐、且底部残块只剩 4 格高（100×68）：
    /// 石头地板铺在那 4 格上，所有下落的物质都要跨一次块边界才落得下去。
    ///
    /// ## 场景刻意是"安静的"
    ///
    /// 只用**沙**和**一洼水**，不用"沙块砸进浅水"那种激烈场景。理由不是
    /// 图省事：那种场景会暴露另一个**既有**的等价性缺口——脏块集合是
    /// tick 开始时算的，而同一 tick 内后面的相位能看到前面相位的写入，
    /// 于是一条 8 格的横向连锁能在 tick 中途跨进一个"本 tick 没被标脏"的块。
    /// 那和残块无关，混进来只会让这条测试指向两个方向。
    #[test]
    fn ragged_chunks_match_a_full_scan() {
        let build = || {
            let mut w = World::new(100, 68).unwrap();
            // 地板正好铺在最下面那个 4 格高的残块行上。
            w.fill_rect(0, 64, 100, 4, MaterialKind::Stone);
            // 沙落在右半边（残块列，宽 36），水在左半边自己摊平。
            w.fill_rect(70, 8, 20, 20, MaterialKind::Sand);
            w.fill_rect(10, 40, 12, 12, MaterialKind::Water);
            w
        };
        let mut skipping = build();
        let mut full = build();
        let mut sim_skip = MaterialSim::new();
        let mut sim_full = MaterialSim::new();

        for t in 0..300 {
            sim_skip.step(&mut skipping);
            full.mark_all_dirty();
            sim_full.step(&mut full);
            assert_eq!(
                skipping.hash(),
                full.hash(),
                "残块世界（100×68）跳块与全扫在第 {t} tick 分叉"
            );
        }
        // 世界必须真的动过：两边都静止的话，这条测试什么都没证明。
        assert_ne!(
            skipping.hash(),
            build().hash(),
            "300 tick 之后世界一格没变——这条测试没测到东西"
        );
        // 沙必须真的落到了残块行上，否则"跨块下落"这件事根本没发生。
        assert_eq!(
            skipping.get(80, 63),
            MaterialKind::Sand,
            "沙没有落到地板上"
        );
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

    // -----------------------------------------------------------------------
    // 并行
    // -----------------------------------------------------------------------

    /// **`apply` 的按块标脏必须与逐格 `mark_dirty` 逐位相同。**
    ///
    /// 这条测试是为一个真 bug 补的。第一版按块标脏把"有格子贴左"和"有格子贴上"
    /// 记成两个独立的布尔值，再取与去标左上块——可那两个条件可以由**两个不同的
    /// 格子**分别满足，于是多标了一块、多扫了一片本来该跳过的格子。
    ///
    /// 后果不是崩溃，是**世界跑出另一个终态**：真实地貌 `--seed 42` 的终态哈希
    /// 从 `0x1abf…` 变成了 `0x1923…`，平均活跃块从 9.5 涨到 10.2。
    /// 当时 134 条测试全绿——因为它们比的是"跑完之后的世界"，
    /// 多扫一块未必在 400 tick 内显形。所以这里直接比**脏集合**本身。
    #[test]
    fn chunk_dirty_marking_matches_the_per_cell_rule() {
        let mut w = World::new(192, 192).unwrap();
        w.fill_all(MaterialKind::Stone);
        w.clear_dirty();

        // 3×3 块的世界，目标块放正中间，八个方向与四个角都能标到。
        let mut ws = Workspace::new();
        load(&w, 1, 1, &mut ws);

        // 刻意让"贴左"和"贴上"落在**两个不同的格子**上——正是那个 bug 的形状。
        let changed = [
            (0usize, 0usize),                 // 左上角
            (CHUNK_SIZE - 1, 0),              // 右上角
            (0, CHUNK_SIZE - 1),              // 左下角
            (CHUNK_SIZE - 1, CHUNK_SIZE - 1), // 右下角
            (0, 20),                          // 只贴左
            (20, 0),                          // 只贴上
            (CHUNK_SIZE - 1, 40),             // 只贴右
            (40, CHUNK_SIZE - 1),             // 只贴下
            (30, 30),                         // 哪条边都不贴
        ];
        for &(dx, dy) in &changed {
            ws.cells[dy * CHUNK_SIZE + dx] = MaterialCell::new(MaterialKind::Sand);
        }

        let mut moved = MovedMask::new(w.cells().len());
        let mut stats = StepStats::default();
        apply(&mut w, &mut moved, &ws, 0, &mut stats);

        // 参考实现：老老实实逐格调 mark_dirty。
        let mut reference = World::new(192, 192).unwrap();
        reference.fill_all(MaterialKind::Stone);
        reference.clear_dirty();
        for &(dx, dy) in &changed {
            reference.mark_dirty(CHUNK_SIZE + dx, CHUNK_SIZE + dy);
        }

        for cy in 0..w.chunk_rows() {
            for cx in 0..w.chunk_cols() {
                assert_eq!(
                    w.is_chunk_dirty(cx, cy),
                    reference.is_chunk_dirty(cx, cy),
                    "分块 ({cx},{cy}) 的脏标记与逐格规则不一致"
                );
            }
        }
    }

    /// 自动选的线程数必须跟着活量走，不能闷头开满。
    ///
    /// 建一个线程约 30 µs。相位里只有一两块可扫的时候，多开一个工人
    /// 分担的活还不如建它的开销——**线程越多反而越慢**。
    /// 而默认值取的是 `available_parallelism()`，也就是默认给最差的配置。
    #[test]
    fn auto_threads_follow_the_workload() {
        // 128² = 2×2 块，四个相位各 1 块 ⇒ 自动模式应当退回单线程。
        let mut small = floored(128, 128);
        small.mark_all_dirty();
        let mut sim = MaterialSim::new();
        sim.step(&mut small);
        assert_eq!(sim.stats().threads, 1, "相位里只有 1 块，不该建线程");

        // 512² = 8×8 块，四个相位各 16 块 ⇒ 上限是 8（每工人至少 2 块）。
        let mut big = floored(512, 512);
        big.mark_all_dirty();
        let mut sim = MaterialSim::new();
        sim.step(&mut big);
        let cap = 16 / MIN_CHUNKS_PER_WORKER;
        let expect = sim.threads().min(cap).max(1);
        assert_eq!(
            sim.stats().threads,
            expect,
            "512² 每相位 16 块，自动线程数应当收敛到 {expect}"
        );
    }

    /// 显式指定的线程数不被启发式压掉——调参的人得能自己压过它。
    #[test]
    fn explicit_thread_count_is_not_capped_by_the_heuristic() {
        let mut w = floored(128, 128);
        w.mark_all_dirty();
        let mut sim = MaterialSim::with_threads(4);
        sim.step(&mut w);
        assert_eq!(sim.stats().threads, 4, "显式 4 线程就该是 4 线程");
    }

    /// **扫描顺序是这个世界定义的一部分，不是实现细节。**
    ///
    /// 这条是黄金测试：钉住"种子 7、128²、200 tick"之后的整张网格。
    ///
    /// 加它的原因是一次真事故——把"自下而上逐块扫"改成"按棋盘相位分组扫"
    /// （并行必须这么分组，否则相邻块会互相写）之后，世界演化的路径就变了：
    /// tick 1 的移动次数从 10316 掉到 6100，终态哈希整个换掉。
    /// 当时所有测试都是绿的，因为它们比的都是"同一份代码跑两遍一样不一样"，
    /// 而不是"和昨天那个世界一样不一样"。物理结果没坏（水面线、陆地分段、
    /// 沙的位移、静止 tick 全都对得上），但**世界确实换了**。
    ///
    /// 所以把终态钉死。以后谁再动扫描顺序、相位划分、或者让某个格子
    /// 晚一个 tick 才被看到，这条会立刻红。
    ///
    /// **v0.9 更新过一次**：加了"无能量残骸被水分解成沙"的规则（`sweep_remains`），
    /// 世界演化的路径因此改变，哈希从 `0xcb31_2891_6747_6c16` 换成了下面这个。
    /// 这次变化是**有意的规则改动**，不是扫描顺序被动了——物理结果依然自洽
    /// （塌稳 tick 不变、水面线不变）。
    ///
    /// **v0.30 又换了一次**：地形生成从"海床漂移 + 几座钟形岛"换成
    /// fBm 高度场 + 热力侵蚀（`worldgen`），初始网格整个不同了，所以
    /// 200 tick 之后的终态必然不同。**物理规则一个字没动**——
    /// 这次变的是输入，不是扫描顺序。
    ///
    /// **v0.33 再换一次**：海床深度从"噪声幅度"改成"离岸距离"
    /// （`SEA_SLOPE`），512² 里海从 71 格深变成 276 格深、水占世界
    /// 从 2.4% 涨到 14.9%。同样是**输入变了**：物理规则（沙往下掉、
    /// 水找平、反应表）一个字没动。
    #[test]
    fn the_scan_order_is_part_of_the_world() {
        let mut w = World::new(128, 128).unwrap();
        sea_and_islands(&mut w, &mut Rng::new(7));
        let mut sim = MaterialSim::with_threads(1);
        for _ in 0..200 {
            sim.step(&mut w);
        }
        assert_eq!(
            w.hash(),
            0x52ba_3a1c_07a5_fc3d,
            "扫描顺序或规则变了：种子 7 的 128² 世界在 200 tick 后不再是原来那个"
        );
        assert_eq!(w.dirty_chunk_count(), 0, "这个世界应当已经塌稳");
    }

    /// **并行的验收锚点：线程数不影响结果。**
    ///
    /// 这条能成立不是靠"多跑几次都没出事"，而是因为调度器把每个分块拷成
    /// 私有副本、相位之间顺序写回——线程之间没有任何共享的可变状态。
    /// 但"构造上应该对"和"实际上对"之间隔着一整个调度器：相位划分、
    /// 跨块写入的排队与写回、掩码的局部/全局划分，任何一处写错都会在这里
    /// 以"1 线程和 8 线程跑出两个世界"的形式暴露出来。
    ///
    /// 世界必须**一直动**才有意义——早早静下来的话，后面的 tick 谁都扫不到东西，
    /// 测了等于没测。
    #[test]
    fn thread_count_does_not_change_the_world() {
        let build = || {
            let mut w = World::new(256, 256).unwrap();
            sea_and_islands(&mut w, &mut Rng::new(2026));
            // 天上撒沙、水里加水、峰顶点火——保证活跃分块长期铺满四个相位。
            w.fill_rect(0, 0, 256, 3, MaterialKind::Sand);
            w.fill_rect(60, 40, 40, 30, MaterialKind::Water);
            w.set(128, 10, MaterialKind::Fire);
            w
        };

        let mut serial = build();
        let mut sim_serial = MaterialSim::with_threads(1);
        let mut parallel = build();
        let mut sim_parallel = MaterialSim::with_threads(8);

        assert_eq!(serial.hash(), parallel.hash(), "初始世界就不一致");

        for t in 0..400 {
            sim_serial.step(&mut serial);
            sim_parallel.step(&mut parallel);
            assert_eq!(
                serial.hash(),
                parallel.hash(),
                "第 {t} tick 分叉：1 线程与 8 线程跑出了两个世界"
            );
        }
    }

    /// 相位划分必须**不重不漏**，而且同相位内任意两块都不相邻。
    ///
    /// 漏掉一块 = 那一块永远不被扫描；重复一块 = 同一格在一个 tick 里
    /// 被处理两次。两者都不会崩，只会让世界悄悄跑偏。
    /// "同相位内不相邻"则是并行安全的**全部来源**——它一旦不成立，
    /// 并行就会变成两个线程偶尔写同一格，一个只在特定地图上偶发、
    /// 换台机器就复现不出来的 bug。
    #[test]
    fn phases_partition_chunks_and_keep_neighbours_apart() {
        const N: usize = 6;
        let mut count = vec![0u32; PHASE_ORDER.len()];
        for cy in 0..N {
            for cx in 0..N {
                let p = phase_index(cx, cy);
                assert!(p < PHASE_ORDER.len());
                assert_eq!(PHASE_ORDER[p], (cx % 2, cy % 2), "相位归属不对");
                count[p] += 1;
            }
        }
        assert_eq!(count.iter().sum::<u32>(), (N * N) as u32, "有分块没被分配");

        for p in 0..PHASE_ORDER.len() {
            let chunks: Vec<(usize, usize)> = (0..N)
                .flat_map(|cy| (0..N).map(move |cx| (cx, cy)))
                .filter(|&(cx, cy)| phase_index(cx, cy) == p)
                .collect();
            for (i, &(ax, ay)) in chunks.iter().enumerate() {
                for &(bx, by) in &chunks[i + 1..] {
                    let (dx, dy) = (ax.abs_diff(bx), ay.abs_diff(by));
                    assert!(
                        dx >= 2 || dy >= 2,
                        "相位 {p} 里的 ({ax},{ay}) 与 ({bx},{by}) 相邻，并行会撞车"
                    );
                }
            }
        }
    }

    /// 跨过分块边界的移动，同样受"一 tick 只动一格"约束。
    ///
    /// 同一分块内部的标记是线程局部的，跨块的标记要走 `spill` 汇入全局掩码。
    /// 少走这一步的话，一格气会在一个 tick 里连穿两块——而且只在正好贴着
    /// 分块边界时发生，肉眼看地图完全看不出来。
    #[test]
    fn gas_crossing_a_chunk_border_still_moves_one_cell_per_tick() {
        // `y = CHUNK_SIZE` 正好是分块边界；从这一格上浮一格就跨块了。
        let mut w = World::new(128, 128).unwrap();
        w.set(32, CHUNK_SIZE as isize, MaterialKind::Gas);
        let mut sim = MaterialSim::with_threads(1);

        sim.step(&mut w);
        assert_eq!(
            w.get(32, CHUNK_SIZE as isize - 1),
            MaterialKind::Gas,
            "跨块上浮必须只走一格"
        );
        assert_eq!(w.get(32, CHUNK_SIZE as isize), MaterialKind::Empty);

        // 再走一格，同样只走一格——说明标记确实跨块传过去了。
        sim.step(&mut w);
        assert_eq!(w.get(32, CHUNK_SIZE as isize - 2), MaterialKind::Gas);
    }

    /// 世界平静下来之后，分块脏标记必须能**清空**。
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

    // -----------------------------------------------------------------------
    // 基准
    // -----------------------------------------------------------------------

    /// 往最上面一行撒沙。
    fn rain(w: &mut World) {
        for x in 0..w.width() {
            w.set(x as isize, 0, MaterialKind::Sand);
        }
    }

    /// 把最底下那层沙抽走，形成一条"传送带"，让世界永远不静止。
    fn drain(w: &mut World) {
        let y = w.height() as isize - 2;
        for x in 0..w.width() {
            let x = x as isize;
            if w.get(x, y) == MaterialKind::Sand {
                w.set(x, y, MaterialKind::Empty);
            }
        }
    }

    /// **性能基准：一个永远不静止的世界。**
    ///
    /// 静止世界跑得快是个陷阱——SPEC 第 2.4 节那套"活跃格子比例"的说法
    /// 在真实世界里长期成立，但一旦玩家往图里倒一桶水，成本立刻回到满负载。
    /// 并行调度要面对的正是这个负载，所以基准必须**逼着世界动**：
    /// 顶上撒沙、底下抽沙，形成一条传送带，活跃格子比例长期接近 100%。
    ///
    /// ```text
    /// cargo test -p vivarium-core --release bench_ -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "基准，手动运行"]
    fn bench_busy_world() {
        use std::time::Instant;

        const W: usize = 512;
        const TICKS: usize = 1000;

        // 每个线程数都从同一个初始世界出发，跑同一段时长。
        // 只有起点一致，横着比才有意义。
        let build = || {
            let mut w = World::new(W, W).unwrap();
            w.fill_rect(0, W as isize - 1, W, 1, MaterialKind::Stone);
            w
        };

        println!("沙雨传送带 {W}² × {TICKS} tick（活跃格子接近 100%）");
        println!(
            "{:>8}  {:>10}  {:>9}  {:>6}  {:>8}",
            "线程", "ms/tick", "加速比", "实到", "hash"
        );

        let mut baseline = 0.0f64;
        for &threads in &[1usize, 2, 4, 8, 16, 0] {
            let mut w = build();
            // 0 表示"让模拟器自己决定"，也就是游戏和 `sim` 二进制的默认路径。
            let mut sim = if threads == 0 {
                MaterialSim::new()
            } else {
                MaterialSim::with_threads(threads)
            };

            // 预热：把管道灌满，否则前几十 tick 只是在等沙落到底。
            for _ in 0..200 {
                rain(&mut w);
                drain(&mut w);
                sim.step(&mut w);
            }

            let t0 = Instant::now();
            let mut visited = 0usize;
            for _ in 0..TICKS {
                rain(&mut w);
                drain(&mut w);
                sim.step(&mut w);
                visited += sim.stats().visited;
            }
            let dt = t0.elapsed();
            let ms = dt.as_secs_f64() * 1000.0 / TICKS as f64;
            if threads == 1 {
                baseline = ms;
            }
            let label = if threads == 0 {
                "自动".to_string()
            } else {
                threads.to_string()
            };
            println!(
                "{label:>8}  {ms:>10.3}  {:>8.2}x  {:>6}  {:#x}",
                baseline / ms,
                sim.stats().threads,
                w.hash()
            );
            if threads == 1 {
                println!(
                    "{:>8}  （平均每 tick 访问 {:.0} 格，活跃分块 {}/{})",
                    "",
                    visited as f64 / TICKS as f64,
                    sim.stats().active_chunks,
                    w.chunk_count()
                );
            }
        }
    }

    /// 把一 tick 拆成三段量：装填 / 跑规则 / 写回。
    ///
    /// 并行只能加速中间那一段——装填和写回是串行的。所以这个拆分直接给出
    /// 加速比的上限（Amdahl）。不量就调并行，等于闭着眼睛调。
    ///
    /// ```text
    /// cargo test -p vivarium-core --release bench_chunk -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "基准，手动运行"]
    fn bench_chunk_overhead() {
        use std::time::Instant;

        const W: usize = 512;
        const REPEAT: usize = 200;

        let mut w = World::new(W, W).unwrap();
        w.fill_rect(0, W as isize - 1, W, 1, MaterialKind::Stone);
        let mut sim = MaterialSim::with_threads(1);
        for _ in 0..200 {
            rain(&mut w);
            drain(&mut w);
            sim.step(&mut w);
        }

        let cols = w.chunk_cols();
        let rows = w.chunk_rows();
        let chunks: Vec<(usize, usize)> = (0..rows)
            .flat_map(|cy| (0..cols).map(move |cx| (cx, cy)))
            .collect();

        // 1) 纯装填：64 块全部拷一遍。
        let mut ws = Workspace::new();
        let mut load_time = 0.0f64;
        for _ in 0..REPEAT {
            let t0 = Instant::now();
            for &(cx, cy) in &chunks {
                load(&w, cx, cy, &mut ws);
            }
            load_time += t0.elapsed().as_secs_f64();
        }

        // 2) 纯写回：副本与原世界逐格相同，所以只有比对、没有写入。
        let mut moved = MovedMask::new(w.cells().len());
        let mut stats = StepStats::default();
        let mut apply_time = 0.0f64;
        for _ in 0..REPEAT {
            let t0 = Instant::now();
            for &(cx, cy) in &chunks {
                load(&w, cx, cy, &mut ws);
                apply(&mut w, &mut moved, &ws, 0, &mut stats);
            }
            apply_time += t0.elapsed().as_secs_f64();
        }
        apply_time -= load_time;

        // 3) 一整 tick 的实测（与 bench_busy 同场景）。
        let mut w2 = World::new(W, W).unwrap();
        w2.fill_rect(0, W as isize - 1, W, 1, MaterialKind::Stone);
        let mut sim2 = MaterialSim::with_threads(1);
        for _ in 0..200 {
            rain(&mut w2);
            drain(&mut w2);
            sim2.step(&mut w2);
        }
        let t0 = Instant::now();
        for _ in 0..REPEAT {
            rain(&mut w2);
            drain(&mut w2);
            sim2.step(&mut w2);
        }
        let total = t0.elapsed().as_secs_f64() / REPEAT as f64;

        let load_ms = load_time / REPEAT as f64 * 1000.0;
        let apply_ms = apply_time / REPEAT as f64 * 1000.0;
        let total_ms = total * 1000.0;
        let rules_ms = total_ms - load_ms - apply_ms;
        println!("512² 满负载一 tick 的三段拆分：");
        println!(
            "  装填（串行）  {load_ms:>7.3} ms  {:>5.1}%",
            load_ms / total_ms * 100.0
        );
        println!(
            "  写回（串行）  {apply_ms:>7.3} ms  {:>5.1}%",
            apply_ms / total_ms * 100.0
        );
        println!(
            "  规则（可并行）{rules_ms:>7.3} ms  {:>5.1}%",
            rules_ms / total_ms * 100.0
        );
        println!("  合计          {total_ms:>7.3} ms");
        println!(
            "  串行段占比 {:.1}% ⇒ 无限线程的理论加速上限 {:.2}x",
            (load_ms + apply_ms) / total_ms * 100.0,
            total_ms / (load_ms + apply_ms)
        );
    }

    /// 只测**规则计算本身**能不能并行扩展——把调度器整个绕过去。
    ///
    /// 每块预先装填到自己的副本里，然后纯粹比较"1 个线程跑 64 块"与
    /// "N 个线程各跑一段"的耗时。这一条能分清两件事：
    ///
    /// - 如果这里能扩展，而完整 tick 不能，那问题一定在调度（锁、建线程、屏障）；
    /// - 如果这里也扩展不动，那是内存带宽或缓存的问题，调调度器没有意义。
    ///
    /// ```text
    /// cargo test -p vivarium-core --release bench_rule -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "基准，手动运行"]
    fn bench_rule_scaling() {
        use std::time::Instant;

        const W: usize = 512;
        const REPEAT: usize = 200;

        let mut w = World::new(W, W).unwrap();
        w.fill_rect(0, W as isize - 1, W, 1, MaterialKind::Stone);
        let mut sim = MaterialSim::with_threads(1);
        for _ in 0..200 {
            rain(&mut w);
            drain(&mut w);
            sim.step(&mut w);
        }

        let cols = w.chunk_cols();
        let rows = w.chunk_rows();
        let coords: Vec<(usize, usize)> = (0..rows)
            .flat_map(|cy| (0..cols).map(move |cx| (cx, cy)))
            .collect();
        let moved = MovedMask::new(w.cells().len());

        // 每个分块一份预先装填好的副本，各线程只碰自己那一份。
        let mut tiles: Vec<Workspace> = Vec::new();

        let world_ref: &World = &w;
        let moved_ref: &MovedMask = &moved;

        println!("纯规则计算：{W}² 满负载，{} 块", coords.len());
        println!("{:>8}  {:>10}  {:>8}", "线程", "ms/遍", "加速比");
        let mut baseline = 0.0f64;

        for &threads in &[1usize, 2, 4, 8, 16] {
            // 每一轮都重新装填：规则会让副本里的材质塌下来，
            // 不重装的话后面几轮跑的是已经静止的世界，越测越快。
            tiles.clear();
            for &(cx, cy) in &coords {
                let mut ws = Workspace::new();
                load(&w, cx, cy, &mut ws);
                tiles.push(ws);
            }

            let t0 = Instant::now();
            for _ in 0..REPEAT {
                if threads == 1 {
                    for ws in tiles.iter_mut() {
                        run_chunk(world_ref, moved_ref, ws, 7);
                    }
                } else {
                    let per = tiles.len().div_ceil(threads).max(1);
                    std::thread::scope(|s| {
                        for group in tiles.chunks_mut(per) {
                            s.spawn(move || {
                                for ws in group.iter_mut() {
                                    run_chunk(world_ref, moved_ref, ws, 7);
                                }
                            });
                        }
                    });
                }
            }
            let dt = t0.elapsed().as_secs_f64() / REPEAT as f64 * 1000.0;
            if threads == 1 {
                baseline = dt;
            }
            println!("{threads:>8}  {dt:>10.3}  {:>8.2}x", baseline / dt);
        }
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
