//! 生命层：细胞、有机体、基因组。
//!
//! ## 这一层与材质层的唯一接触面
//!
//! 材质层不认识"有机体"，它只知道有些格子不能让路（[`World::yields_to`]）。
//! 反过来，生命层通过 [`World`] 读写材质与占格表，但**不碰**任何材质规则：
//! 细胞死了就把那格写成 [`MaterialKind::Remains`]，剩下的交给材质层。
//! 于是"死亡 → 残骸 → 被吃掉/沉降"这条能量回流通道是**免费**的，
//! 不需要在生命层里再写一遍物质循环。
//!
//! ## 身体是"锚点 + 偏移"
//!
//! 有机体的每个细胞只记录**相对锚点**的偏移（[`Cell::offset`]），
//! 绝对坐标由 `anchor + offset` 现算。这样整体移动就是改两个整数，
//! 不用遍历全身、不用重排占格表之外的东西。
//!
//! 代价是：局部变化（某个细胞被吃掉、某个细胞分裂）必须重排偏移表。
//! M2 不做局部变化——细胞要么全活要么全死——所以这个代价现在不用付。
//!
//! ## 能量记在有机体上，不记在细胞上
//!
//! SPEC §3.2 说"每个细胞带局部状态：能量 / 年龄 / 类型"。实现时把能量
//! 收到有机体一级：多细胞生物靠循环系统共享养分，而不是每个细胞各自记账。
//! 这不是偷懒——它让"能量账逐 tick 守恒"变成一句可以直接写的断言
//! （`Σ organism.energy + 环境残骸能量 + 累计耗散 == 累计注入`），
//! 而细胞级能量需要额外定义"能量怎么在细胞间流动"，那是一整套新规则。
//! 年龄仍然记在细胞上，因为"某个部位先老死"是 M3 才要的东西。

use crate::material::MaterialKind;
use crate::world::World;

/// 有机体编号。0 是保留值（表示"没有主人"），见 [`crate::world::CellOwner`]。
pub type OrganismId = u32;

/// 一个残骸格蕴含的生物可用能量。
///
/// 死亡时细胞把能量交给残骸，进食时再取回来。这个数字是生命层
/// 能量账的**唯一汇率**，所以只在这里定义一次。
pub const REMAINS_ENERGY: u32 = 25;

/// 细胞类型（SPEC §3.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum CellType {
    /// 嘴：吃掉邻格的残骸 / 细胞。
    Mouth,
    /// 生产者：把光转成生物可用能量。**唯一**的"环境 → 生物"入口。
    Producer,
    /// 移动：提供位移。没有它，有机体哪儿也去不了。
    Mover,
    /// 眼：扩大感知范围（M2 里只是让邻域感知更准）。
    Eye,
    /// 杀手：伤害邻格细胞。
    Killer,
    /// 装甲：减免受到的伤害。
    Armor,
    /// 构造器：把材质转成结构（M3）。
    Builder,
}

impl CellType {
    /// 类型总数。
    pub const COUNT: usize = 7;

    /// 全部类型，按 `as_u8` 的顺序。
    pub const ALL: [Self; Self::COUNT] = [
        Self::Mouth,
        Self::Producer,
        Self::Mover,
        Self::Eye,
        Self::Killer,
        Self::Armor,
        Self::Builder,
    ];

    /// 稳定的字节表示。存档与哈希都靠它。
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 每 tick 的维持成本。
    ///
    /// 活着必须很贵——这是"长更多嘴还是长更多腿"能成为真问题的前提
    /// （SPEC §3.3）。生产者最贵，因为它自己就能把光变成能量；
    /// 装甲和眼最便宜，因为它们是"不产出但也不消耗"的器官。
    #[inline]
    pub const fn upkeep(self) -> u32 {
        match self {
            Self::Mouth => 2,
            Self::Producer => 5,
            Self::Mover => 4,
            Self::Eye => 1,
            Self::Killer => 3,
            Self::Armor => 1,
            Self::Builder => 6,
        }
    }

    /// 质量。移动代价与它挂钩——装甲多的身体更重。
    #[inline]
    pub const fn mass(self) -> u32 {
        match self {
            Self::Armor => 3,
            Self::Mover => 2,
            _ => 1,
        }
    }

