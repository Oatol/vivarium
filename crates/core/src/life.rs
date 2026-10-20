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

use crate::grn::Grn;
use crate::material::MaterialKind;
use crate::world::World;

/// 有机体编号。0 是保留值（表示"没有主人"），见 [`crate::world::CellOwner`]。
pub type OrganismId = u32;

/// 一个残骸格蕴含的生物可用能量。
///
/// 死亡时细胞把能量交给残骸，进食时再取回来。这个数字是生命层
/// 能量账的**唯一汇率**，所以只在这里定义一次。
pub const REMAINS_ENERGY: u32 = 25;

/// 每格细胞的能量上限。
///
/// 原来是固定 1000（等于三格鱼的额度）。换手工体型之后必须按格数给，
/// 否则大身体永远攒不到繁殖门槛——身体越大造子代越贵，而天花板钉死在
/// 一个三格鱼的数上。
///
/// 350 让三格鱼的上限落在 1050，与旧的 1000 只差 5%：
/// 老的生态测试与调参结论因此仍然可比。
pub const MAX_ENERGY_PER_CELL: u32 = 350;

/// 每格细胞在繁殖时的"造价"。
///
/// 67 让三格鱼算出来是 201，对上旧的 `BIRTH_ENERGY = 200`。
pub const BIRTH_ENERGY_PER_CELL: u32 = 67;

/// 每次繁殖改几条**形态发生规则**。
///
/// 单独一个常数，因为规则表的变异不能用权重的变异率：规则表有几千个槽，
/// 按概率逐槽掷会一次改掉上百条（见 `Grn::mutate`）。这里按"改几条"算。
///
/// 1 → 2 的理由：32000 tick 只有 39~54 代，每次只改 1 条规则，
/// 一条谱系累计下来只有几十次改动——**远不足以"发明"一个更好的身体**。
/// 实测形态发生只能收敛到手工体型里最好的那个，从没长出新东西。
pub const GRN_EDITS_PER_BIRTH: usize = 2;

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
    /// **出生时它是不是占着一格水。**
    ///
    /// 这是水量守恒的全部秘密。出生时身体会把它占住的那格水"借走"
    /// （见 [`crate::life_sim::LifeSim::spawn`]），死的时候再还回来——
    /// 但只有当初真的借了才该还。
    ///
    /// 一条鱼贴着水面出生，上半身泡在空气里，那几格它从没借过水；
    /// 后来它沉下去死了，尸体泡在水里、按"水解"规则腐烂成水，
    /// 于是凭空多出几格水。实测 32000 tick 多了一万八千格，
    /// 而能量账一分不差、生物数还在涨——从任何别的指标上都看不出来。
    ///
    /// 债务必须在**出生那一刻**定死，不能到死的时候按"当时泡不泡在水里"倒推。
    pub wet: bool,
}

