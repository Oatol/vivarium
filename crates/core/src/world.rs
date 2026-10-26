//! 世界网格：分块 + 脏标记 + 确定性哈希。
//!
//! ## 性能认知（SPEC 第 2.4 节）
//!
//! 落沙层的成本取决于**活跃格子比例**，不是网格大小。
//! 1024² 里如果只有 3% 的格子是动的，成本约等于 256² 全动。
//! 所以"大网格 + 稀疏活跃"是可行的——**前提是保证大部分区域真的静止**。
//! 分块脏标记就是为这个前提服务的：一块内所有格子一个 tick 都没变，下一 tick 整块跳过。
//!
//! ## 确定性
//!
//! [`World::hash`] 给出网格的确定性指纹。SPEC 的验收断言依赖它：
//! - 同种子同 tick ⇒ 哈希一致
//! - 切换 Pixel / Ascii 渲染器后 ⇒ 哈希不变

use std::fmt;

use crate::material::MaterialKind;

/// 分块边长。与 Noita 一致——这个尺寸在缓存局部性与调度粒度之间平衡得最好。
pub const CHUNK_SIZE: usize = 64;

/// 单格数据。
///
/// 保持 2 字节：`kind` 是 `repr(u8)` 枚举，`flags` 预留给"本 tick 已移动过"之类的标记。
/// 512² 的世界因此只占 512 KiB，整个网格能舒服地待在 L2 缓存附近。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MaterialCell {
    /// 材质类型。
    pub kind: MaterialKind,
    /// 位标记，当前未使用。
    pub flags: u8,
}

impl MaterialCell {
    /// 新建一个只有材质类型的格子。
    #[inline]
    pub const fn new(kind: MaterialKind) -> Self {
        Self { kind, flags: 0 }
    }
}

/// 一格的占用者：属于哪个有机体的第几个细胞。
///
/// `organism == 0` 表示这一格没有主人。用 0 当哨兵而不是 `Option<(u32, u16)>`，
/// 是因为 `Option` 的 niche 优化在这个形状上不生效（`u32` 没有空位），
/// 而"没有主人"本来就是这张表的一种取值，不是缺失。
///
/// 这张表与材质层**互斥**（SPEC §2.1）：一格要么是材质，要么属于某个细胞。
/// 细胞的材质字段是 `Empty`，但生物体内部不是空位——所有"能不能进这一格"的
/// 判定都必须问 [`World::yields_to`]，而不是只看材质。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellOwner {
    /// 有机体编号，0 表示无主。
    pub organism: u32,
    /// 该格是这个有机体的第几个细胞。
    pub cell: u16,
    /// 这一格细胞的**细胞器位掩码**（`crate::organelle::Organelles`）。
    ///
    /// 渲染层靠它决定画什么形状、上什么颜色。之所以存一份在占格表里，
    /// 而不是让渲染器去查生命层：渲染器只拿得到 `&World`，为了知道
    /// "这一格带哪些细胞器"而让它持有整个 `LifeSim`，是把两层焊死。
    ///
    /// **v0.41 从 `kind: u8` 换成 `organelles: u16`。** 原来存的是
    /// "代表类型"，而颜色一旦要按细胞器混（叶绿体绿 + 气囊淡蓝 = 薄荷），
    /// 那个有损的降级就不够用了：`叶绿体 + 气囊` 的代表类型是 `Producer`，
    /// 按类型上色的话它和纯生产者一模一样。
    ///
    /// 换成 `u16` 之后结构体仍然是 8 字节（`u32 + u16 + u16`），
    /// 一格都没多占。
    pub organelles: u16,
}

impl CellOwner {
    /// 无主。
    pub const NONE: Self = Self {
        organism: 0,
        cell: 0,
        organelles: 0,
    };

    /// 由有机体编号、细胞下标与细胞器位掩码构造。
    #[inline]
    pub const fn new(organism: u32, cell: u16, organelles: u16) -> Self {
        Self {
            organism,
            cell,
            organelles,
        }
    }

    /// 这一格是否被占用。
    #[inline]
    pub const fn is_some(self) -> bool {
        self.organism != 0
    }
}

/// 世界构造失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldError {
    /// 尺寸为 0。
    ZeroSize,
    /// 尺寸超出上限。
    ///
    /// 上限不是为了省内存，是为了**在构造时就拦住**——一张 20000×20000 的
    /// 世界会先吃掉 8 GB 再被显卡拒绝，而那时候报错的位置已经离用户的操作很远了。
    TooLarge { width: usize, height: usize, max: usize },
}

impl fmt::Display for WorldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSize => write!(f, "world size must be non-zero"),
            Self::TooLarge {
                width,
                height,
                max,
            } => write!(f, "world size {width}x{height} exceeds the limit {max}"),
        }
    }
}

impl std::error::Error for WorldError {}

/// 世界单边最大格数。
///
/// 2000 是"内存 × 显卡纹理上限"两条约束里更紧的那条：2000² 的材质表 8 MB、
/// 世界纹理 16 MB，而多数适配器的 `max_texture_dimension_2d` 是 8192，
/// 所以纹理不成问题。真正要挡的是"手滑把滑块拖到一个天文数字"。
pub const MAX_WORLD_SIDE: usize = 2000;

/// [`World::mark_dirty`] 的横向晕影半径（格）。
///
/// 必须 **≥** 材质层一次移动能横跨的最大距离（`sim::LATERAL_REACH` = 8），
/// 否则"某格变了"无法传播到"离它 8 格之外、因此变得可动"的那一格。
/// 两者的关系在 `sim.rs` 里有编译期断言钉着。
pub const DIRTY_HALO_X: usize = 8;