    /// 单字显示，给 ASCII 渲染器与调试输出用。
    #[inline]
    pub const fn symbol(self) -> char {
        match self {
            Self::Mouth => 'M',
            Self::Producer => 'P',
            Self::Mover => 'V',
            Self::Eye => 'o',
            Self::Killer => 'K',
            Self::Armor => 'A',
            Self::Builder => 'B',
        }
    }
}

/// 一个细胞。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// 细胞类型。
    pub kind: CellType,
    /// 相对锚点的偏移（格）。可以是负的——身体向上长就靠它。
    pub offset: (i16, i16),
    /// 这个细胞活了多少 tick。
    pub age: u32,
}

impl Cell {
    /// 新建一个细胞。
    #[inline]
    pub const fn new(kind: CellType, offset: (i16, i16)) -> Self {
        Self {
            kind,
            offset,
            age: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// 基因组
// ---------------------------------------------------------------------------

/// 行为网络的输入个数。
///
/// 输入刻意做成"抽象感知"而不是逐格 one-hot：9 个邻格 × 5 类要 45 个输入，
/// 权重表会膨胀到 640 个数字，手工设计一条鱼基本不可能。
/// 这里只保留"前方有没有吃的/有没有挡路的/自己还剩多少力气"这类决策真正需要的信息。
pub const INPUTS: usize = 12;

/// 隐藏层宽度。
pub const HIDDEN: usize = 8;

/// 输出个数：左 / 右 / 上 / 下 / 进食 / 繁殖。
pub const OUTPUTS: usize = 6;

/// 权重总数（含偏置与直连）。
pub const WEIGHT_COUNT: usize =
    INPUTS * HIDDEN + HIDDEN + HIDDEN * OUTPUTS + OUTPUTS + INPUTS * OUTPUTS;

/// 输出下标：向左移动。
pub const OUT_LEFT: usize = 0;
/// 输出下标：向右移动。
pub const OUT_RIGHT: usize = 1;
/// 输出下标：向上（对抗重力）。
pub const OUT_UP: usize = 2;
/// 输出下标：向下（顺重力）。
pub const OUT_DOWN: usize = 3;
/// 输出下标：进食。
pub const OUT_EAT: usize = 4;
/// 输出下标：繁殖。
pub const OUT_REPRODUCE: usize = 5;

/// 输入下标：前方有食物（残骸）。
pub const IN_FOOD_AHEAD: usize = 0;
/// 输入下标：前方被阻挡。
pub const IN_BLOCKED_AHEAD: usize = 1;
/// 输入下标：前方有同类。
pub const IN_KIN_AHEAD: usize = 2;
/// 输入下标：前方有异种。
pub const IN_ALIEN_AHEAD: usize = 3;
/// 输入下标：左侧被阻挡。
pub const IN_BLOCKED_LEFT: usize = 4;
/// 输入下标：右侧被阻挡。
pub const IN_BLOCKED_RIGHT: usize = 5;
/// 输入下标：上方被阻挡。
pub const IN_BLOCKED_UP: usize = 6;
/// 输入下标：下方被支撑（脚下有东西，站得住）。
pub const IN_SUPPORTED_BELOW: usize = 7;
/// 输入下标：能量比例（0..1）。
pub const IN_ENERGY: usize = 8;
/// 输入下标：年龄比例（0..1）。
pub const IN_AGE: usize = 9;
/// 输入下标：朝向 x（−1 / 0 / +1）。
pub const IN_FACING_X: usize = 10;
/// 输入下标：朝向 y（−1 / 0 / +1）。
pub const IN_FACING_Y: usize = 11;

/// 基因调控网络（GRN）。
///
/// M2 **不跑形态发生**——手工生物的身体是直接给定的，形状还不由基因决定。
/// 这个结构在这里是为了让 [`Genome`] 的形状从第一天起就包含它，
/// 免得 M3 加形态发生时要改所有序列化与变异代码。
/// 字段留空（`Vec::new()`）是合法的：空网络 = "不发育，用给定形状"。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Grn {
    /// 基因节点。每个节点描述"产物类型 + 阈值 + 位置提示"。
    pub nodes: Vec<GeneNode>,
    /// 调控边。
    pub edges: Vec<Regulation>,
}

/// GRN 的一个节点。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GeneNode {
    /// 这个基因让细胞变成什么类型。
    pub product: CellType,
    /// 激活阈值。
    pub activate_at: f32,
    /// 抑制阈值。
    pub inhibit_at: f32,
}

/// GRN 的一条调控边。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Regulation {
    /// 源节点。
    pub from: u16,
    /// 目标节点。
    pub to: u16,
    /// 权重。负值是抑制。
    pub weight: f32,
}