impl Cell {
    /// 新建一个细胞。`wet` 由 `spawn` 按出生位置填。
    #[inline]
    pub const fn new(kind: CellType, offset: (i16, i16)) -> Self {
        Self {
            kind,
            offset,
            age: 0,
            wet: false,
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
pub const INPUTS: usize = 17;

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
/// 输入下标：这一格的光照强度（0..1）。
///
/// M3 加它，是因为"待在哪儿"必须变成**可进化的性状**。
/// 光随深度衰减、又被上方的邻居挡住，所以浅水和深水的收益差得很远；
/// 但鱼如果感知不到光，就没法把"我这儿太暗了"变成一个动作，
/// 于是自然选择只能在"能不能活着"上筛，选不出"活得更好"。
///
/// 有了这一路输入，"往光里游"才有可能被突变撞出来，
/// 而基因距离与平均能量才有机会同步上升——那是"适应"的标志。
pub const IN_LIGHT: usize = 12;
/// 输入下标：最近的**含能量食物**在水平方向的相对位置（−1..+1）。
///
/// 和 [`IN_FOOD_AHEAD`] 的区别是尺度：那一路上是"嘴前面那一格有没有东西"，
/// 只能支撑"撞上就吃"，撑不起"去哪儿吃"。这一路是**嗅觉**——
/// 沿四个方向各射一条射线，撞到固体就断（墙后面闻不到），
/// 找到食物就按距离衰减地累加一个方向分量。
///
/// 为什么必须有它：海底的藻席一旦成为主要能量来源，"去哪儿吃"
/// 就成了唯一值得学的决策。而一个看不见食物的网络只能靠随机游走碰上，
/// 那种"学习"跟掷骰子没有区别。
pub const IN_FOOD_DIR_X: usize = 13;
/// 输入下标：最近的含能量食物在垂直方向的相对位置（−1..+1，+1 是下方）。
pub const IN_FOOD_DIR_Y: usize = 14;
/// 输入下标：最近的**别的生物**在水平方向的相对位置（−1..+1）。
///
/// 和 `IN_ALIEN_AHEAD` 的区别就是"前方一格"和"二十格内哪边"。
/// 只有前者的时候，"追"和"逃"都做不到——你只能撞上才知道有别人，
/// 于是捕食全靠运气，猎物也从不躲。
///
/// 这一路让"追 / 逃"第一次成为可能，而**要不要追、要不要逃由基因组决定**：
/// 手工基因组给的是"有牙的靠过去、没牙的躲开"（见
/// `life_sim::handcrafted_fish_genome`），之后突变自己去调。
pub const IN_ORGANISM_DIR_X: usize = 15;
/// 输入下标：最近的别的生物在垂直方向的相对位置（−1..+1，+1 是下方）。
pub const IN_ORGANISM_DIR_Y: usize = 16;

/// 嗅觉射线的基础长度（格）：**没有眼**的身体能闻多远。
///
/// 眼不再是白付维持成本的装饰：每多一只眼，射线长 [`SMELL_PER_EYE`] 格。
/// 这一条是"让形状有用"的一半——在那之前，眼、装甲、移动细胞对**形状**
/// 毫无影响，体型只是"细胞数量的多重集"，突变没有方向可走。
///
/// **基础值刻意保持 20**（和"眼还没用"的时候一样）。第一版顺手写成 12，
/// 结果所有无眼体型（叶背鱼 / 蠕虫 / 掠食者）的嗅觉一起被砍，
/// 整个生态的最优体型从 10 格跳到 14 格——那是**削弱**，不是"形状有了梯度"。
/// 加维度的时候不能顺手改动已有的绝对值，否则实验就没法归因了。
pub const FOOD_SMELL_RADIUS: i32 = 20;

/// 每多一只眼，嗅觉射线加长多少格。
pub const SMELL_PER_EYE: i32 = 4;

/// 嗅觉射线的上限（格）。再长就开始挤占材质层的时间预算。
pub const MAX_SMELL_RADIUS: i32 = 32;

/// 每多几个移动细胞，每 tick 多走一格。
///
/// 取 4：银鱼（3 移动）走 1 格、水母（4）走 2 格、蠕虫（6）走 2 格。
/// 移动成本按 `质量 / 速度` 摊到每一步，所以**总成本不随速度上涨**——
/// 多长几格移动细胞买的是"同样的钱跑得更远"，代价是维持成本和重量。
pub const MOVERS_PER_EXTRA_STEP: usize = 4;

/// 每 tick 最多走几格。
pub const MAX_STEPS: i32 = 3;

/// 基因组：行为网络权重 + 形态发生网络。
#[derive(Debug, Clone, PartialEq)]
pub struct Genome {
    /// 行为网络权重，布局见 [`WEIGHT_COUNT`]。
    pub weights: Vec<f32>,
    /// 形态发生：这套规则决定身体长成什么样。见 [`crate::grn`]。
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
        // 形态发生基因**必须单独算变异率**。
        //
        // 规则表有上千个槽，用 `rate` 逐个掷骰子的话，一个正常的 5% 就会
        // 一次改掉几十条规则——身体立刻变成随机团。所以这里按"改几条"算，
        // 而且一次只改 1 条（见 `Grn::mutate` 的注释）。
        self.grn.mutate(rng, GRN_EDITS_PER_BIRTH);
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
        self.cell_at_offset(self.anchor, cell.offset)
    }

    /// 偏移在某个锚点上的绝对落点。x 已折回世界内。
    ///
    /// 单独拆出来是因为 [`crate::life_sim::LifeSim::try_move`] 需要在**移动之前**
    /// 用"移动之后的锚点 + 移动之后的朝向"算一遍落点：先验完再动手，
    /// 半步移动会留下撕裂的身体。
    #[inline]
    pub fn cell_at_offset(&self, anchor: (i32, i32), offset: (i16, i16)) -> (i32, i32) {
        (
            self.wrap_x(anchor.0 + self.offset_x(offset)),
            anchor.1 + offset.1 as i32,
        )
    }

    /// 偏移的 x 分量，按当前朝向镜像。
    ///
    /// 体型只在 [`crate::body`] 里按"朝右"画一遍，朝左时整个身体沿 x 翻过来。
    /// 不翻的话，一条朝左游的鱼会把嘴留在尾巴那一侧——三格身体时看不出来，
    /// 十几格时一眼就是错的。
    ///
    /// 镜像**不单独存一个字段**，而是从 `facing.x` 现算：存一份就必然有
    /// 一处忘了同步，而"身体位置和朝向不一致"这种 bug 从画面上看像是穿模。
    #[inline]
    pub fn offset_x(&self, offset: (i16, i16)) -> i32 {
        Self::offset_x_for(self.facing, offset)
    }

    /// [`Self::offset_x`] 的静态版本，给"算一个**未来**朝向下的落点"用。
    ///
    /// 掉头时必须在动手之前就把新朝向的落点全验一遍，而那时候有机体本身
    /// 的朝向还是旧的。克隆一份有机体来改朝向是能算，但一次移动克隆一整个
    /// 基因组实在没必要。
    #[inline]
    pub const fn offset_x_for(facing: Facing, offset: (i16, i16)) -> i32 {
        if facing.x < 0 {
            -(offset.0 as i32)
        } else {
            offset.0 as i32
        }
    }

    /// 身体在某个方向上最远的那一格，再往外一格。
    ///
    /// 这是"感知探针"该有的样子。用 `anchor ± 1` 是错的：三格鱼的身体
    /// 本来就占住锚点右侧，于是"右边被挡"恒为真——手工鱼因此一直在往左游，
    /// 只是因为它同时也在撞左墙，才看起来像在正常游动。
    ///
    /// 十几格的身体更不可能靠锚点判断：墙可能离锚点四五格远，
    /// 探针不贴到身体表面就永远看不见它。
    pub fn probe(&self, dx: i32, dy: i32) -> (i32, i32) {
        let mut best = self.anchor;
        let mut best_proj = i32::MIN;
        for cell in &self.cells {
            // 投影用相对偏移算，不用绝对坐标：水平环绕会让左右边界的两格
            // 在绝对坐标上贴在一起，比大小就没有意义了。
            let proj = self.offset_x(cell.offset) * dx + cell.offset.1 as i32 * dy;
            if proj > best_proj {
                best_proj = proj;
                best = self.cell_at(cell);
            }
        }
        (self.wrap_x(best.0 + dx), best.1 + dy)
    }

    /// 朝向前方那一格（贴着身体，不是贴着锚点）。x 已折回世界内。
    #[inline]
    pub fn front(&self) -> (i32, i32) {
        self.probe(self.facing.x as i32, self.facing.y as i32)
    }

    /// 能量上限：按细胞数算。
    ///
    /// 固定 1000 会让"大身体"没法繁殖——身体越大，造一个子代越贵，
    /// 而固定上限把攒钱的天花板钉死了。按格数给额度之后，
    /// **大身体必须真的活得更久才生得起**，这才是体型该有的代价。
    #[inline]
    pub fn max_energy(&self) -> u32 {
        MAX_ENERGY_PER_CELL * self.cells.len().max(1) as u32
    }

    /// 造一个子代要多少能量（子代出生时携带的部分）。
    #[inline]
    pub fn birth_energy(&self) -> u32 {
        BIRTH_ENERGY_PER_CELL * self.cells.len().max(1) as u32
    }

    /// 每 tick 最多走几格。
    ///
    /// **这是"形状有用"的第一条**。在此之前移动细胞只影响维持成本和重量，
    /// 一个 6 移动细胞的蠕虫和一个 3 移动细胞的银鱼跑得一样快——
    /// 于是"多长几格移动细胞"永远是个坏买卖，体型不可能朝这个方向进化。
    #[inline]
    pub fn speed(&self) -> i32 {
        let movers = self.count(CellType::Mover);
        let extra = movers / MOVERS_PER_EXTRA_STEP;
        (1 + extra as i32).min(MAX_STEPS)
    }

    /// 嗅觉射线有多长。
    ///
    /// **第二条**。眼在此之前是个纯装饰：付维持成本，什么也不换。
    /// 现在它买的是"更早闻到食物"——在一片 512 宽的海里，闻得远就是活得久。
    #[inline]
    pub fn smell_radius(&self) -> i32 {
        let eyes = self.count(CellType::Eye) as i32;
        (FOOD_SMELL_RADIUS + eyes * SMELL_PER_EYE).min(MAX_SMELL_RADIUS)
    }

    /// 繁殖门槛：攒到这个数才分裂。
    ///
    /// 是子代造价的 **两倍**，也就是"生完还剩一半"。这个比例是从三格鱼的
    /// 旧常数（400 / 200）原样继承的——它已经被几千 tick 的生态测试调过，
    /// 没有理由在换体型的时候顺手改掉。
    #[inline]
    pub fn reproduction_energy(&self) -> u32 {
        self.birth_energy() * 2
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

    /// 把身体写进占格表。调用方保证这些格子当前无主。
    pub fn claim(&self, world: &mut World) {
        for (i, cell) in self.cells.iter().enumerate() {
            let (x, y) = self.cell_at(cell);
            world.set_owner(
                x as isize,
                y as isize,
                crate::world::CellOwner::new(self.id, i as u16, cell.kind.as_u8()),
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
///
/// 返回 [`MaterialKind::Corpse`] 而不是 [`MaterialKind::Remains`]：
/// 前者是"生命制造的"，会腐烂；后者是 worldgen 撒下的天然碎屑，永远稳定。
/// 这个区分是材质层的事（见 `MaterialKind::Corpse` 的注释）。
#[inline]
pub const fn remains_of(_kind: CellType) -> MaterialKind {
    MaterialKind::Corpse
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
        // 前方是**贴着身体**再往外一格，不是锚点右侧一格。
        //
        // 老实现返回 (11, 20)，而那正是嘴自己的格子——于是 `passable` 判它被占，
        // "前方被挡"恒为真。三格鱼因此一直在往左游，只是因为撞到左墙会反向，
        // 看起来像在正常游动，没人发现。
        assert_eq!(org.front(), (12, 20), "前方是嘴再往外一格");
    }

    #[test]
    fn the_body_mirrors_when_the_organism_turns_around() {
        // 体型只按"朝右"画一遍，朝左时整个身体沿 x 翻过来。
        // 不翻的话，一条朝左游的鱼会把嘴留在尾巴那一侧。
        let mut org = Organism::new(
            1,
            (10, 20),
            vec![
                Cell::new(CellType::Mover, (-1, 0)),
                Cell::new(CellType::Mouth, (2, 0)),
            ],
            Genome::blank(),
        );
        assert_eq!(org.footprint().collect::<Vec<_>>(), vec![(9, 20), (12, 20)]);
        assert_eq!(org.front(), (13, 20));

        org.facing = Facing { x: -1, y: 0 };
        assert_eq!(
            org.footprint().collect::<Vec<_>>(),
            vec![(11, 20), (8, 20)],
            "朝左时身体必须沿 x 镜像"
        );
        assert_eq!(org.front(), (7, 20), "嘴翻到左边，前方也要跟着翻");
    }

    #[test]
    fn energy_budget_scales_with_the_body() {
        // 固定上限会让大身体永远攒不到繁殖门槛——体型变大在规则层面被禁止。
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
                Cell::new(CellType::Producer, (0, -1)),
                Cell::new(CellType::Producer, (0, 1)),
            ],
            Genome::blank(),
        );
        assert_eq!(big.max_energy(), small.max_energy() * 3);
        assert_eq!(big.reproduction_energy(), big.birth_energy() * 2);
        // 三格鱼应当落在旧的 1000 / 400 / 200 附近，老调参结论才仍然可比。
        let fish = Organism::new(
            3,
            (0, 0),
            vec![
                Cell::new(CellType::Mover, (0, 0)),
                Cell::new(CellType::Mouth, (1, 0)),
                Cell::new(CellType::Producer, (0, -1)),
            ],
            Genome::blank(),
        );
        assert!((fish.max_energy() as i32 - 1000).abs() <= 50);
        assert!((fish.reproduction_energy() as i32 - 400).abs() <= 10);
        assert!((fish.birth_energy() as i32 - 200).abs() <= 10);
    }
}