/// [`World::mark_dirty`] 的纵向晕影半径（格）。
///
/// 一次移动纵跨 1 格，所以 1 就够。写在这里而不是硬编码，是为了让它和
/// 横向那个数字一起被看见——它们是同一个问题的两个方向，只改一个必然出错。
pub const DIRTY_HALO_Y: usize = 1;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 格子世界。
///
/// 布局是扁平的 `Vec<MaterialCell>`（行主序），而不是 `Vec<Vec<_>>`——
/// 连续内存对分块扫描和将来的 rayon 并行都很友好。
#[derive(Debug, Clone)]
pub struct World {
    width: usize,
    height: usize,
    chunk_cols: usize,
    chunk_rows: usize,
    cells: Vec<MaterialCell>,
    /// 每格的占用者，与 `cells` 同序。生命层独占这张表，材质层只读。
    owner: Vec<CellOwner>,
    dirty: Vec<bool>,
    dirty_count: usize,
    tick: u64,
    /// **水与有机体共格**（实验性物理内核，默认关）。
    ///
    /// 关着 = v0.54 及以前的模型：身体占住的那一格水被**删掉**（记成
    /// `Cell::wet` 的欠条），移动时靠 `swap` 把水顶到刚让出来的格子里。
    /// 两条路都会**搬运水**：删除让水少一格、swap 让水多一格，而它们在
    /// 世界里发生的**位置不同**——净效果就是一台持续运转的水泵
    /// （实测海平面以上悬着 27137 格水）。
    ///
    /// 开着 = 双层模型：水**留在原地**，身体叠在它上面；材质层跳过被占住的
    /// 格子，于是那格水既流不走、也灌不进。没有借、没有还、没有顶。
    ///
    /// 为什么这个开关住在 `World` 而不是 `LifeSim`：**材质层也要读它**
    /// （见 `sim::Ctx::visit`）。放在生命层会需要第二份副本，而两份副本
    /// 迟早会漂开——症状是"水在身体里流走了，但账上还说它被托着"。
    share_water: bool,
}

impl World {
    /// 创建空世界。
    ///
    /// ## 尺寸可以是任意值，不必是 [`CHUNK_SIZE`] 的整数倍
    ///
    /// 早先要求对齐，理由是分块扫描要"一整块一整块地拷"。那条约束是从
    /// **实现**里长出来的，却写进了**接口**——于是玩家想开一个 600×450 的世界
    /// 时，唯一的出路是偷偷把它凑成 640×512，而多出来的那一条会在画面上
    /// 出现、在水位统计里出现、在相机取景里出现。
    ///
    /// 现在分块允许"最后一行/列是残块"：`chunk_cols = ceil(width / CHUNK_SIZE)`，
    /// 每块的真实宽高由 [`World::chunk_w`] / [`World::chunk_h`] 给出。
    /// 代价是材质层不能再按常数拷贝整块，必须按残块尺寸拷贝——
    /// 那几行改完之后，"块"和"格"的关系重新变得干净。
    pub fn new(width: usize, height: usize) -> Result<Self, WorldError> {
        if width == 0 || height == 0 {
            return Err(WorldError::ZeroSize);
        }
        if width > MAX_WORLD_SIDE || height > MAX_WORLD_SIDE {
            return Err(WorldError::TooLarge {
                width,
                height,
                max: MAX_WORLD_SIDE,
            });
        }

        let chunk_cols = width.div_ceil(CHUNK_SIZE);
        let chunk_rows = height.div_ceil(CHUNK_SIZE);
        let chunk_count = chunk_cols * chunk_rows;

        Ok(Self {
            width,
            height,
            chunk_cols,
            chunk_rows,
            cells: vec![MaterialCell::default(); width * height],
            owner: vec![CellOwner::NONE; width * height],
            // 新建的世界整块都是脏的，第一个 tick 需要完整扫描一次。
            dirty: vec![true; chunk_count],
            dirty_count: chunk_count,
            tick: 0,
            share_water: false,
        })
    }

    /// 世界宽度（格）。
    #[inline]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// 世界高度（格）。
    #[inline]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// 已推进的 tick 数。
    #[inline]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// 分块列数。
    #[inline]
    pub const fn chunk_cols(&self) -> usize {
        self.chunk_cols
    }

    /// 分块行数。
    #[inline]
    pub const fn chunk_rows(&self) -> usize {
        self.chunk_rows
    }

    /// 分块总数。
    #[inline]
    pub const fn chunk_count(&self) -> usize {
        self.chunk_cols * self.chunk_rows
    }

    /// 第 `cx` 列分块的实际宽度（格）。
    ///
    /// 只有最右边那一列可能小于 [`CHUNK_SIZE`]——但材质层不能假设
    /// "只有最后一列"，它必须每块都问一次。写成特判会让"残块"变成
    /// 一个隐藏的边界条件，而隐藏的边界条件正是这套代码最怕的东西。
    #[inline]
    pub fn chunk_w(&self, cx: usize) -> usize {
        let x0 = cx * CHUNK_SIZE;
        (self.width - x0).min(CHUNK_SIZE)
    }

    /// 第 `cy` 行分块的实际高度（格）。见 [`World::chunk_w`]。
    #[inline]
    pub fn chunk_h(&self, cy: usize) -> usize {
        let y0 = cy * CHUNK_SIZE;
        (self.height - y0).min(CHUNK_SIZE)
    }