/// 基因组：行为网络权重 + 形态发生网络。
#[derive(Debug, Clone, PartialEq)]
pub struct Genome {
    /// 行为网络权重，布局见 [`WEIGHT_COUNT`]。
    pub weights: Vec<f32>,
    /// 形态发生网络。M2 为空。
    pub grn: Grn,
}

impl Genome {
    /// 全零基因组（大脑什么都不做）。
    pub fn blank() -> Self {
        Self {
            weights: vec![0.0; WEIGHT_COUNT],
            grn: Grn::default(),
        }
    }

    /// 从一组权重构造。长度不对会补零或截断——它只用于测试与手工生物，
    /// 不该成为"能不能构造出基因组"的失败点。
    pub fn from_weights(weights: Vec<f32>) -> Self {
        let mut w = vec![0.0; WEIGHT_COUNT];
        let n = weights.len().min(WEIGHT_COUNT);
        w[..n].copy_from_slice(&weights[..n]);
        Self {
            weights: w,
            grn: Grn::default(),
        }
    }

    /// 输入 `i` 到隐藏 `h` 的权重下标。
    #[inline]
    pub const fn w_ih(i: usize, h: usize) -> usize {
        i * HIDDEN + h
    }

    /// 隐藏层偏置的下标。
    #[inline]
    pub const fn b_h(h: usize) -> usize {
        INPUTS * HIDDEN + h
    }

    /// 隐藏 `h` 到输出 `o` 的权重下标。
    #[inline]
    pub const fn w_ho(h: usize, o: usize) -> usize {
        INPUTS * HIDDEN + HIDDEN + h * OUTPUTS + o
    }

    /// 输出偏置的下标。
    #[inline]
    pub const fn b_o(o: usize) -> usize {
        INPUTS * HIDDEN + HIDDEN + HIDDEN * OUTPUTS + o
    }

    /// 输入 `i` 直连输出 `o` 的权重下标。
    ///
    /// 这一层是后加的，理由很实在：纯两层网络里"看到食物就吃"要落到 8 个隐藏
    /// 神经元上，而多个这样的连接会共用隐藏单元、互相串扰——手工设计一条鱼
    /// 会变成猜谜。加一层直连之后，一句 `connect(IN_FOOD_AHEAD, OUT_EAT, 5.0)`
    /// 就是字面意思。隐藏层仍然在，负责组合特征（"左边有墙且能量低"之类）。
    #[inline]
    pub const fn w_io(i: usize, o: usize) -> usize {
        INPUTS * HIDDEN + HIDDEN + HIDDEN * OUTPUTS + OUTPUTS + i * OUTPUTS + o
    }

    /// 把一条"输入 → 输出"的直连接上。
    ///
    /// 手工设计大脑时最常用的一句话是"看到食物就往前走"，
    /// 这个辅助函数让它变成 `genome.connect(IN_FOOD_AHEAD, OUT_RIGHT, 1.0)`。
    /// 多次连接同一条边会累加（和真实突触一样）。
    pub fn connect(&mut self, input: usize, output: usize, strength: f32) {
        self.weights[Self::w_io(input, output)] += strength;
    }

    /// 跑一遍行为网络，返回 6 个输出。
    ///
    /// 结构是 `输入 → tanh 隐藏 → tanh 输出`，外加一条输入直连输出的通路。
    /// 输出不过 softmax：6 个通道不是互斥选项（可以同时想往右走和想吃东西），
    /// 每个通道各自与 0 比大小即可。
    pub fn think(&self, inputs: &[f32; INPUTS]) -> [f32; OUTPUTS] {
        let mut hidden = [0.0f32; HIDDEN];
        for (h, slot) in hidden.iter_mut().enumerate() {
            let mut sum = self.weights[Self::b_h(h)];
            for (i, &x) in inputs.iter().enumerate() {
                if x != 0.0 {
                    sum += x * self.weights[Self::w_ih(i, h)];
                }
            }
            *slot = sum.tanh();
        }

        let mut out = [0.0f32; OUTPUTS];
        for (o, slot) in out.iter_mut().enumerate() {
            let mut sum = self.weights[Self::b_o(o)];
            for (h, &x) in hidden.iter().enumerate() {
                sum += x * self.weights[Self::w_ho(h, o)];
            }
            for (i, &x) in inputs.iter().enumerate() {
                if x != 0.0 {
                    sum += x * self.weights[Self::w_io(i, o)];
                }
            }
            *slot = sum.tanh();
        }
        out
    }