    /// 坐标是否落在世界内。接受 `isize` 以便邻域查询时直接传 `x - 1`。
    #[inline]
    pub fn in_bounds(&self, x: isize, y: isize) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height
    }

    /// 行主序索引。
    ///
    /// 公开出去是因为"本 tick 已移动"之类的辅助数组要和网格同序索引，
    /// 让它们各自重算一遍 `y * width + x` 迟早会有一处写错。
    #[inline]
    pub const fn index_of(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    /// 读取格子。越界返回 [`MaterialKind::Empty`]。
    #[inline]
    pub fn get(&self, x: isize, y: isize) -> MaterialKind {
        if self.in_bounds(x, y) {
            self.cells[self.index_of(x as usize, y as usize)].kind
        } else {
            MaterialKind::Empty
        }
    }

    /// 读取格子（不越界检查，调用方保证坐标合法）。
    #[inline]
    pub fn get_unchecked(&self, x: usize, y: usize) -> MaterialKind {
        self.cells[self.index_of(x, y)].kind
    }

    // -- 占格层（生命层） ---------------------------------------------------
    //
    // 材质层**不写**这张表，只通过 [`World::yields_to`] 读它。
    // 于是"一格非材质即细胞"这条互斥规则只有一个执行点。

    /// 读取某格的占用者。越界返回 [`CellOwner::NONE`]。
    #[inline]
    pub fn owner(&self, x: isize, y: isize) -> CellOwner {
        if self.in_bounds(x, y) {
            self.owner[self.index_of(x as usize, y as usize)]
        } else {
            CellOwner::NONE
        }
    }

    /// 这一格是否被有机体占用。
    #[inline]
    pub fn is_occupied(&self, x: isize, y: isize) -> bool {
        self.owner(x, y).is_some()
    }

    /// 设置某格的占用者。越界被忽略，值未变化时返回 `false`。
    ///
    /// 会标脏：占用状态的改变会让邻居"能不能流进来"跟着变
    /// （一格从空位变成生物体的一部分，旁边的水就再也进不来了）。
    pub fn set_owner(&mut self, x: isize, y: isize, owner: CellOwner) -> bool {
        if !self.in_bounds(x, y) {
            return false;
        }
        let idx = self.index_of(x as usize, y as usize);
        if self.owner[idx] == owner {
            return false;
        }
        self.owner[idx] = owner;
        self.mark_dirty(x as usize, y as usize);
        true
    }

    /// 清空某格的占用者。
    #[inline]
    pub fn vacate(&mut self, x: isize, y: isize) -> bool {
        self.set_owner(x, y, CellOwner::NONE)
    }

    /// 整张占用表，与 [`World::cells`] 同序。渲染器用它判断"这一格是材质还是细胞"。
    #[inline]
    pub fn owners(&self) -> &[CellOwner] {
        &self.owner
    }

    /// 材质层视角下，这一格能不能被 `mover` 挤开。
    ///
    /// 比 [`MaterialKind::displaceable_by`] 多一条：**被有机体占用的格子永远不让路**。
    /// 细胞的材质字段是 `Empty`，如果只问材质，水会理所当然地灌进生物体内——
    /// 而"一格非材质即细胞"是这套架构的地基（SPEC §2.1）。
    ///
    /// 材质层的所有让路判定都必须走这里，而不是自己拿 `get` 再比一遍。
    #[inline]
    pub fn yields_to(&self, x: isize, y: isize, mover: MaterialKind) -> bool {
        if self.is_occupied(x, y) {
            return false;
        }
        self.get(x, y).displaceable_by(mover)
    }

    /// 世界里被占用的格子总数。
    pub fn occupied_count(&self) -> usize {
        self.owner.iter().filter(|o| o.is_some()).count()
    }

    /// 水与有机体是否共格（见 [`World::share_water`] 字段的注释）。
    #[inline]
    pub const fn shares_water(&self) -> bool {
        self.share_water
    }

    /// 切换共格模型。
    ///
    /// **切换时整个世界标脏**：换掉的是一套物理规则，原本"已经稳定下来"
    /// 的块在新规则下未必还稳定（关掉时那些被身体托着的水突然自由了，
    /// 该塌的得让它塌）。只标一次，代价是"切换后的一两个 tick 全扫"，
    /// 比"切换后水永远悬在那儿"便宜得多。
    ///
    /// 值没变时不动任何东西——这个函数会被界面每帧调用。
    pub fn set_share_water(&mut self, on: bool) {
        if self.share_water == on {
            return;
        }
        self.share_water = on;
        self.mark_all_dirty();
    }

    /// 读取格子的标记位。越界返回 0。    #[inline]
    pub fn flags(&self, x: isize, y: isize) -> u8 {
        if self.in_bounds(x, y) {
            self.cells[self.index_of(x as usize, y as usize)].flags
        } else {
            0
        }
    }

    /// 写入格子的标记位。越界被忽略，值未变化时不标脏。
    ///
    /// 标记位不改变画面，但会改变模拟结果，所以**必须**标脏——
    /// 否则一个"只剩寿命在倒计时"的火焰所在分块会被跳过，永远烧不完。
    pub fn set_flags(&mut self, x: isize, y: isize, flags: u8) -> bool {
        if !self.in_bounds(x, y) {
            return false;
        }
        let idx = self.index_of(x as usize, y as usize);
        if self.cells[idx].flags == flags {
            return false;
        }
        self.cells[idx].flags = flags;
        self.mark_dirty(x as usize, y as usize);
        true
    }

    /// 写入格子。值未变化时不标脏，这是脏标记有效的前提。
    ///
    /// 换材质时**清空标记位**：标记位是材质自己的状态（火的剩余寿命、
    /// 本 tick 是否已移动），换了材质它就失去意义，留着会污染新材质的行为。
    ///
    /// 越界写入被静默忽略——画笔拖到边缘时不该 panic。
    pub fn set(&mut self, x: isize, y: isize, kind: MaterialKind) -> bool {
        if !self.in_bounds(x, y) {
            return false;
        }
        let idx = self.index_of(x as usize, y as usize);
        if self.cells[idx].kind == kind {
            return false;
        }
        self.cells[idx].kind = kind;
        self.cells[idx].flags = 0;
        self.mark_dirty(x as usize, y as usize);
        true
    }

    /// 交换两格的全部内容（材质 + 标记位）。
    ///
    /// 材质层的移动**全部**通过它完成：粉末下坠、液体下沉、气体上浮都不是
    /// "把 A 复制到 B 再清空 A"，而是交换。原因有两个：
    ///
    /// - 目标格往往不是空的（沙落进水里、水沉进气里），复制会**凭空吞掉**那格材质，
    ///   世界里的物质总量就不再守恒，M1 的守恒断言立刻失效。
    /// - 标记位随材质一起搬走，"这一格本 tick 已经动过"才不会丢。
    ///
    /// 任一端越界、或两格内容完全相同，返回 `false` 且不标脏。
    pub fn swap(&mut self, x1: isize, y1: isize, x2: isize, y2: isize) -> bool {
        if !self.in_bounds(x1, y1) || !self.in_bounds(x2, y2) {
            return false;
        }
        let (x1u, y1u) = (x1 as usize, y1 as usize);
        let (x2u, y2u) = (x2 as usize, y2 as usize);
        let i = self.index_of(x1u, y1u);
        let j = self.index_of(x2u, y2u);
        if i == j || self.cells[i] == self.cells[j] {
            return false;
        }
        self.cells.swap(i, j);
        self.mark_dirty(x1u, y1u);
        self.mark_dirty(x2u, y2u);
        true
    }

    /// 直接写入整格（材质 + 标记位），内容变化时标脏。
    ///
    /// 与 [`World::set`] 的区别：`set` 换材质时会把标记位清成 0，
    /// 而这里写入的是一格**已经算好的完整状态**。材质层在分块私有副本上
    /// 已经把标记位算好了（火的剩余寿命、本 tick 是否动过），写回时必须
    /// 原样落地——否则火会被反复当成"刚生成"、寿命永远烧不完。
    pub fn put_cell(&mut self, x: usize, y: usize, cell: MaterialCell) -> bool {
        let idx = self.index_of(x, y);
        if self.cells[idx] == cell {
            return false;
        }
        self.cells[idx] = cell;
        self.mark_dirty(x, y);
        true
    }

    /// 写入一格，**不**标脏。
    ///
    /// 只给材质层的批量写回用：那边一次要写回整整一个分块，
    /// 每格都去跑一遍 [`World::mark_dirty`] 的 3×3 循环是纯浪费——
    /// 满负载时每 tick 十几万次写入 × 9 次邻格检查，实测能吃掉三成 tick 时间。
    /// 调用方必须自己用 [`World::mark_chunk_dirty`] 把该标的块补上。
    #[inline]
    pub fn put_cell_quiet(&mut self, x: usize, y: usize, cell: MaterialCell) -> bool {
        let idx = self.index_of(x, y);
        if self.cells[idx] == cell {
            return false;
        }
        self.cells[idx] = cell;
        true
    }

    /// 把一个分块标脏（按分块坐标，越界忽略）。
    ///
    /// 与 [`World::mark_dirty`] 的分工：`mark_dirty` 接收的是**格**坐标、
    /// 自动带 1 格晕影，适合零散写入；这个接收的是**块**坐标、不带晕影，
    /// 适合"我已经自己算好哪些块该标"的批量写回。
    #[inline]
    pub fn mark_chunk_dirty(&mut self, cx: isize, cy: isize) {
        if cx < 0 || cy < 0 {
            return;
        }
        let (cx, cy) = (cx as usize, cy as usize);
        if cx >= self.chunk_cols || cy >= self.chunk_rows {
            return;
        }
        let ci = cy * self.chunk_cols + cx;
        if !self.dirty[ci] {
            self.dirty[ci] = true;
            self.dirty_count += 1;
        }
    }

    /// 把矩形区域刷成同一种材质。画笔工具的底层操作。
    ///
    /// 区域会被裁剪到世界边界内。
    pub fn fill_rect(&mut self, x: isize, y: isize, w: usize, h: usize, kind: MaterialKind) {
        for dy in 0..h as isize {
            for dx in 0..w as isize {
                self.set(x + dx, y + dy, kind);
            }
        }
    }

    /// 把整个世界刷成同一种材质。
    pub fn fill_all(&mut self, kind: MaterialKind) {
        for cell in &mut self.cells {
            *cell = MaterialCell::new(kind);
        }
        self.dirty.iter_mut().for_each(|d| *d = true);
        self.dirty_count = self.chunk_count();
    }

    /// 标记某格**及其影响范围**所在的分块为脏。
    ///
    /// ## 为什么必须带上邻格
    ///
    /// 脏块的作用是"下一 tick 跳过完全没动过的区域"。但一个格子能不能动，
    /// 取决于它的邻居——(10, 10) 的水流走了，(10, 9) 的水才终于能往下走，
    /// 而 (10, 9) 自己一个字节都没变过。
    ///
    /// 只标变化格所在的分块，就会出现"这一格已经该动了，但它所在的分块
    /// 被判定为静止而跳过"，于是跳块扫描与全扫的结果**不一致**。
    ///
    /// ## 晕影为什么横向是 8 格而不是 1 格
    ///
    /// 这一条曾经是 1，而它是**错的**——错得很安静，跑了很久才被抓到。
    ///
    /// 液体找平一次能横跨 [`crate::sim::LATERAL_REACH`] = 8 格：水在 (52, 88)
    /// 看中了 (60, 88) 的空位，一 tick 就跳过去。于是"52 号格变了"这件事
    /// 会改变**60 号格**能不能动，而 60 号格自己一个字节都没变。
    /// 1 格晕影覆盖不到 8 格，跨分块时就会分叉：
    ///
    /// ```text
    /// tick 117  跳块：水停在 52、60         全扫：52→60→68→76 一路推过去
    /// ```
    ///
    /// 实测触发场景是"沙块砸进浅水"——被挤出来的水沿着水面横向连锁，
    /// 一次 tick 能串起四五段 8 格的跳跃，正好跨在分块边界上。
    /// 旧的 512² 演示世界里这种连锁很少跨块，所以一直没显形。
    ///
    /// 纵向仍然是 1：一次移动纵跨 1 格，所以"变了"只影响上下各一格。
    /// 两个数字的关系写成了编译期断言（见 `sim.rs`），改任何一边都会编译失败。
    #[inline]
    pub fn mark_dirty(&mut self, x: usize, y: usize) {
        // 以格为单位往外扩，再折算成分块——比"直接给分块坐标 ±1"
        // 精确得多，后者会在格子不贴边时白标一圈分块。
        for dy in -(DIRTY_HALO_Y as isize)..=(DIRTY_HALO_Y as isize) {
            for dx in -(DIRTY_HALO_X as isize)..=(DIRTY_HALO_X as isize) {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if !self.in_bounds(nx, ny) {
                    continue;
                }
                let ci = (ny as usize / CHUNK_SIZE) * self.chunk_cols + (nx as usize / CHUNK_SIZE);
                if !self.dirty[ci] {
                    self.dirty[ci] = true;
                    self.dirty_count += 1;
                }
            }
        }
    }

    /// 分块是否为脏。
    #[inline]
    pub fn is_chunk_dirty(&self, cx: usize, cy: usize) -> bool {
        self.dirty[cy * self.chunk_cols + cx]
    }

    /// 当前脏块数量。用来观察"活跃格子比例"——这是落沙层的真实性能指标。
    #[inline]
    pub const fn dirty_chunk_count(&self) -> usize {
        self.dirty_count
    }

    /// 清空所有脏标记。
    ///
    /// 材质层在每个 tick 的**开头**调用，于是本 tick 产生的脏标记
    /// 正好留给下一个 tick 使用。这条协议是自洽的：
    /// 第 N tick 结束时脏块集合 = "在第 N tick 里变化过的格子所在的分块"，
    /// 而第 N+1 tick 需要重新检查的正是这些分块。
    pub fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
        self.dirty_count = 0;
    }

    /// 把所有分块标为脏，即"下一个 tick 强制全网格扫描"。
    ///
    /// 两个用途：
    /// - 读档、时间倒带之后，脏块集合不可信，需要重新扫一遍；
    /// - 作为**测试锚点**：让模拟器在"跳块"与"全扫"两种模式下跑同一份初始世界，
    ///   逐 tick 比较哈希，就能直接证明跳块没有改变结果。
    ///   没有这条断言，跳块优化就只能靠"看起来没问题"来验收。
    pub fn mark_all_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = true);
        self.dirty_count = self.chunk_count();
    }

    /// 推进 tick 计数。
    ///
    /// 这里**不再**顺带清脏标记。M0 时 `step()` 把两件事捆在一起，
    /// 但材质层必须先**读**上一 tick 留下的脏块集合，再清空它开始本 tick——
    /// 捆在一起就没法在清空之前读到。推进与清空拆开，调用方按需要排序。
    #[inline]
    pub fn advance_tick(&mut self) {
        self.tick += 1;
    }

    /// 直接设定 tick 计数。
    ///
    /// 只有一个正当用途：**读档**。存档里带着它自己的 tick，而 `tick` 是
    /// 世界内容的一部分（`tick % 25` 之类的周期规则、标题栏、可复现性断言
    /// 都读它）。读回来从 0 重新数会让"存档那一刻发生了什么"永远对不上。
    #[inline]
    pub fn set_tick(&mut self, tick: u64) {
        self.tick = tick;
    }

    /// 统计某种材质的格子数。
    pub fn count_of(&self, kind: MaterialKind) -> usize {
        self.cells.iter().filter(|c| c.kind == kind).count()
    }

    /// 一次性统计所有材质的格子数。
    pub fn census(&self) -> [usize; MaterialKind::COUNT] {
        let mut out = [0usize; MaterialKind::COUNT];
        for cell in &self.cells {
            out[cell.kind.as_u8() as usize] += 1;
        }
        out
    }

    /// 网格的确定性指纹（FNV-1a，64 位）。
    ///
    /// SPEC 的可复现性断言直接建立在这个函数上，所以它**必须**是稳定且平台无关的——
    /// 不要换成 `DefaultHasher`，它的输出在不同 Rust 版本间不保证一致。
    pub fn hash(&self) -> u64 {
        let mut h = FNV_OFFSET_BASIS;
        for (cell, owner) in self.cells.iter().zip(&self.owner) {
            h ^= cell.kind.as_u8() as u64;
            h = h.wrapping_mul(FNV_PRIME);
            h ^= cell.flags as u64;
            h = h.wrapping_mul(FNV_PRIME);
            // 空占用不携带任何信息，跳过它。
            // 这一条同时保证了"M1 的世界"（没有任何有机体）哈希与加占格层之前
            // 逐位相同——黄金测试因此不必改，也说明这层是真的"只增不改"。
            if owner.is_some() {
                h ^= owner.organism as u64;
                h = h.wrapping_mul(FNV_PRIME);
                h ^= owner.cell as u64;
                h = h.wrapping_mul(FNV_PRIME);
                // 细胞器也要进哈希：同一个编号、同一个下标，换成另一种细胞
                // 就是另一个世界，而"世界变了哈希没变"会让回放对不上。
                h ^= owner.organelles as u64;
                h = h.wrapping_mul(FNV_PRIME);
            }
        }
        h
    }

    /// 只读访问整张网格，供渲染器与快照使用。
    #[inline]
    pub fn cells(&self) -> &[MaterialCell] {
        &self.cells
    }

    /// 读档：整张材质表一次性装回去。长度不符返回 `false`。
    ///
    /// 不走 [`World::set`] 一格一格写，理由是**成本**：`set` 会调
    /// [`World::mark_dirty`]，而那里要扩一圈 17×3 的晕影。512² 就是
    /// 26 万格 × 51 次写 = 1300 万次，2000² 直接上两亿——读一个档要好几秒，
    /// 而它做的事只是"把这些字节放到那张表里"。
    ///
    /// 装完之后**所有块都标脏**：读档等于换了一个世界，没有任何一块能假定
    /// 自己上一 tick 的状态还有意义。
    ///
    /// 占格表不动。它由生命层在 [`crate::save::decode`] 里逐个有机体
    /// [`crate::life::Organism::claim`] 重建——材质层不该知道细胞怎么摆。
    pub fn restore_cells(&mut self, cells: &[MaterialCell]) -> bool {
        if cells.len() != self.cells.len() {
            return false;
        }
        self.cells.copy_from_slice(cells);
        self.mark_all_dirty();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{CellOwner, World, WorldError, CHUNK_SIZE, DIRTY_HALO_X, MAX_WORLD_SIDE};
    use crate::material::MaterialKind;

    #[test]
    fn rejects_impossible_sizes() {
        assert_eq!(World::new(0, 0).unwrap_err(), WorldError::ZeroSize);
        assert_eq!(World::new(0, 64).unwrap_err(), WorldError::ZeroSize);
        assert_eq!(
            World::new(MAX_WORLD_SIDE + 1, 64).unwrap_err(),
            WorldError::TooLarge {
                width: MAX_WORLD_SIDE + 1,
                height: 64,
                max: MAX_WORLD_SIDE
            }
        );
    }

    /// 任意尺寸都能开——**包括不是 64 倍数的**。
    ///
    /// 这条曾经是反的（要求对齐）。改掉的理由是玩家要的是"600×450 的世界"，
    /// 而不是"640×512 的世界再告诉他凑合一下"。
    #[test]
    fn accepts_any_size() {
        for (w, h) in [(1, 1), (20, 20), (100, 70), (256, 512), (768, 728)] {
            let world = World::new(w, h).expect("任意尺寸都该能建");
            assert_eq!(world.width(), w);
            assert_eq!(world.height(), h);
            assert_eq!(world.chunk_cols(), w.div_ceil(CHUNK_SIZE));
            assert_eq!(world.chunk_rows(), h.div_ceil(CHUNK_SIZE));
            // 每块宽高加起来必须**刚好**等于世界宽高：多一格会读到界外，
            // 少一格会有一列永远不被扫描。
            let sum_w: usize = (0..world.chunk_cols()).map(|cx| world.chunk_w(cx)).sum();
            let sum_h: usize = (0..world.chunk_rows()).map(|cy| world.chunk_h(cy)).sum();
            assert_eq!(sum_w, w, "{w}x{h}: 分块宽度之和不对");
            assert_eq!(sum_h, h, "{w}x{h}: 分块高度之和不对");
        }
    }

    #[test]
    fn accepts_chunk_aligned_size() {
        let w = World::new(256, 512).unwrap();
        assert_eq!(w.chunk_cols(), 4);
        assert_eq!(w.chunk_rows(), 8);
        assert_eq!(w.chunk_count(), 32);
        assert_eq!(w.chunk_w(0), CHUNK_SIZE);
        assert_eq!(w.chunk_h(7), CHUNK_SIZE);
    }

    /// 残块尺寸：最后一行/列只到自己该到的地方。
    #[test]
    fn ragged_edge_chunks_report_their_real_size() {
        let w = World::new(100, 70).unwrap();
        assert_eq!(w.chunk_cols(), 2);
        assert_eq!(w.chunk_rows(), 2);
        assert_eq!(w.chunk_w(0), 64);
        assert_eq!(w.chunk_w(1), 36);
        assert_eq!(w.chunk_h(0), 64);
        assert_eq!(w.chunk_h(1), 6);
    }

    #[test]
    fn out_of_bounds_reads_are_empty() {
        let w = World::new(64, 64).unwrap();
        assert_eq!(w.get(-1, 0), MaterialKind::Empty);
        assert_eq!(w.get(0, -1), MaterialKind::Empty);
        assert_eq!(w.get(64, 0), MaterialKind::Empty);
        assert_eq!(w.get(0, 64), MaterialKind::Empty);
    }

    #[test]
    fn out_of_bounds_writes_are_ignored() {
        let mut w = World::new(64, 64).unwrap();
        assert!(!w.set(-1, 0, MaterialKind::Stone));
        assert!(!w.set(999, 999, MaterialKind::Stone));
        assert_eq!(w.hash(), World::new(64, 64).unwrap().hash());
    }

    #[test]
    fn redundant_write_does_not_dirty() {
        let mut w = World::new(64, 64).unwrap();
        w.clear_dirty();
        assert!(!w.set(10, 10, MaterialKind::Empty)); // 本来就是 Empty
        assert_eq!(w.dirty_chunk_count(), 0);
        assert!(w.set(10, 10, MaterialKind::Stone));
        assert_eq!(w.dirty_chunk_count(), 1);
    }

    #[test]
    fn dirty_marking_is_per_chunk() {
        let mut w = World::new(128, 128).unwrap();
        w.clear_dirty();
        // 同一块内写多次，只应标脏一次。
        // 坐标都离块边界足够远，晕影不会溢到邻块去——那是另一个测试的事。
        w.set(10, 10, MaterialKind::Stone);
        w.set(11, 11, MaterialKind::Stone);
        w.set(40, 40, MaterialKind::Stone);
        assert_eq!(w.dirty_chunk_count(), 1);
        // 跨到相邻块，应变成两块。
        w.set(70, 10, MaterialKind::Stone);
        assert_eq!(w.dirty_chunk_count(), 2);
        assert!(w.is_chunk_dirty(1, 0));
    }

    #[test]
    fn same_seed_same_hash() {
        let mut a = World::new(256, 256).unwrap();
        let mut b = World::new(256, 256).unwrap();
        for (w, off) in [(&mut a, 0isize), (&mut b, 0isize)] {
            w.fill_rect(10 + off, 10, 32, 32, MaterialKind::Sand);
            w.fill_rect(100, 50, 16, 16, MaterialKind::Water);
            w.advance_tick();
        }
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn different_content_different_hash() {
        let mut a = World::new(64, 64).unwrap();
        let mut b = World::new(64, 64).unwrap();
        a.set(5, 5, MaterialKind::Sand);
        b.set(5, 5, MaterialKind::Water);
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn census_matches_manual_count() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 0, 10, 10, MaterialKind::Stone);
        w.fill_rect(20, 20, 4, 4, MaterialKind::Water);
        let c = w.census();
        assert_eq!(c[MaterialKind::Stone.as_u8() as usize], 100);
        assert_eq!(c[MaterialKind::Water.as_u8() as usize], 16);
        assert_eq!(c[MaterialKind::Empty.as_u8() as usize], 64 * 64 - 116);
    }

    #[test]
    fn clear_dirty_and_advance_tick_are_separate() {
        let mut w = World::new(64, 64).unwrap();
        w.set(1, 1, MaterialKind::Stone);
        assert_eq!(w.dirty_chunk_count(), 1);
        // 只推进 tick：脏标记必须原样留着，材质层还要读它。
        w.advance_tick();
        assert_eq!(w.tick(), 1);
        assert_eq!(w.dirty_chunk_count(), 1);
        // 再清空。
        w.clear_dirty();
        assert_eq!(w.dirty_chunk_count(), 0);
        assert_eq!(w.tick(), 1);
    }

    #[test]
    fn chunk_size_is_what_we_think_it_is() {
        assert_eq!(CHUNK_SIZE, 64);
    }

    // -----------------------------------------------------------------------
    // 交换
    // -----------------------------------------------------------------------

    #[test]
    fn swap_moves_both_cells() {
        let mut w = World::new(64, 64).unwrap();
        w.set(10, 10, MaterialKind::Sand);
        assert!(w.swap(10, 10, 10, 11));
        assert_eq!(w.get(10, 10), MaterialKind::Empty);
        assert_eq!(w.get(10, 11), MaterialKind::Sand);
    }

    /// 交换是**守恒**的：世界里的材质总量不变。
    ///
    /// 这条直接支撑 M1 的守恒断言。用复制实现的移动会在这里红：
    /// 沙落进水里会把那格水凭空吞掉。
    #[test]
    fn swap_conserves_every_material() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 0, 8, 8, MaterialKind::Sand);
        w.fill_rect(0, 8, 8, 8, MaterialKind::Water);
        let before = w.census();
        // 把整个沙块和水块逐格对调一遍。
        for y in 0..8 {
            for x in 0..8 {
                w.swap(x, y, x, y + 8);
            }
        }
        assert_eq!(w.census(), before);
    }

    #[test]
    fn swap_carries_flags_along() {
        let mut w = World::new(64, 64).unwrap();
        w.set(5, 5, MaterialKind::Fire);
        w.set_flags(5, 5, 9);
        w.swap(5, 5, 5, 6);
        assert_eq!(w.get(5, 6), MaterialKind::Fire);
        assert_eq!(w.flags(5, 6), 9, "flags must travel with the material");
        assert_eq!(w.flags(5, 5), 0);
    }

    #[test]
    fn swapping_identical_cells_is_a_noop() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 0, 4, 4, MaterialKind::Water);
        w.clear_dirty();
        assert!(
            !w.swap(0, 0, 1, 0),
            "two identical water cells need no swap"
        );
        assert_eq!(w.dirty_chunk_count(), 0);
    }

    #[test]
    fn swap_with_itself_is_a_noop() {
        let mut w = World::new(64, 64).unwrap();
        w.set(3, 3, MaterialKind::Sand);
        w.clear_dirty();
        assert!(!w.swap(3, 3, 3, 3));
        assert_eq!(w.get(3, 3), MaterialKind::Sand);
        assert_eq!(w.dirty_chunk_count(), 0);
    }

    #[test]
    fn swap_out_of_bounds_is_rejected() {
        let mut w = World::new(64, 64).unwrap();
        w.set(0, 0, MaterialKind::Sand);
        assert!(!w.swap(0, 0, -1, 0));
        assert!(!w.swap(0, 0, 64, 0));
        assert_eq!(w.get(0, 0), MaterialKind::Sand);
    }

    #[test]
    fn swap_across_chunks_dirties_both() {
        let mut w = World::new(128, 64).unwrap();
        w.clear_dirty();
        w.set(63, 10, MaterialKind::Sand); // 会在两块上标脏（晕影）
        w.clear_dirty();
        w.swap(63, 10, 64, 10); // 跨块
        assert!(w.is_chunk_dirty(0, 0));
        assert!(w.is_chunk_dirty(1, 0));
    }

    // -----------------------------------------------------------------------
    // 标记位
    // -----------------------------------------------------------------------

    #[test]
    fn flags_roundtrip() {
        let mut w = World::new(64, 64).unwrap();
        assert_eq!(w.flags(7, 7), 0);
        assert!(w.set_flags(7, 7, 42));
        assert_eq!(w.flags(7, 7), 42);
        assert!(!w.set_flags(7, 7, 42), "redundant write must not dirty");
        assert_eq!(w.flags(-1, 0), 0, "out of bounds reads give 0");
        assert!(!w.set_flags(-1, 0, 1), "out of bounds writes are ignored");
    }

    #[test]
    fn changing_material_clears_stale_flags() {
        let mut w = World::new(64, 64).unwrap();
        w.set(2, 2, MaterialKind::Fire);
        w.set_flags(2, 2, 7);
        w.set(2, 2, MaterialKind::Water);
        assert_eq!(w.flags(2, 2), 0, "new material must not inherit old state");
    }

    /// 标记位参与哈希——否则"火焰只剩寿命在倒计时"这类状态
    /// 在哈希里看不见，可复现性断言就形同虚设。
    #[test]
    fn flags_affect_hash() {
        let mut a = World::new(64, 64).unwrap();
        let mut b = World::new(64, 64).unwrap();
        a.set(1, 1, MaterialKind::Fire);
        b.set(1, 1, MaterialKind::Fire);
        b.set_flags(1, 1, 5);
        assert_ne!(a.hash(), b.hash());
    }

    // -----------------------------------------------------------------------
    // 脏标记晕影
    // -----------------------------------------------------------------------

    /// 变化格的**影响范围**所在的分块也要标脏。
    ///
    /// 这条是"跳块扫描 == 全扫"的前提：邻居的流动会让本格变得可动，
    /// 而本格自己没变过。
    #[test]
    fn dirty_marks_the_influence_radius() {
        let mut w = World::new(256, 256).unwrap();
        w.clear_dirty();
        // 正好落在块 (0,0) 的右下角——晕影会溢出到右边和下边的块。
        w.set(63, 63, MaterialKind::Stone);
        assert!(w.is_chunk_dirty(0, 0), "the cell's own chunk");
        assert!(w.is_chunk_dirty(1, 0), "right neighbour chunk");
        assert!(w.is_chunk_dirty(0, 1), "below neighbour chunk");
        assert!(w.is_chunk_dirty(1, 1), "diagonal neighbour chunk");
        assert!(!w.is_chunk_dirty(2, 0), "two chunks away is not affected");
        assert_eq!(w.dirty_chunk_count(), 4);
    }

    /// **横向晕影必须覆盖住一次移动的跨度。**
    ///
    /// 这条守的是那个安静了很久的 bug：液体找平一次能横跨 8 格，
    /// 而晕影只有 1 格，于是"52 号格变了"传不到"离它 8 格、因此变得可动"
    /// 的那一格——跨分块时跳块与全扫就分叉。
    ///
    /// 判据取的是**最刁钻的位置**：距离块边界正好 `DIRTY_HALO_X` 格的那一格，
    /// 它的影响范围恰好够到隔壁块。少一格就漏，多一格就白扫。
    #[test]
    fn halo_reaches_exactly_across_the_chunk_border() {
        let mut w = World::new(256, 256).unwrap();
        w.clear_dirty();
        // x = 63 - 8 = 55：往右 8 格正好是 63，还没过界。
        w.set(55, 30, MaterialKind::Stone);
        assert!(!w.is_chunk_dirty(1, 0), "55 + 8 = 63 仍在块内");

        w.clear_dirty();
        // x = 56：往右 8 格是 64，正好跨过边界。
        w.set(56, 30, MaterialKind::Stone);
        assert!(
            w.is_chunk_dirty(1, 0),
            "56 + {DIRTY_HALO_X} = 64 已经跨块，隔壁必须被标脏"
        );
    }

    #[test]
    fn halo_does_not_escape_the_world() {
        let mut w = World::new(128, 128).unwrap();
        w.clear_dirty();
        w.set(0, 0, MaterialKind::Stone); // 左上角，晕影会越界
        assert_eq!(w.dirty_chunk_count(), 1);
        w.clear_dirty();
        w.set(127, 127, MaterialKind::Stone); // 右下角
        assert_eq!(w.dirty_chunk_count(), 1);
    }

    #[test]
    fn halo_inside_one_chunk_stays_one_chunk() {
        let mut w = World::new(128, 128).unwrap();
        w.clear_dirty();
        w.set(30, 30, MaterialKind::Stone); // 四邻都在块 (0,0) 内
        assert_eq!(w.dirty_chunk_count(), 1);
    }

    // -- 占格层 -------------------------------------------------------------

    #[test]
    fn occupancy_roundtrip() {
        let mut w = World::new(128, 128).unwrap();
        assert!(!w.is_occupied(10, 10));
        assert_eq!(w.owner(10, 10), CellOwner::NONE);

        assert!(w.set_owner(10, 10, CellOwner::new(7, 3, 0)));
        assert!(w.is_occupied(10, 10));
        assert_eq!(w.owner(10, 10), CellOwner::new(7, 3, 0));
        assert_eq!(w.occupied_count(), 1);

        // 同一个值再写一次不该有任何变化。
        assert!(!w.set_owner(10, 10, CellOwner::new(7, 3, 0)));

        assert!(w.vacate(10, 10));
        assert!(!w.is_occupied(10, 10));
        assert_eq!(w.occupied_count(), 0);
    }

    #[test]
    fn occupancy_out_of_bounds_is_none_and_ignored() {
        let mut w = World::new(128, 128).unwrap();
        assert_eq!(w.owner(-1, 0), CellOwner::NONE);
        assert_eq!(w.owner(0, 128), CellOwner::NONE);
        assert!(!w.is_occupied(128, 0));
        assert!(!w.set_owner(-1, -1, CellOwner::new(1, 0, 0)));
        assert_eq!(w.occupied_count(), 0);
    }

    /// 有机体占住的格子，任何材质都进不去——哪怕它的材质字段是 `Empty`。
    ///
    /// 这是"一格非材质即细胞"在材质层的唯一执行点。少了它，水会灌进鱼肚子里。
    #[test]
    fn yields_to_blocks_occupied_cells() {
        let mut w = World::new(128, 128).unwrap();
        w.set(20, 20, MaterialKind::Empty);
        // 空位对沙是"可以让开"的。
        assert!(w.yields_to(20, 20, MaterialKind::Sand));

        w.set_owner(20, 20, CellOwner::new(1, 0, 0));
        assert!(!w.yields_to(20, 20, MaterialKind::Sand));
        assert!(!w.yields_to(20, 20, MaterialKind::Water));

        // 石本来就挡一切，占用与否都一样。
        w.set(21, 20, MaterialKind::Stone);
        assert!(!w.yields_to(21, 20, MaterialKind::Sand));
    }

    #[test]
    fn occupying_marks_dirty() {
        let mut w = World::new(128, 128).unwrap();
        w.clear_dirty();
        w.set_owner(30, 30, CellOwner::new(1, 0, 0));
        assert_eq!(w.dirty_chunk_count(), 1, "占格会改变邻居能不能流进来");
    }

    /// 没有有机体的世界，哈希必须与加占格层**之前**逐位相同。
    ///
    /// 这条钉住的是"只增不改"：占格层不该悄悄改变 M1 世界的指纹，
    /// 否则所有历史黄金哈希都失效，而那种失效看起来会很像"物理变了"。
    #[test]
    fn empty_occupancy_keeps_the_hash_unchanged() {
        let mut w = World::new(128, 128).unwrap();
        for x in 0..128isize {
            w.set(x, 100, MaterialKind::Stone);
        }
        let materials_only = {
            let mut h = super::FNV_OFFSET_BASIS;
            for cell in w.cells() {
                h ^= cell.kind.as_u8() as u64;
                h = h.wrapping_mul(super::FNV_PRIME);
                h ^= cell.flags as u64;
                h = h.wrapping_mul(super::FNV_PRIME);
            }
            h
        };
        assert_eq!(w.hash(), materials_only);
    }

    #[test]
    fn occupancy_changes_the_hash() {
        let mut w = World::new(128, 128).unwrap();
        let before = w.hash();
        w.set_owner(5, 5, CellOwner::new(1, 0, 0));
        assert_ne!(w.hash(), before);
        // 同一个世界、同一格换个细胞下标，哈希也该变。
        let with_first = w.hash();
        w.set_owner(5, 5, CellOwner::new(1, 1, 0));
        assert_ne!(w.hash(), with_first);
    }
}