    /// 点突变：每个权重以 `rate` 的概率加上一个高斯扰动。
    ///
    /// 只动数值、不动拓扑，所以这是 SPEC §4 里"最弱的算子"。
    /// 拓扑变异（基因重复、边增删）要等 GRN 真正参与形态发生之后才有意义——
    /// 现在身体形状是给定的，多一条调控边不会长出任何东西。
    ///
    /// 随机性全部来自调用方传入的 [`crate::rng::Rng`]，所以整个流程仍然确定性。
    pub fn mutate(&mut self, rng: &mut crate::rng::Rng, rate: f32, sigma: f32) {
        for w in &mut self.weights {
            if rng.next_f32() < rate {
                // Box–Muller：两个均匀分布 → 一个正态分布。
                let u1 = rng.next_f32().max(1e-7);
                let u2 = rng.next_f32();
                let g = (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos();
                *w = (*w + g * sigma).clamp(-8.0, 8.0);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 有机体
// ---------------------------------------------------------------------------

/// 朝向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Facing {
    /// 水平分量：−1 向左，+1 向右。
    pub x: i8,
    /// 垂直分量：−1 向上，+1 向下。
    pub y: i8,
}

impl Facing {
    /// 默认朝右。
    pub const RIGHT: Self = Self { x: 1, y: 0 };

    /// 水平翻转。
    #[inline]
    pub const fn flipped(self) -> Self {
        Self {
            x: -self.x,
            y: self.y,
        }
    }
}

impl Default for Facing {
    fn default() -> Self {
        Self::RIGHT
    }
}

/// 一个有机体。
///
/// `anchor` 是身体锚点的**绝对**坐标；细胞的绝对坐标 = `anchor + cell.offset`。
///
/// ## 水平方向是环面
///
/// 世界在 x 方向首尾相接：右边界向右走一格就回到左边界。这条**只对生命层成立**
/// ——材质层仍然是有界的（水不会从左边流出去又从右边流回来），
/// 因为材质层的分块并行依赖"两块之间至少隔一整块"这个几何前提，
/// 而生物的移动是整体平移、取模几乎不要钱。
///
/// 做这件事的动机很具体：只给"往右"偏置的手工鱼会全部游到右边界堆成一团，
/// 然后被领地规则卡住，种群停在 9 条——表面看是"生态平衡"，
/// 实际是"所有鱼叠在一个角落"。环绕之后它们会自己散开。
#[derive(Debug, Clone)]
pub struct Organism {
    /// 编号。同时也是 [`crate::world::CellOwner::organism`] 里存的值。
    pub id: OrganismId,
    /// 锚点的绝对坐标。x 已经取过模，永远落在 `0..width`。
    pub anchor: (i32, i32),
    /// 世界宽度，用于水平取模。`0` 表示"不知道世界多大，不环绕"——
    /// 只有测试里手工构造、还没 `spawn` 的有机体是这个状态。
    pub width: i32,
    /// 身体：每个细胞的类型与相对偏移。
    pub cells: Vec<Cell>,
    /// 生物可用能量（整数点，避免浮点漂移破坏守恒断言）。
    pub energy: u32,
    /// 活了多少 tick。
    pub age: u32,
    /// 朝向。决定"前方"是哪一格。
    pub facing: Facing,
    /// 基因组。
    pub genome: Genome,
    /// 亲代编号。`None` 表示"投放进来的初代"，不是谁生出来的。
    pub parent: Option<OrganismId>,
    /// 第几代。初代是 0，子代是亲代 + 1。
    ///
    /// 这是 M3 第一个观测指标：**代数在涨，才说明这个种群真的在迭代**。
    /// 没有它，一个"看起来在动"的种群可能只是同一批个体在活，
    /// 那和进化是两件完全不同的事。
    pub generation: u32,
    /// 本 tick 是否已经行动过。
    ///
    /// 生命层是单线程顺序执行的，这一位主要防"繁殖出来的子代在同一个 tick
    /// 里又行动一次"——那种情况会让一代的繁殖速度凭空翻倍。
    pub acted: bool,
}

impl Organism {
    /// 新建有机体。能量给 0，调用方自己分配。
    ///
    /// `width` 从 0 开始（不环绕）；[`crate::life_sim::LifeSim::spawn`]
    /// 会把它设成世界的真实宽度。
    pub fn new(id: OrganismId, anchor: (i32, i32), cells: Vec<Cell>, genome: Genome) -> Self {
        Self {
            id,
            anchor,
            width: 0,
            cells,
            energy: 0,
            age: 0,
            facing: Facing::RIGHT,
            genome,
            parent: None,
            generation: 0,
            acted: false,
        }
    }

    /// 把 x 折回 `0..width`。`width == 0` 时原样返回。
    #[inline]
    pub fn wrap_x(&self, x: i32) -> i32 {
        if self.width > 0 {
            x.rem_euclid(self.width)
        } else {
            x
        }
    }

    /// 身体的格子总数。
    #[inline]
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }

    /// 每 tick 的维持成本总和。
    pub fn upkeep(&self) -> u32 {
        self.cells.iter().map(|c| c.kind.upkeep()).sum()
    }

    /// 身体总质量。
    pub fn mass(&self) -> u32 {
        self.cells.iter().map(|c| c.kind.mass()).sum()
    }

    /// 某个细胞的绝对坐标。x 已折回世界内。
    #[inline]
    pub fn cell_at(&self, cell: &Cell) -> (i32, i32) {
        (
            self.wrap_x(self.anchor.0 + cell.offset.0 as i32),
            self.anchor.1 + cell.offset.1 as i32,
        )
    }

    /// 全身的绝对坐标。
    pub fn footprint(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.cells.iter().map(|c| self.cell_at(c))
    }

    /// 有没有某种细胞。
    #[inline]
    pub fn has(&self, kind: CellType) -> bool {
        self.cells.iter().any(|c| c.kind == kind)
    }

    /// 某种细胞的数量。
    #[inline]
    pub fn count(&self, kind: CellType) -> usize {
        self.cells.iter().filter(|c| c.kind == kind).count()
    }

    /// 朝向前方那一格的绝对坐标。x 已折回世界内。
    #[inline]
    pub fn front(&self) -> (i32, i32) {
        (
            self.wrap_x(self.anchor.0 + self.facing.x as i32),
            self.anchor.1 + self.facing.y as i32,
        )
    }

    /// 把身体写进占格表。调用方保证这些格子当前无主。
    pub fn claim(&self, world: &mut World) {
        for (i, cell) in self.cells.iter().enumerate() {
            let (x, y) = self.cell_at(cell);
            world.set_owner(
                x as isize,
                y as isize,
                crate::world::CellOwner::new(self.id, i as u16),
            );
        }
    }

    /// 从占格表里抹掉自己的身体。
    pub fn release(&self, world: &mut World) {
        for cell in &self.cells {
            let (x, y) = self.cell_at(cell);
            world.vacate(x as isize, y as isize);
        }
    }
}

/// 一个细胞死亡时，这一格该变成什么材质。
///
/// 集中在一处，是因为"死亡 → 残骸"是 SPEC §2.1 点名不能省的能量回流通道。
/// 如果哪天想让装甲细胞留下石头、生产者留下可燃物，只改这里。
#[inline]
pub const fn remains_of(_kind: CellType) -> MaterialKind {
    MaterialKind::Remains
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weight_layout_is_contiguous_and_in_bounds() {
        // 三段权重 + 两段偏置必须刚好铺满整个数组，不许有重叠或空洞。
        assert_eq!(Genome::w_ih(INPUTS - 1, HIDDEN - 1), INPUTS * HIDDEN - 1);
        assert_eq!(Genome::b_h(0), INPUTS * HIDDEN);
        assert_eq!(Genome::b_h(HIDDEN - 1), INPUTS * HIDDEN + HIDDEN - 1);
        assert_eq!(Genome::w_ho(0, 0), INPUTS * HIDDEN + HIDDEN);
        assert_eq!(
            Genome::w_ho(HIDDEN - 1, OUTPUTS - 1),
            INPUTS * HIDDEN + HIDDEN + HIDDEN * OUTPUTS - 1
        );
        assert_eq!(
            Genome::b_o(OUTPUTS - 1),
            INPUTS * HIDDEN + HIDDEN + HIDDEN * OUTPUTS + OUTPUTS - 1
        );
        assert_eq!(
            Genome::w_io(0, 0),
            INPUTS * HIDDEN + HIDDEN + HIDDEN * OUTPUTS + OUTPUTS
        );
        assert_eq!(
            Genome::w_io(INPUTS - 1, OUTPUTS - 1) + 1,
            WEIGHT_COUNT,
            "直连段必须刚好铺到数组末尾"
        );
    }

    #[test]
    fn blank_genome_thinks_nothing() {
        let g = Genome::blank();
        let out = g.think(&[0.0; INPUTS]);
        assert!(out.iter().all(|v| *v == 0.0), "全零网络必须输出全零");
    }

    #[test]
    fn connect_wires_an_input_to_an_output() {
        let mut g = Genome::blank();
        g.connect(IN_FOOD_AHEAD, OUT_RIGHT, 4.0);

        let mut inputs = [0.0f32; INPUTS];
        let quiet = g.think(&inputs);
        inputs[IN_FOOD_AHEAD] = 1.0;
        let food = g.think(&inputs);

        assert!(
            food[OUT_RIGHT] > quiet[OUT_RIGHT] + 0.5,
            "看到食物应该更想往右走：{} vs {}",
            food[OUT_RIGHT],
            quiet[OUT_RIGHT]
        );
        // 没有接线的那几路不该被带动。
        assert!(food[OUT_UP].abs() < 1e-6);
    }

    #[test]
    fn thinking_is_deterministic() {
        let mut g = Genome::blank();
        g.connect(IN_ENERGY, OUT_EAT, 2.0);
        g.connect(IN_FOOD_AHEAD, OUT_LEFT, -1.5);
        let inputs = [0.3; INPUTS];
        assert_eq!(g.think(&inputs), g.think(&inputs));
    }

    #[test]
    fn mutation_changes_weights_but_stays_bounded() {
        let mut g = Genome::blank();
        let mut rng = crate::rng::Rng::new(12345);
        g.mutate(&mut rng, 1.0, 0.5);
        assert!(g.weights.iter().any(|w| *w != 0.0), "rate=1 必须动到权重");
        assert!(g.weights.iter().all(|w| w.abs() <= 8.0), "突变必须有界");
    }

    #[test]
    fn mutation_is_reproducible() {
        let mut a = Genome::blank();
        let mut b = Genome::blank();
        a.mutate(&mut crate::rng::Rng::new(7), 0.5, 0.3);
        b.mutate(&mut crate::rng::Rng::new(7), 0.5, 0.3);
        assert_eq!(a.weights, b.weights, "同种子必须突变成同一个基因组");
    }

    #[test]
    fn upkeep_scales_with_body() {
        let small = Organism::new(
            1,
            (0, 0),
            vec![Cell::new(CellType::Mover, (0, 0))],
            Genome::blank(),
        );
        let big = Organism::new(
            2,
            (0, 0),
            vec![
                Cell::new(CellType::Mover, (0, 0)),
                Cell::new(CellType::Producer, (1, 0)),
                Cell::new(CellType::Mouth, (0, 1)),
            ],
            Genome::blank(),
        );
        assert!(big.upkeep() > small.upkeep());
        assert_eq!(big.upkeep(), 4 + 5 + 2);
        assert_eq!(big.mass(), 2 + 1 + 1);
    }

    #[test]
    fn offsets_resolve_against_the_anchor() {
        let org = Organism::new(
            1,
            (10, 20),
            vec![
                Cell::new(CellType::Mover, (0, 0)),
                Cell::new(CellType::Mouth, (1, 0)),
                Cell::new(CellType::Eye, (-1, -1)),
            ],
            Genome::blank(),
        );
        let spots: Vec<_> = org.footprint().collect();
        assert_eq!(spots, vec![(10, 20), (11, 20), (9, 19)]);
        assert_eq!(org.front(), (11, 20), "朝右时前方是锚点右侧一格");
    }
}
