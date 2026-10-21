//! 生命层调度：代谢、死亡、能量账。
//!
//! ## 为什么这一层不进材质线程池
//!
//! SPEC §2.4 明确要求：有机体跨分块移动，**不能**放进材质层的并行调度。
//! 材质层的并行前提是"每块只碰自己的格子"，而一个生物的身体横跨两三块是常态。
//! 硬塞进去就得给占格表加锁，而占格表恰恰是所有生物都要写的东西——
//! 那把锁会比材质计算本身还贵。
//!
//! 所以这里是**单线程顺序执行**。代价可以接受：M2 的有机体数量是个位数到几十，
//! 每个只做十几次邻域查询，总开销远小于材质层。等生物上千了再谈并行，
//! 而那时候的形状大概率是"按生物并行"（每个生物互不重叠），
//! 和材质层的分块并行是两套不同的东西。
//!
//! ## 能量账怎么才能真的对上
//!
//! 只有一个办法：**所有能量进出都必须走同一个记账点**。
//!
//! - 太阳能：`inject` 时记账，只记真正加到生物身上的那一份（取整丢掉的零头不计入）
//! - 维持成本：`dissipated` 累加，能量离开生物可用池但仍在账上
//! - 死亡：剩余能量按细胞数摊进残骸的 `flags`，摊不尽的零头计入 `dissipated`
//! - 进食：从残骸的 `flags` 取出，一进一出，总数不变
//!
//! 于是 [`LifeSim::audit`] 的断言永远是
//! `Σ organism.energy + Σ remains.flags + dissipated == injected`。
//!
//! **已知的不守恒出口**：火烧掉残骸。材质层不知道残骸里存着能量，
//! 一把火把残骸烧成空，那部分能量就凭空消失了。M2 不处理（测试场景里没有火），
//! 但这条必须写在这里，免得以后有人拿"守恒测试没过"当谜题查一整天。

use crate::body::{BodyPlan, PLANS};
use crate::grn::Grn;
use crate::life::{
    remains_of, Cell, CellType, Genome, Organism, OrganismId, INPUTS, IN_AGE, IN_ALIEN_AHEAD,
    IN_BLOCKED_AHEAD, IN_BLOCKED_LEFT, IN_BLOCKED_RIGHT, IN_BLOCKED_UP, IN_ENERGY, IN_FACING_X,
    IN_FACING_Y, IN_FOOD_AHEAD, IN_FOOD_DIR_X, IN_FOOD_DIR_Y, IN_KIN_AHEAD, IN_LIGHT,
    IN_ORGANISM_DIR_X, IN_ORGANISM_DIR_Y, IN_SUPPORTED_BELOW, OUTPUTS, OUT_DOWN, OUT_EAT, OUT_LEFT,
    OUT_REPRODUCE, OUT_RIGHT, OUT_UP, REMAINS_ENERGY,
};
use crate::material::MaterialKind;
use crate::rng::Rng;
use crate::world::World;

/// 生产者细胞在满光下每 tick 产出的能量。
///
/// 这个数字不是拍的，是从"想看到什么生态"反推的：
/// 一条三格鱼（移动 + 嘴 + 生产者）维持成本 11、移动再花 3，合计 14。
/// 要让它在浅水（光 ≈ 0.8）净赚、在深水（光 ≈ 0.15）净亏，
/// 产出就得落在 `14 / 0.8 ≈ 17.5` 到 `14 / 0.15 ≈ 93` 之间。
/// 取 24 意味着浅水每 tick 净赚 5、深水每 tick 亏 11——
/// 攒够一次繁殖（200 点）大约 40 tick，而沉到深处 20 tick 就开始挨饿。
///
/// ## 试过降到 15，失败了，所以是 24
///
/// 目的是把生态从"光照受限"推到"觅食受限"：24 的时候一条 4 生产者的鱼
/// 在光 0.8 下净赚 +26.8/tick，光本身就够活，于是"跑得快 / 看得远"
/// 永远不如"多长一个生产者"，体型只能朝一个方向走。
///
/// 降到 15（甚至 18、20）之后发生的事，比数字本身有用：
///
/// 1. **48 条鱼 4000 tick 内全灭**，而海底的藻席堆到 800 格没人吃。
/// 2. 原因不是"鱼不想吃"，是**几何**：海深 184 格，一趟下潜往返的移动成本
///    是 `质量 × 距离 ≈ 13 × 368 ≈ 4800`，而一格藻只有 120 能量——
///    整片藻席吃下去也回不了本。**海底的藻席对水面上的鱼是不可达的。**
/// 3. 改成"浮游藻"（悬在透光层里）之后鱼能吃到，但藻是**静态固体**，
///    悬在水里会把水流切成碎块——1500 格就让 `活跃块` 掉到 3/64，
///    3000 格时整个世界静止（`移动 0`），鱼全被顶进空气。
///
/// 结论：**这不是调参能解决的。** 要让食物可达又不成障碍，藻需要一套
/// 自己的移动规则（随水漂、可被水挤开），那是一次材质层的改动，不是一次调参。
/// 在它做出来之前，这里保持 24——**宁可"光照受限"但生态健康，
/// 也不要"觅食受限"但世界冻住。**
pub const PRODUCER_YIELD: u32 = 24;

// 能量上限 / 繁殖门槛 / 子代携带能量**都按细胞数算**，见 `life::MAX_ENERGY_PER_CELL`。
//
// 旧版是三个固定常数（1000 / 400 / 200），那等于把"三格鱼"写进了代谢规则里：
// 身体一大，造子代的成本跟着涨，而天花板钉死在三格鱼的额度上，
// 于是大身体永远攒不到繁殖门槛——**体型变大这件事在规则层面就被禁止了**。

/// 能顶开水体的最大身体格数（SPEC §2.2）。
///
/// 超过这个规模就被水挡住。SPEC 的理由是"如果任何生物都能无代价顶开液体，
/// 一个 1000 格的生物移动会推走 1000 格水，行为和性能都会崩"。
///
/// **8 是"最大体型三格"那个时代的上限**，现在必须放大：最大的手工体型是
/// 14 格，上限卡在 8 意味着"设计一个更大的身体"在物理上根本做不到——
/// 大生物会出生在一个水腔里，然后一格也走不动。这不是平衡问题，是硬故障。
///
/// 取 16（14 格 + 2 格余量）而不是干脆取消：SPEC 担心的 1000 格极端情形
/// 仍然被排除在外，而 `try_move` 的搬运代价是 O(格数²)，
/// 16 格时是 256 次比较，可以忽略。
pub const SMALL_BODY_MAX: usize = 16;

/// 自然寿命上限（tick）。到点就死，不商量。
pub const MAX_AGE: u32 = 20_000;

/// 突变率：每个权重被扰动的概率。
pub const MUTATION_RATE: f32 = 0.05;

/// 突变强度（高斯标准差）。
pub const MUTATION_SIGMA: f32 = 0.15;

/// 方向输出的阈值。低于它就不动。
///
/// 没有这个阈值，一个输出恒为 0.02 的基因组会让生物每 tick 都试图移动，
/// 白白烧掉能量——看起来像"生物莫名消瘦"，实际是网络在噪声上做决策。
pub const MOVE_THRESHOLD: f32 = 0.15;

/// 生命层自己的随机源。繁殖突变与将来的发育都用它。
///
/// 与材质层分开：材质层的随机全部是位置派生的（[`crate::rng::Rng::at`]），
/// 不需要状态；生命层的突变是**事件**（"这一次繁殖"），没有位置可言，
/// 所以用顺序 RNG，种子固定即可保证确定性。
const LIFE_SEED: u64 = 0x5EED_1F3E;

/// 光照被上方生物遮挡的检查深度（格）。
///
/// 这一条是种群的**负反馈**：鱼都挤在水面附近时，上面那层会挡住下面那层的光，
/// 产出下降，繁殖自然放缓。没有它，浅水里的能量收入是恒定的，
/// 种群就会指数爆炸——实测 256² 里 400 tick 从 4 条涨到 480 条。
pub const SUN_SHADE_DEPTH: i32 = 12;

/// 遮挡的横向半径。
///
/// **不能只查正上方**：手工鱼是水平游的，它们会排成一排而不是叠罗汉。
/// 只查正上方的遮挡对这种队形完全无效（实测种群卡在 9 条，能量却条条顶格）。
/// 查一个小方柱之后，"这一片水域有多少生物"才真的影响光照。
pub const SUN_SHADE_RADIUS: i32 = 3;

/// 多少格生物才算把光挡掉一半。
///
/// 12 太狠了：一条鱼繁殖后子代就贴在旁边（3 格），遮挡率立刻 25%，
/// 亲代剩下的 200 点能量撑不过十几 tick，于是"生完就死"。
/// 24 让一个中等鱼群只损失三四成光，亲代有机会缓过来。
const SHADE_SATURATION: f32 = 24.0;

/// 最多被遮挡掉多少光。留一点保底，免得一层鱼把下面彻底闷死。
const MAX_SHADE: f32 = 0.85;

/// 繁殖时的领地半径（切比雪夫距离）。
pub const CROWD_RADIUS: i32 = 4;

/// 半径内有多少个同类时不再繁殖。
pub const CROWD_LIMIT: usize = 6;

// ---------------------------------------------------------------------------
// 藻席：海底的可再生食物
// ---------------------------------------------------------------------------

/// 尸体腐烂 + 藻席生长的间隔（tick）。
///
/// 两者都走**周期性全局清扫**：海底是世界最先静下来的地方，塞进分块访问里
/// 就永远不会执行（静止的块没人访问）。
pub const ORGANICS_INTERVAL: u64 = 25;

/// 每次清扫时，一格泡在水里的尸体被分解的概率（百分比）。⇒ 平均 125 tick 消失。
pub const CORPSE_ROT_PERCENT: u32 = 20;

/// 露在空气里的尸体被分解的概率（百分比）。
///
/// 慢七倍——"腐烂是水解过程"这个说法没错，水里的确烂得快得多。
///
/// 但**不能是 0**。最初写的就是 0（"干着的尸体不会烂"），而 0 是一个
/// **吸收态**：鱼会游到水面之上的天空里死掉，那层尸体永远不消失，
/// 实测 8000 tick 堆了 4.3 万格。
pub const DRY_CORPSE_ROT_PERCENT: u32 = 3;

/// 藻长在水面以下多少格之内。
///
/// 用"离水面多深"而不是"光照阈值"：光照是 `1 - y/height`，它把**世界尺寸**
/// 也编进去了——同一个阈值在 512² 和 128² 里对应完全不同的深度。
/// 而这里要表达的是"藻悬浮在透光层"，那是一个深度问题。
pub const ALGAE_DEPTH: i32 = 28;

/// 藻的存量上限（格）。
///
/// **这是"存量"上限，不是"能不能长"的判据。** 早先的版本按"全世界含能量的
/// 残骸格数"封顶 1500，而世界里天然碎屑本身就有 2791 格——上限一开始就满，
/// 一格藻都长不出来。那次错的不是"封顶"这件事，是**判据数错了东西**：
/// 它把天然碎屑和新长出来的藻混成了一件事。现在 `Algae` 是独立材质，
/// `world.count_of(Algae)` 数的就是藻本身。
pub const ALGAE_CAP: usize = 700;

/// 每次清扫长出几格藻。
///
/// 这是"光 → 藻 → 鱼"这条通路的**能量通量**，和 [`PRODUCER_YIELD`] 是同一类
/// 常数：它决定这个生态最多能多养活多少东西，而不是"长得多快"。
///
/// 30 格 / 25 tick ≈ 144 能量/tick。对照：一条 4 生产者的鱼在浅水晒光约
/// 67 能量/tick，五十条鱼加起来是三千多。所以藻是**补剂**，不是主粮——
/// 想让鱼真的靠觅食活着，得先压 [`PRODUCER_YIELD`]，那是另一件事。
///
/// 取 30 而不是 6：6 的时候藻一长出来就被吃光，存量长期在个位数，
/// **看不见**。藻是这一步要展示的东西，它得能形成一层看得见的悬浮物。
pub const ALGAE_GROWTH_PER_SWEEP: usize = 30;

/// 为长一格藻，最多试几个位置。
///
/// 位置是随机撒的（透光层里空气占了大半），所以大部分尝试会落在不能长的地方。
const ALGAE_GROWTH_TRIES: usize = 40;

/// 藻生长用的随机种子盐。
const SALT_ALGAE_SPAWN: u64 = 0xA16A_E5EE_D000_0000;

/// 这一格是不是泡在水下。
///
/// 沿列向上找，**跳过其他固体**——尸体和碎屑会堆成堆，堆内部的格子四周全是
/// 同类，但它们确实泡在水下，照样该烂。遇到水算在水下，遇到空气或固体就不算。
///
/// 从材质层搬过来的：腐烂规则现在住在生命层（它要记账），
/// 这个判据跟着一起搬，免得两处各写一份、慢慢走样。
fn underwater(world: &World, x: usize, y: usize) -> bool {
    for ny in (0..y).rev() {
        match world.get(x as isize, ny as isize) {
            // 藻是水里的悬浮物：泡在藻层里也算泡在水里。
            // 不算的话，水面那层藻下面的尸体会被当成"露在空气里"，
            // 按干腐烂变成空位——而它那一格水本来是身体借走的，于是水量
            // 每死一条贴着藻层的鱼就少一格（实测 32000 tick 少了 750 格）。
            MaterialKind::Water | MaterialKind::Algae => return true,
            // 空气意味着这一格露在水面之上，不烂。
            MaterialKind::Empty => return false,
            _ => continue,
        }
    }
    false
}

/// 四邻域偏移。捕食判定用它找"隔壁是谁"。
const NEIGHBORS_4: [(i32, i32); 4] = [(0, -1), (-1, 0), (1, 0), (0, 1)];

/// 尸体腐烂用的位置派生随机盐。必须和材质层的盐不同。
const SALT_CORPSE_ROT: u32 = 0xDEAD;

/// 一格藻蕴含的生物可用能量。
///
/// 这个数字必须**压过"待在水面晒光"的净收益**，否则觅食就是个坏主意：
/// 鱼会闻着味道下潜、花掉一截移动成本、吃到一点点，然后发现还不如待在上面。
///
/// 三格鱼的账：水面净收益约 +26/tick；一口藻吃一格，所以藻的能量
/// 必须明显高于"维持成本 37 + 那 26"。取 120 时，坐在藻席上吃是
/// **+83/tick**——值得专门跑一趟，而且吃几口就饱（能量上限会挡住）。
///
/// 第一版取 40 的实测结果是**负收益**：种群从 52 掉到 37，
/// 细胞数从 688 掉到 442。不是藻没长出来，是长得再多也不值得去。
pub const FOOD_ENERGY: u32 = 120;

// ---------------------------------------------------------------------------
// 捕食
// ---------------------------------------------------------------------------

/// 一枚杀手细胞每 tick 从邻格猎物身上咬下多少能量。
///
/// 捕食是**能量转移**，不是注入：猎物少多少，捕食者就多多少（装不下的部分
/// 记进耗散）。所以它不会破坏守恒，只会改变能量的分布——
/// 这正是"捕食会带来频率依赖选择"的物理来源。
pub const PREDATION_BITE: u32 = 30;

/// 猎物身上每有一格装甲，一口咬下去少掉多少。
///
/// 这一条给了装甲一个**存在的理由**。在此之前装甲只是"加重、不产出"的配重，
/// 纯粹是设计上的减分项——叶背鱼驮着三格装甲，除了变重什么也没换来。
///
/// 取 8：叶背鱼（3 装甲）只挨 6 点，水母（2 装甲）挨 14，
/// 银鱼和蠕虫一格装甲都没有，挨满 30。**软的身体成为猎物**，
/// 于是"要不要长装甲"第一次是个真问题。
pub const ARMOR_ABSORB: u32 = 8;

/// 环境容纳量：全世界生产者细胞数的软上限。
///
/// 这是**第三道、也是最后一道负反馈**，前两道（局部遮挡、领地）都是"位置相关"的，
/// 一旦鱼水平散开就都失效了。
///
/// 为什么需要它：把左右边界改成环绕之后，鱼不再堆在右墙，于是局部遮挡和领地
/// 都不再触发——实测 512² 从 8 条一路涨到 **1737 条**，残骸堆满 26% 的世界，
/// 5.5 ms/tick。那个"240 条的平衡"其实是几何假象，不是生态。
///
/// 物理含义是"水体里的营养总量是有限的"：生产者越多，每个分到的越少。
/// 双曲衰减 `1 / (1 + P/CAP)` 比线性截断温和——永远不会突然归零，
/// 只会越来越薄，所以种群会平滑地停在平衡点附近而不是硬撞天花板。
///
/// 推导平衡点：单条鱼净收益 = `PRODUCER_YIELD × 光 × comp − 维持成本`。
/// 令其为零、取浅水光 ≈ 0.8、维持 15，得 `P ≈ 0.28 × CAP`。
/// CAP = 800 对应 **≈ 220 条**，与实测的 240 吻合。
pub const CARRYING_CAPACITY: f32 = 800.0;

/// 生成失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnError {
    /// 身体有一部分落在世界之外。
    OutOfBounds,
    /// 目标格已经被别的细胞占了。
    CellOccupied,
    /// 目标格是固体（石 / 沙 / 残骸），身体放不进去。
    Blocked,
    /// 身体是空的。
    EmptyBody,
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfBounds => write!(f, "organism body leaves the world"),
            Self::CellOccupied => write!(f, "target cell is already occupied"),
            Self::Blocked => write!(f, "target cell is solid"),
            Self::EmptyBody => write!(f, "organism has no cells"),
        }
    }
}

impl std::error::Error for SpawnError {}

/// 能量账。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EnergyLedger {
    /// 累计注入：初始残骸能量 + 初始生物能量 + 太阳能。
    pub injected: u64,
    /// 累计耗散：维持成本 + 死亡时的零头。
    pub dissipated: u64,
}

/// 一次能量审计的结果。
///
/// `accounted() == injected` 就是守恒。不相等时 `leak()` 直接告诉你差多少，
/// 正数是凭空多出来的，负数是漏掉的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnergyAudit {
    /// 所有活着的有机体身上的能量。
    pub organisms: u64,
    /// 环境里所有残骸蕴含的能量（残骸格的 `flags` 之和）。
    pub remains: u64,
    /// 累计耗散。
    pub dissipated: u64,
    /// 累计注入。
    pub injected: u64,
}

impl EnergyAudit {
    /// 账面上应有的总量。
    pub fn accounted(&self) -> u64 {
        self.organisms + self.remains + self.dissipated
    }

    /// 账实差额。0 表示守恒。
    pub fn leak(&self) -> i64 {
        self.accounted() as i64 - self.injected as i64
    }

    /// 是否严格守恒。
    pub fn is_balanced(&self) -> bool {
        self.leak() == 0
    }
}

/// 一 tick 的生命层统计。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct LifeStats {
    /// 活着的有机体数。
    pub organisms: usize,
    /// 活着的细胞总数。
    pub cells: usize,
    /// 所有有机体的能量之和。
    pub energy: u64,
    /// 本 tick 出生数。
    pub born: u32,
    /// 本 tick 死亡数。
    pub died: u32,
    /// 本 tick 饿死的数量（死亡的一个子集）。
    pub starved: u32,
    /// 本 tick 吃掉的残骸格数。
    pub eaten: u32,
    /// 本 tick 发生的捕食次数（一枚杀手细胞咬到一口算一次）。
    pub bitten: u32,
    /// 最小的身体有多少格。
    ///
    /// 和 `max_cells` 一起看：形态发生模式下这两个数会**分开**——
    /// 一开始所有身体都是 9~14 格，跑上几万 tick 之后如果 min 掉到 4、max 涨到 18，
    /// 就说明突变真的改到了形状。只看平均值是看不出来的（平均值不动，
    /// 可能只是个体大小在两端互换）。
    pub min_cells: usize,
    /// 最大的身体有多少格。
    pub max_cells: usize,
    /// 当前活着的杀手细胞数。
    ///
    /// 这个数归零就说明**捕食者灭绝了**——而"生物数""细胞数""能量账"
    /// 三项全都看不出这件事。捕食者灭绝后世界看起来一切正常，
    /// 只是少了一整个营养级。
    pub killers: usize,
    /// 本 tick 新长出的藻席格数。
    pub food_grown: u32,
    /// 本 tick 腐烂掉的尸体格数。
    ///
    /// 和 `food_grown` 是一对：一个进、一个出。生态有没有在"堆尸体"，
    /// 看这两个数就够了——它们长期失衡时，`Corpse` 的格数会一路涨。
    pub corpses_rotted: u32,
    /// 当前种群的最大代数。
    pub max_generation: u32,
    /// 当前种群的平均代数。
    pub avg_generation: f32,
    /// 当前种群与**初代**基因组的平均权重距离（每个权重的平均绝对差）。
    ///
    /// 它涨说明基因确实在漂移；但它涨**不等于**适应度提高——
    /// 中性漂移也会让它涨。要区分这两者，得看 [`LifeStats::energy`] 是否同步上升。
    pub gene_distance: f32,
    /// 整个身体都不挨着水的个体数（"飘在天上"）。
    ///
    /// 判据是"每一格的上下左右都不是水/气"。不能看格子本身的材质——
    /// 身体格的材质永远是 `Empty`，那是材质层与生命层的互斥约定。
    ///
    /// 正常恒为 0：移动和出生两道门都要求"至少一格泡在水/气里"。
    /// 它一旦不为 0，说明有谁绕过了那两道门——而这些个体既不能动也不能繁殖，
    /// 只会坐在原地把能量晒满，从"生物数"上看完全正常。
    ///
    /// 这个指标是被一次真实的崩坏逼出来的：去掉那道门槛之后，鱼靠繁殖
    /// 一代代往上爬进天空，种群从 145 掉到 11，而能量账一分不差、画面上
    /// 也看不出异常。
    pub stranded: usize,
}

/// 生命层调度器。
#[derive(Debug)]
pub struct LifeSim {
    organisms: Vec<Organism>,
    next_id: OrganismId,
    ledger: EnergyLedger,
    rng: Rng,
    /// 初代基因组，用来度量"种群漂了多远"。取第一次投放的那一条。
    founder: Option<Genome>,
    stats: LifeStats,
    /// 海底要不要长藻。默认开，见 [`LifeSim::set_food_growth`]。
    food_growth: bool,
}

impl Default for LifeSim {
    /// 空世界的调度器。能量账从 0 开始，没有任何有机体。
    ///
    /// 注意它**不等于** [`LifeSim::new`]：`new` 会把世界里已有的残骸计入注入，
    /// 而这里没有世界可看。所以正常流程一律走 `new`。
    fn default() -> Self {
        Self {
            organisms: Vec::new(),
            next_id: 1,
            ledger: EnergyLedger::default(),
            rng: Rng::new(LIFE_SEED),
            founder: None,
            stats: LifeStats::default(),
            food_growth: true,
        }
    }
}

impl LifeSim {
    /// 新建调度器，并把世界里**已有**的残骸当作初始能量注入。
    ///
    /// 地貌生成器撒下的碎屑不是生物尸体，但它们确实是可吃的能量，
    /// 所以从第一天起就得进账——否则第一次有东西啃了一口天然残骸，
    /// 守恒断言就会莫名其妙地多出 25 点能量。
    pub fn new(world: &mut World) -> Self {
        let mut ledger = EnergyLedger::default();
        for idx in 0..world.cells().len() {
            let cell = world.cells()[idx];
            if !matches!(cell.kind, MaterialKind::Remains | MaterialKind::Corpse) {
                continue;
            }
            let (x, y) = (idx % world.width(), idx / world.width());
            // 记的是这一格**实际**带多少能量，不是固定值。
            // 已经带值的残骸（读档、上一轮初始化过）必须按原值入账，
            // 否则账面上会凭空多出"标称值 − 实际值"。
            let energy = if cell.flags == 0 {
                world.set_flags(x as isize, y as isize, REMAINS_ENERGY as u8);
                REMAINS_ENERGY as u64
            } else {
                cell.flags as u64
            };
            ledger.injected += energy;
        }
        Self {
            organisms: Vec::new(),
            next_id: 1,
            ledger,
            rng: Rng::new(LIFE_SEED),
            founder: None,
            stats: LifeStats::default(),
            food_growth: true,
        }
    }

    /// 打开/关闭藻席生长。
    ///
    /// 关掉它是为了**对照实验**：想知道藻席到底改变了什么，就得能跑一个
    /// 没有它的世界。顺带也让那些"精确数残骸格数"的单元测试不被海底长出来的
    /// 藻干扰——但那些测试关它是因为**测的是别的东西**，不是因为藻是噪声。
    pub fn set_food_growth(&mut self, on: bool) {
        self.food_growth = on;
    }

    /// 当前的统计。
    #[inline]
    pub fn stats(&self) -> LifeStats {
        self.stats
    }

    /// 当前的账本。
    #[inline]
    pub fn ledger(&self) -> EnergyLedger {
        self.ledger
    }

    /// 所有有机体。
    #[inline]
    pub fn organisms(&self) -> &[Organism] {
        &self.organisms
    }

    /// 所有有机体（可变）。
    #[inline]
    pub fn organisms_mut(&mut self) -> &mut [Organism] {
        &mut self.organisms
    }

    /// 按编号找有机体。
    pub fn organism(&self, id: OrganismId) -> Option<&Organism> {
        self.organisms.iter().find(|o| o.id == id)
    }

    /// 往世界里放一个有机体，**不**记注入账。
    ///
    /// 用于"能量从别处来"的场景：繁殖（子代的能量是亲代给的）、读档。
    /// 凭空造一个生物请用 [`LifeSim::spawn_injected`]。
    ///
    /// 先把身体每一格都验一遍（在界内、无主），全部通过才落子。
    /// 半途失败会留下一个缺了几块的身体，那比放不进去更难查。
    pub fn spawn(
        &mut self,
        world: &mut World,
        mut org: Organism,
    ) -> Result<OrganismId, SpawnError> {
        if org.cells.is_empty() {
            return Err(SpawnError::EmptyBody);
        }
        // 水平环绕：先告诉有机体世界有多宽，之后 `cell_at` 自己取模。
        // 必须在验证之前设——否则贴着右边界的身体会被算成越界。
        org.width = world.width() as i32;
        // 锚点也折回界内：调用方可能传进一个刚超出边界的坐标。
        org.anchor.0 = org.wrap_x(org.anchor.0);
        // 第一遍只验证，不动世界——半途失败会留下一个缺了几块的身体，
        // 那种残缺比"放不进去"难查得多。
        for cell in &org.cells {
            let (x, y) = org.cell_at(cell);
            if !Self::can_place(world, x, y) {
                return Err(if !world.in_bounds(x as isize, y as isize) {
                    SpawnError::OutOfBounds
                } else if world.is_occupied(x as isize, y as isize) {
                    SpawnError::CellOccupied
                } else {
                    SpawnError::Blocked
                });
            }
        }
        // 第二遍落地：把身体格腾空，然后占格。
        //
        // ## 那一格水去哪了：被**借走**了
        //
        // 身体占住的那格水记在细胞头上（`Cell::wet`），变成尸体、
        // 尸体腐烂或被吃掉时还回来。**债务在出生这一刻定死**，
        // 不能到死的时候按"当时泡不泡在水里"倒推——那样一条在空气里出生的鱼
        // 沉下去死了，会还出一格它从没借过的水。实测 32000 tick 多了一万八千格。
        // 落点先算好再动 cells：`cell_at` 要借 `org`，而下面要 `&mut org.cells`。
        let spots: Vec<(i32, i32)> = org.cells.iter().map(|c| org.cell_at(c)).collect();
        let mut crushed = 0u64;
        for (cell, &(x, y)) in org.cells.iter_mut().zip(&spots) {
            let kind = world.get(x as isize, y as isize);
            // 只有水和藻才算"借"。气体、火被挤掉不算债务——它们本来就不是水。
            //
            // 藻算借，是因为**藻就是水里的悬浮物**：那一格在体积账上本来记成
            // 藻，身体占住它之后记成细胞，死后变成尸体、烂掉再变回水——
            // 一路都在同一个守恒量里。写成 `wet = false` 的话这条链断在中间，
            // 每吃掉一格藻就少一格水。
            cell.wet = matches!(kind, MaterialKind::Water | MaterialKind::Algae);
            if kind == MaterialKind::Algae {
                // 被身体挤掉的那格藻没被吃掉，能量变成热。
                //
                // 必须记账：藻的能量是记在 `flags` 里、由 `audit` 数进
                // "残骸"那一栏的。悄悄清掉它，守恒断言会报"少了 N 点"，
                // 而报错指向的是生命层，查起来毫无头绪。
                crushed += world.flags(x as isize, y as isize) as u64;
            }
            if kind.is_fluid() || kind == MaterialKind::Algae {
                world.set(x as isize, y as isize, MaterialKind::Empty);
            }
        }
        self.ledger.dissipated += crushed;

        org.id = self.next_id;
        self.next_id += 1;
        org.claim(world);

        let id = org.id;
        self.organisms.push(org);
        Ok(id)
    }

    /// 往世界里放一个有机体，并把它身上的能量记为"注入"。
    ///
    /// 初始投放与玩家投放走这条路。用错了会怎样：忘了记账 ⇒ 守恒断言报
    /// "少了 N 点"；多记一次 ⇒ 报"多了 N 点"。两种都会在测试里立刻显形。
    pub fn spawn_injected(
        &mut self,
        world: &mut World,
        org: Organism,
    ) -> Result<OrganismId, SpawnError> {
        let energy = org.energy as u64;
        if self.founder.is_none() {
            self.founder = Some(org.genome.clone());
        }
        let id = self.spawn(world, org)?;
        self.ledger.injected += energy;
        Ok(id)
    }

    /// 把一个有机体从世界里抹掉（用于测试与"玩家删除"）。
    ///
    /// 身上的能量记入耗散，不留残骸。
    pub fn despawn(&mut self, world: &mut World, id: OrganismId) {
        if let Some(i) = self.organisms.iter().position(|o| o.id == id) {
            let org = self.organisms.remove(i);
            org.release(world);
            self.ledger.dissipated += org.energy as u64;
        }
    }

    /// 初代基因组。没投放过的世界返回 `None`。
    #[inline]
    pub fn founder(&self) -> Option<&Genome> {
        self.founder.as_ref()
    }

    /// 当前种群与初代基因组的平均距离（每个权重的平均绝对差）。
    ///
    /// 这是 M3 的第一个观测指标，也是最容易被误读的一个：
    /// **它涨不等于适应度提高**。突变是中性的，没有选择压力时基因照样会漂移，
    /// 只是漂移是随机游走、不会累积成方向。
    /// 判断"选择有没有发生"要看它与平均能量的关系，而不是它本身。
    pub fn gene_distance(&self) -> f32 {
        let Some(founder) = &self.founder else {
            return 0.0;
        };
        if self.organisms.is_empty() {
            return 0.0;
        }
        let sum: f32 = self
            .organisms
            .iter()
            .map(|o| genome_distance(&o.genome, founder))
            .sum();
        sum / self.organisms.len() as f32
    }

    /// 审计能量账。**不是**每 tick 都该调用——它要扫一遍整张网格。
    pub fn audit(&self, world: &World) -> EnergyAudit {
        let mut remains = 0u64;
        for cell in world.cells() {
            if matches!(
                cell.kind,
                MaterialKind::Remains | MaterialKind::Corpse | MaterialKind::Algae
            ) {
                remains += cell.flags as u64;
            }
        }
        EnergyAudit {
            organisms: self.organisms.iter().map(|o| o.energy as u64).sum(),
            remains,
            dissipated: self.ledger.dissipated,
            injected: self.ledger.injected,
        }
    }

    /// 推进一个 tick 的生命层。
    ///
    /// 顺序：太阳能 → 有机物（腐烂 / 长藻）→ 行为（移动 / 进食 / 繁殖）→ 捕食
    /// → 代谢（可能饿死）→ 清理尸体 → 统计。
    ///
    /// 行为放在代谢之前，是因为"这一 tick 能动"应该由"这一 tick 开始时还剩多少能量"
    /// 决定。反过来的话，一个刚好付得起维持成本的生物会先被扣光、再判定能不能动，
    /// 于是永远动不了——那是一个很难从现象反推的死结。
    pub fn step(&mut self, world: &mut World) {
        self.stats = LifeStats::default();

        // 1) 太阳能。生产者是唯一的"环境 → 生物"入口（SPEC §3.3）。
        self.inject_sunlight(world);

        // 1b) 尸体腐烂 + 藻的生长。第二条能量注入通道，见 `tick_organics`。
        self.tick_organics(world);

        // 2) 行为。
        self.act(world);

        // 2b) 捕食。放在行为之后：这一 tick 游到嘴边的猎物，这一 tick 就该被咬。
        self.predate(world);

        // 3) 代谢。付不起维持成本就饿死。
        let mut starved: Vec<usize> = Vec::new();
        let mut dissipated = 0u64;
        for (i, org) in self.organisms.iter_mut().enumerate() {
            org.age += 1;
            for cell in &mut org.cells {
                cell.age += 1;
            }
            let upkeep = org.upkeep();
            if org.age >= MAX_AGE || org.energy < upkeep {
                // 老死与饿死走同一条埋葬路径。这里**不**动 energy：
                // 剩下的那点（哪怕不够付一次维持成本）会随尸体进入残骸，
                // 由 `bury` 统一记账。在这里先扣一遍会让它被算两次。
                starved.push(i);
            } else {
                org.energy -= upkeep;
                dissipated += upkeep as u64;
            }
        }
        self.ledger.dissipated += dissipated;
        self.stats.starved = starved.len() as u32;

        // 4) 清理。逆序删，下标才不会错位。
        for &i in starved.iter().rev() {
            let org = self.organisms.remove(i);
            self.bury(world, &org);
            self.stats.died += 1;
        }

        // 4b) 能量走势采样。检查器要画一条"这个身体最近过得怎么样"的曲线，
        //     而这条曲线只能在每 tick 走完之后采——代谢刚扣完钱，那才是
        //     它真正拿到的能量。
        if world.tick() % crate::life::ENERGY_HIST_INTERVAL == 0 {
            for org in &mut self.organisms {
                org.sample_energy();
            }
        }

        // 5) 统计。
        self.stats.organisms = self.organisms.len();
        self.stats.cells = self.organisms.iter().map(|o| o.cell_count()).sum();
        self.stats.min_cells = self
            .organisms
            .iter()
            .map(|o| o.cell_count())
            .min()
            .unwrap_or(0);
        self.stats.max_cells = self
            .organisms
            .iter()
            .map(|o| o.cell_count())
            .max()
            .unwrap_or(0);
        self.stats.stranded = self
            .organisms
            .iter()
            .filter(|o| {
                let cells: Vec<(i32, i32)> = o.footprint().collect();
                !Self::touches_water(world, o.width, &cells)
            })
            .count();
        self.stats.killers = self
            .organisms
            .iter()
            .map(|o| {
                o.cells
                    .iter()
                    .filter(|c| c.kind == CellType::Killer)
                    .count()
            })
            .sum();
        self.stats.energy = self.organisms.iter().map(|o| o.energy as u64).sum();
        self.stats.max_generation = self
            .organisms
            .iter()
            .map(|o| o.generation)
            .max()
            .unwrap_or(0);
        self.stats.avg_generation = if self.organisms.is_empty() {
            0.0
        } else {
            self.organisms
                .iter()
                .map(|o| o.generation as f32)
                .sum::<f32>()
                / self.organisms.len() as f32
        };
        self.stats.gene_distance = self.gene_distance();
    }

    /// 让一个有机体变成残骸。
    ///
    /// **尸体携带的是它死时还剩的能量，没有额外的基础值。**
    /// 这一点是刻意的：如果尸体固定带 25 点，那么"饿死 → 变残骸 → 被吃"
    /// 就成了一条凭空造能量的永动机，守恒断言立刻失效。
    /// 代价是饿死的尸体是空的——物理上也说得通，它已经被自己烧光了。
    ///
    /// 每格分到 `energy / 细胞数`，摊不尽的零头、以及超过 `u8` 上限的部分
    /// 都记进耗散。用 `flags` 存能量而不是另开一张表：材质层交换格子时
    /// 会连 `flags` 一起搬，于是残骸沉降、被水冲走，能量都跟着走——
    /// 这一点是免费的。
    fn bury(&mut self, world: &mut World, org: &Organism) {
        let n = org.cells.len().max(1) as u32;
        // 每格分不到 4 点能量的尸体视为"已经被自己烧光了"：flags 直接写 0。
        // （腐烂已经不看 `flags` 了——那套判据被 `Corpse` 材质取代。
        // 这里仍然把零头归零，只是为了不留下"1 点能量的尸体"这种噪声。）
        let per_cell = org.energy / n;
        let per_cell = if per_cell < 4 {
            0
        } else {
            per_cell.min(u8::MAX as u32)
        };
        let mut placed = 0u32;
        for cell in &org.cells {
            let (x, y) = org.cell_at(cell);
            let (x, y) = (x as isize, y as isize);
            world.vacate(x, y);
            // **没借过水的细胞不留下尸体。**
            //
            // 出生时泡在空气里那几格从没占过水，死后也不该变成"会烂成水"的
            // 尸体——否则凭空多出几格水（见 `Cell::wet` 的注释）。
            if cell.wet {
                world.set(x, y, remains_of(cell.kind));
                // `set` 会把 flags 清 0，所以能量必须写在它之后。
                world.set_flags(x, y, per_cell as u8);
                placed += 1;
            }
        }
        // 摊不尽的零头 + **没能落到任何一格上的那份**（干细胞的份额）都要记账。
        // 少记这一项，守恒断言会报"少了 N 点"——实测 32000 tick 少了 22657 点。
        self.ledger.dissipated += (org.energy - per_cell * placed) as u64;
    }

    /// 捕食：杀手细胞咬邻格的其他生物。
    ///
    /// ## 为什么是自动的，不是网络输出
    ///
    /// 没有 `OUT_BITE` 这一路输出，也刻意不加。理由和"手工基因组不该被硬塞
    /// 觅食策略"（§3.11）是同一条：**要不要当捕食者，是长成什么样决定的。**
    /// 杀手细胞自己就要付维持成本（3/tick，和生产者一样贵），
    /// 养不起就饿死——这已经是足够强的选择压力了。
    ///
    /// 再加一个"咬不咬"的输出，只会让网络有机会长出"长了牙但不用"的个体，
    /// 而那是个纯亏的局部最优。
    ///
    /// ## 能量是转移，不是凭空产生
    ///
    /// 猎物少 `dealt`，捕食者最多拿 `dealt`（受自己能量上限限制），
    /// 装不下的那份记进耗散——咬了但咽不下去，能量变成热。
    /// 这样守恒断言依然成立，而"捕食"只改变能量的分布，不改变总量。
    fn predate(&mut self, world: &World) {
        // 先收集再结算：结算要同时改两个有机体，边遍历边改借不过编译器。
        let mut bites: Vec<(OrganismId, OrganismId)> = Vec::new();
        for org in &self.organisms {
            let mut killers = org.cells.iter().filter(|c| c.kind == CellType::Killer);
            if killers.next().is_none() {
                continue;
            }
            for cell in org.cells.iter().filter(|c| c.kind == CellType::Killer) {
                let (kx, ky) = org.cell_at(cell);
                for (dx, dy) in NEIGHBORS_4 {
                    let nx = org.wrap_x(kx + dx);
                    let ny = ky + dy;
                    let owner = world.owner(nx as isize, ny as isize);
                    if owner.organism != 0 && owner.organism != org.id {
                        bites.push((org.id, owner.organism));
                    }
                }
            }
        }
        if bites.is_empty() {
            return;
        }

        for (predator, prey) in bites {
            let Some(qi) = self.organisms.iter().position(|o| o.id == prey) else {
                continue;
            };
            let armor = self.organisms[qi]
                .cells
                .iter()
                .filter(|c| c.kind == CellType::Armor)
                .count() as u32;
            let bite = PREDATION_BITE.saturating_sub(armor * ARMOR_ABSORB);
            let dealt = bite.min(self.organisms[qi].energy);
            if dealt == 0 {
                continue;
            }
            let Some(pi) = self.organisms.iter().position(|o| o.id == predator) else {
                continue;
            };
            self.organisms[qi].energy -= dealt;
            let room = self.organisms[pi]
                .max_energy()
                .saturating_sub(self.organisms[pi].energy);
            let gained = dealt.min(room);
            self.organisms[pi].energy += gained;
            self.ledger.dissipated += (dealt - gained) as u64;
            self.stats.bitten += 1;
        }
    }

    /// 生产者按光照产能量。
    ///
    /// 光从世界顶端射入，随深度线性衰减到 0。这条梯度是"生产者必须待在浅水、
    /// 而浅水正是掠食者猎场"的物理来源（SPEC §2.0）——
    /// 如果光照处处相同，生物就没有理由浮上来，也就没有垂直方向的生态位。
    ///
    /// 还要再乘一个**遮挡系数**：上方每一格被别的生物占住，就挡掉一部分光。
    /// 这一条是种群的负反馈，没有它浅水的能量收入恒定，种群指数爆炸
    /// （实测 256² 里 400 tick 从 4 条涨到 480 条，还在涨）。
    fn inject_sunlight(&mut self, world: &mut World) {
        let height = world.height().max(1) as f32;

        // 环境容纳量：生产者越多，每个分到的光越少。
        // 算一次总数就够了——它是全局量，不随位置变。
        let total_producers: u32 = self
            .organisms
            .iter()
            .map(|o| o.count(CellType::Producer) as u32)
            .sum();
        let competition = 1.0 / (1.0 + total_producers as f32 / CARRYING_CAPACITY);

        let mut injected = 0u64;
        for org in &mut self.organisms {
            let producers = org.count(CellType::Producer);
            if producers == 0 {
                continue;
            }
            let (ax, ay) = org.anchor;
            let depth = (ay.max(0) as f32 / height).clamp(0.0, 1.0);

            // 这一小片水域里有多少格被**别人**占着。自己的格子不算——
            // 否则一条鱼只要生产者长在自己头顶就永远在自我遮挡。
            let mut blocked = 0i32;
            for dy in 1..=SUN_SHADE_DEPTH {
                for dx in -SUN_SHADE_RADIUS..=SUN_SHADE_RADIUS {
                    let owner = world.owner((ax + dx) as isize, (ay - dy) as isize);
                    if owner.is_some() && owner.organism != org.id {
                        blocked += 1;
                    }
                }
            }
            let shade = (blocked as f32 / SHADE_SATURATION).min(MAX_SHADE);

            let light = (1.0 - depth) * (1.0 - shade) * competition;
            let gain = (PRODUCER_YIELD as f32 * light) as u32 * producers as u32;
            if gain == 0 {
                continue;
            }
            let room = org.max_energy().saturating_sub(org.energy);
            let gain = gain.min(room);
            org.energy += gain;
            // 只记真正加进去的那份。被上限截掉的、以及浮点转整数丢掉的零头
            // 从未进入生物可用池，不该出现在"注入"里。
            injected += gain as u64;
        }
        self.ledger.injected += injected;
    }

    /// 海底的日常维护：**尸体腐烂** + **藻席生长**。
    ///
    /// 两件事合在一个扫描里，因为它们都是"全网格、低频、和格子位置无关"的工作，
    /// 分开写就要扫两遍世界（262144 格 × 2 / 25 tick，实测能吃掉四成帧率）。
    ///
    /// ## 尸体腐烂
    ///
    /// 只有 [`MaterialKind::Corpse`]（生命制造的）会烂，
    /// [`MaterialKind::Remains`]（worldgen 撒下的天然碎屑）永远稳定——
    /// 否则"没有生物的世界也能完全静止"这条就不成立了。
    ///
    /// 早先两者是同一种材质，靠 `flags == 0` 来区分"尸体"和"碎屑"。
    /// 那个判据混淆了两件事，而且**带能量的尸体永远不烂**：
    /// 实测残骸占比从 1.7% 涨到 3.3% 还在爬。
    ///
    /// 腐烂要把尸体身上剩下的能量记进 `dissipated`：那是热量，
    /// 离开了生物可用池但仍在账上。这正是"腐烂规则必须住在生命层"的理由——
    /// 材质层没有账本，它悄悄清掉一格带能量的尸体，守恒断言就会报"多了 N 点"。
    ///
    /// ## 藻的生长
    ///
    /// 藻**悬浮在水面以下的一层水里**，不在海底。
    ///
    /// 这个位置是被两次失败逼出来的：
    ///
    /// 1. 长在海底时鱼**够不着**。海深 184 格，一趟下潜往返的移动成本
    ///    约 `质量 × 距离 ≈ 13 × 368 ≈ 4800`，而一格藻只有 120 能量——
    ///    整片藻席吃下去也回不了本。食物放在鱼到不了的地方等于没有食物。
    /// 2. 悬在水里但**是固体**时会把水流切碎。3000 格静态藻让整个世界静止
    ///    （`移动 0`）、全部鱼被顶进空气里。
    ///
    /// 所以现在：**悬浮（不挡路）+ 被水挤开（会漂）+ 长在透光层（够得着）**。
    /// 三者缺一不可，前两条在材质层（见 `MaterialKind::Algae` 与
    /// `MoveRule::Drifts`），这一条在生命层。
    ///
    /// ## 为什么是"每次撒几格"而不是"每格掷骰子"
    ///
    /// 藻长在约两万格水里，全扫一遍每 25 tick 一次，而每次只需要长出
    /// [`ALGAE_GROWTH_PER_SWEEP`] 格——扫描的 99.9% 是白跑。随机撒点
    /// 是 O(尝试次数)，和世界大小无关，而且**通量是常数**：
    /// 决定"藻能养活多少东西"的是这个常数，不是世界有多大。
    fn tick_organics(&mut self, world: &mut World) {
        if world.tick() % ORGANICS_INTERVAL != 0 {
            return;
        }
        let tick = world.tick();
        let (w, h) = (world.width(), world.height());
        let mut rotted = 0u64;
        let mut rotted_cells = 0u32;

        for y in 0..h {
            for x in 0..w {
                let (xs, ys) = (x as isize, y as isize);
                if world.get(xs, ys) != MaterialKind::Corpse {
                    continue;
                }
                let wet = underwater(world, x, y);
                let percent = if wet {
                    CORPSE_ROT_PERCENT
                } else {
                    DRY_CORPSE_ROT_PERCENT
                };
                if !Rng::at_percent(x, y, tick, SALT_CORPSE_ROT, percent) {
                    continue;
                }
                rotted += world.flags(xs, ys) as u64;
                rotted_cells += 1;
                // 泡在水里的尸体烂掉之后**变回水**——那一格水是身体出生时
                // 借走的，身体彻底消失了就该还回来。露在空气里的变成空位。
                world.set(
                    xs,
                    ys,
                    if wet {
                        MaterialKind::Water
                    } else {
                        MaterialKind::Empty
                    },
                );
            }
        }

        // 腐烂的能量是热量：离开生物可用池，但仍在账上。
        self.ledger.dissipated += rotted;
        self.stats.corpses_rotted = rotted_cells;

        let grown = if self.food_growth {
            self.grow_algae(world)
        } else {
            0
        };
        // 藻的能量是新进入生物可用池的（光 → 藻 → 鱼），记注入。
        self.ledger.injected += grown as u64 * FOOD_ENERGY as u64;
        self.stats.food_grown = grown;
    }

    /// 在水面以下几格的水里撒几格藻。返回实际长出来几格。
    ///
    /// 位置是**随机撒的**，不是扫描出来的：透光层有两万格，而每次只长几格，
    /// 扫一遍是纯浪费。代价是大部分尝试会落在空气或已占用的格子上——
    /// 所以每个位置要试 [`ALGAE_GROWTH_TRIES`] 次。
    fn grow_algae(&mut self, world: &mut World) -> u32 {
        if world.count_of(MaterialKind::Algae) >= ALGAE_CAP {
            return 0;
        }
        let (w, h) = (world.width(), world.height());
        if w == 0 || h == 0 {
            return 0;
        }
        // 每个清扫周期用一个独立的随机源，种子只和 tick 有关——所以
        // 同一个世界、同一个种子，藻长在哪里是可复现的。
        let mut rng = Rng::new(world.tick().wrapping_mul(0x9E37_79B9) ^ SALT_ALGAE_SPAWN);
        let mut grown = 0u32;
        for _ in 0..ALGAE_GROWTH_PER_SWEEP {
            if world.count_of(MaterialKind::Algae) >= ALGAE_CAP {
                break;
            }
            for _ in 0..ALGAE_GROWTH_TRIES {
                let x = rng.gen_range_usize(0, w);
                // 先找这一列的水面：从顶往下第一格水。找不到就是陆地或空列。
                let Some(surface) =
                    (0..h).find(|&y| world.get(x as isize, y as isize) == MaterialKind::Water)
                else {
                    continue;
                };
                let depth = rng.gen_range_usize(0, ALGAE_DEPTH as usize);
                let y = surface + depth;
                if y >= h {
                    continue;
                }
                let (xs, ys) = (x as isize, y as isize);
                if world.get(xs, ys) != MaterialKind::Water || world.is_occupied(xs, ys) {
                    continue;
                }
                world.set(xs, ys, MaterialKind::Algae);
                world.set_flags(xs, ys, FOOD_ENERGY as u8);
                grown += 1;
                break;
            }
        }
        grown
    }

    /// 世界里有多少格含能量的食物（藻席 + 有能量的残骸）。
    ///
    /// 这是"有多少吃的"，不是"有多少藻"——天然碎屑与带能量的尸体也算。
    /// 全网格扫描，所以只在报告里调，不要每 tick 调。
    pub fn food_cells(&self, world: &World) -> usize {
        let (w, h) = (world.width(), world.height());
        let mut n = 0;
        for y in 0..h {
            for x in 0..w {
                let (xs, ys) = (x as isize, y as isize);
                if matches!(
                    world.get(xs, ys),
                    MaterialKind::Remains | MaterialKind::Corpse | MaterialKind::Algae
                ) && world.flags(xs, ys) > 0
                {
                    n += 1;
                }
            }
        }
        n
    }

    // -----------------------------------------------------------------------
    // 行为
    // -----------------------------------------------------------------------

    /// 让每个有机体感知、决策、行动。
    ///
    /// 顺序按编号（= 加入顺序），不按位置。位置顺序会让"谁先动"取决于地图，
    /// 而地图每 tick 都在变，于是同一个种子跑两遍结果不同。编号顺序是稳定的，
    /// 而且它天然表达"老个体先动"。
    fn act(&mut self, world: &mut World) {
        for org in &mut self.organisms {
            org.acted = false;
        }
        let mut eaten = 0u32;
        // 长度取快照：本 tick 新生的个体 `acted == true`，不该在这一轮里行动。
        let n = self.organisms.len();
        for i in 0..n {
            eaten += self.act_one(world, i);
        }
        self.stats.eaten += eaten;
    }

    /// 一个有机体的"感知 → 决策 → 执行"。
    fn act_one(&mut self, world: &mut World, i: usize) -> u32 {
        if self.organisms[i].acted {
            return 0;
        }
        let inputs = self.sense(world, i);
        let out = self.organisms[i].genome.think(&inputs);
        // 存下来给检查器看：界面要能回答"它此刻闻到了什么、想干什么"。
        // 不存的话这些数字在函数返回时就没了，而重算一遍需要 `&World`，
        // 那是界面层拿不到、也不该拿到的东西。
        self.organisms[i].last_inputs = inputs;
        self.organisms[i].last_out = out;
        self.organisms[i].acted = true;

        // 进食与移动的先后：先吃再走。反过来的话，一个"看到食物就扑过去"的
        // 生物会永远差一步——它走到食物格上时，食物已经不在邻格了。
        let mut eaten = 0;
        if out[OUT_EAT] > 0.0 {
            if let Some((x, y)) = self.food_target(world, i) {
                let id = self.organisms[i].id;
                if self.eat_remains(world, id, x, y) {
                    eaten = 1;
                }
            }
        }

        if let Some((dx, dy)) = pick_direction(&out) {
            // 移动细胞多就跑得远（见 `Organism::speed`）。每一格都独立验通行性，
            // 所以不会穿墙——走不动就停在撞上的前一格。
            let speed = self.organisms[i].speed();
            for _ in 0..speed {
                if !self.try_move(world, i, dx, dy) {
                    break;
                }
            }
        }

        if out[OUT_REPRODUCE] > 0.0 {
            let id = self.organisms[i].id;
            // 生不出来就算了，不重试——否则一个挤在角落里的生物会把
            // 全部 tick 预算花在"再试一次找位置"上。
            self.reproduce(world, id);
        }

        eaten
    }

    /// 把周围世界编码成网络输入。
    fn sense(&self, world: &World, i: usize) -> [f32; INPUTS] {
        let org = &self.organisms[i];
        let body = org.cells.len();
        let mut inp = [0.0f32; INPUTS];

        let (fx, fy) = org.front();
        inp[IN_FOOD_AHEAD] = matches!(
            world.get(fx as isize, fy as isize),
            MaterialKind::Remains | MaterialKind::Corpse | MaterialKind::Algae
        ) as u8 as f32;
        inp[IN_BLOCKED_AHEAD] = (!Self::passable(world, body, fx, fy)) as u8 as f32;

        let owner = world.owner(fx as isize, fy as isize);
        inp[IN_KIN_AHEAD] = (owner.is_some() && owner.organism != org.id) as u8 as f32;
        // "前方有别的生物"——对捕食者就是"前方有猎物"。
        //
        // 还没有物种概念，所以"异种"就是"别的个体"。这和 `IN_KIN_AHEAD`
        // 目前同值，但两路的**用途**不同：亲缘那一路将来要喂给"别吃同类"，
        // 异种那一路喂给"扑上去"。现在合并着，等有了物种再分开。
        inp[IN_ALIEN_AHEAD] = inp[IN_KIN_AHEAD];

        let (_, ay) = org.anchor;
        // 探针必须贴着**身体表面**，不能是 `anchor ± 1`。
        //
        // 老写法在三格鱼上就已经是错的：身体本来就占住锚点右侧那一格，
        // 于是 `IN_BLOCKED_RIGHT` 恒为真，手工鱼其实一直在往左游——
        // 只是因为左右两路都接了"撞墙就反向"，撞到左墙时它又往右，
        // 看起来像在正常游动，没人发现。
        //
        // 十几格的身体会把错得更明显：墙可能离锚点四五格远，
        // 探针不贴到身体表面就永远看不见。
        let (lx, ly) = org.probe(-1, 0);
        let (rx, ry) = org.probe(1, 0);
        let (ux, uy) = org.probe(0, -1);
        let (dx_, dy_) = org.probe(0, 1);
        inp[IN_BLOCKED_LEFT] = (!Self::passable(world, body, lx, ly)) as u8 as f32;
        inp[IN_BLOCKED_RIGHT] = (!Self::passable(world, body, rx, ry)) as u8 as f32;
        inp[IN_BLOCKED_UP] = (!Self::passable(world, body, ux, uy)) as u8 as f32;
        inp[IN_SUPPORTED_BELOW] = (!Self::passable(world, body, dx_, dy_)) as u8 as f32;

        inp[IN_ENERGY] = org.energy as f32 / org.max_energy().max(1) as f32;
        inp[IN_AGE] = org.age as f32 / MAX_AGE as f32;
        inp[IN_FACING_X] = org.facing.x as f32;
        inp[IN_FACING_Y] = org.facing.y as f32;

        // 光照：只看深度，不算遮挡。算遮挡要扫 84 格，而 `inject_sunlight`
        // 已经做过一次了——感知这里只给"我在多深"这个粗略信号就够了。
        // 鱼要学会的是"往浅处游"，不是精确估计自己拿到多少能量。
        let height = world.height().max(1) as f32;
        inp[IN_LIGHT] = (1.0 - (ay.max(0) as f32 / height)).clamp(0.0, 1.0);

        // 嗅觉：四向射线。撞到固体就断（墙后面闻不到），找到含能量的食物
        // 就按 1/距离 累加一个方向分量。
        //
        // 用射线而不是"扫一个半径 12 的方框"：方框是 625 格/生物/tick，
        // 射线是 48 格。而且射线天然表达遮挡——一条鱼闻到隔壁岩架后面的藻
        // 是没有道理的，它得绕过去。
        let (mut smell_x, mut smell_y) = (0.0f32, 0.0f32);
        let (mut org_x, mut org_y) = (0.0f32, 0.0f32);
        let smell_radius = org.smell_radius();
        for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            for d in 1..=smell_radius {
                let sx = org.wrap_x(org.anchor.0 + dx * d);
                let sy = org.anchor.1 + dy * d;
                let (sxs, sys) = (sx as isize, sy as isize);
                if !world.in_bounds(sxs, sys) {
                    break;
                }
                // 生物要先看：身体格的材质是 `Empty`，不挡射线，
                // 按材质判的话会"穿过"一条鱼继续往后闻。
                let owner = world.owner(sxs, sys);
                if owner.organism != 0 && owner.organism != org.id {
                    org_x += dx as f32 / d as f32;
                    org_y += dy as f32 / d as f32;
                    break;
                }
                if matches!(
                    world.get(sxs, sys),
                    MaterialKind::Remains | MaterialKind::Corpse | MaterialKind::Algae
                ) && world.flags(sxs, sys) > 0
                {
                    smell_x += dx as f32 / d as f32;
                    smell_y += dy as f32 / d as f32;
                    break;
                }
                // 水、气、空位都能"闻过去"；石头、沙、火挡住。
                if !matches!(
                    world.get(sxs, sys),
                    MaterialKind::Water | MaterialKind::Gas | MaterialKind::Empty
                ) {
                    break;
                }
            }
        }
        inp[IN_FOOD_DIR_X] = smell_x.clamp(-1.0, 1.0);
        inp[IN_FOOD_DIR_Y] = smell_y.clamp(-1.0, 1.0);
        inp[IN_ORGANISM_DIR_X] = org_x.clamp(-1.0, 1.0);
        inp[IN_ORGANISM_DIR_Y] = org_y.clamp(-1.0, 1.0);
        inp
    }

    /// 这个身长的生物能不能进这一格（SPEC §2.2）。
    ///
    /// 石 / 沙 / 残骸一律挡；水和气按体型放行；火在 M2 里也挡
    /// （伤害规则推迟到有装甲细胞的时候一起做，免得先做一套没法验证的伤害）。
    fn passable(world: &World, body: usize, x: i32, y: i32) -> bool {
        if !world.in_bounds(x as isize, y as isize) {
            return false;
        }
        if world.is_occupied(x as isize, y as isize) {
            return false;
        }
        match world.get(x as isize, y as isize) {
            MaterialKind::Empty | MaterialKind::Gas => true,
            MaterialKind::Water => body <= SMALL_BODY_MAX,
            // 藻是悬浮在水里的食物，**不是障碍**：鱼要能游进去吃。
            //
            // 做成障碍的后果不是"鱼被挡住"这么轻：悬在透光层里的几千格藻
            // 会把水流切成碎块，实测整个世界静止（`移动 0`）、
            // 全部鱼被顶进空气里——而"藻"和"石头"在材质表上只差一个枚举值。
            MaterialKind::Algae => true,
            _ => false,
        }
    }

    /// 身体能不能进这一格（与 [`Self::passable`] 同一套判据）。
    ///
    /// 单独一个函数只为了让"身体占住一格时该挤开什么"和"能进哪一格"
    /// 用**同一个判断**——两处写岔了，就会出现"验通行时说能进、
    /// 真进去时那格的东西没被挤走"的幽灵材质。
    #[inline]
    fn body_displaces(kind: MaterialKind) -> bool {
        kind.is_fluid() || kind == MaterialKind::Algae
    }

    /// 找一个能吃的残骸格：任何嘴细胞的四邻。x 方向环绕。
    fn food_target(&self, world: &World, i: usize) -> Option<(isize, isize)> {
        let org = &self.organisms[i];
        for cell in &org.cells {
            if cell.kind != CellType::Mouth {
                continue;
            }
            let (x, y) = org.cell_at(cell);
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let nx = org.wrap_x(x + dx);
                let ny = y + dy;
                if matches!(
                    world.get(nx as isize, ny as isize),
                    MaterialKind::Remains | MaterialKind::Corpse | MaterialKind::Algae
                ) {
                    return Some((nx as isize, ny as isize));
                }
            }
        }
        None
    }

    /// 整体平移一格。
    ///
    /// 分两步：**先把所有目标格验完，再动手**。半步移动会留下一个撕裂的身体，
    /// 而占格表里那些格子从此再也没人认领——那是一种"世界慢慢漏掉细胞"的 bug，
    /// 从现象上完全看不出是移动写错了。
    fn try_move(&mut self, world: &mut World, i: usize, dx: i32, dy: i32) -> bool {
        let org = &self.organisms[i];
        let body = org.cells.len();
        // 成本由 `Organism::move_cost` 统一算：**速度每加一档，每 tick 的
        // 移动开销就往上跳一跳**（1.0 → 1.5 → 2.0 倍质量）。
        //
        // 早先的写法是 `质量 / 速度`，也就是总成本不随速度涨——那样"多长
        // 移动细胞"是纯赚，体型只能朝一个方向进化。现在快慢各有代价。
        let cost = org.move_cost();
        if org.energy < cost {
            return false;
        }

        // 先算出**移动之后的朝向**，再算落点。
        //
        // 顺序反过来是错的：朝向决定镜像（`offset_x`），镜像决定身体占哪几格。
        // 先按旧朝向算落点、再改朝向，claim 出来的格子就不是验过的那批——
        // 轻则身体穿进石头，重则盖掉别的生物。
        //
        // 掉头因此不是"转身"，而是**原地翻面**：锚点不动，身体沿 x 翻过来。
        // 这也是二维精灵的做法，看起来就该是这样。
        let mut facing = org.facing;
        if dx != 0 {
            facing = crate::life::Facing { x: dx as i8, y: 0 };
        } else if dy != 0 {
            facing.y = dy as i8;
        }

        // 水平方向环绕：锚点取模，身体格也跟着取模。
        // 这一步让"右边界向右"变成"从左边界出现"，而不是被墙挡住。
        let new_anchor = (org.wrap_x(org.anchor.0 + dx), org.anchor.1 + dy);
        let old: Vec<(i32, i32)> = org.footprint().collect();

        // 落点按新朝向算。用静态版 `offset_x_for` 而不是克隆一份有机体：
        // 一次移动克隆一整个基因组太浪费。
        let mut targets = Vec::with_capacity(body);
        for cell in &org.cells {
            let x =
                org.wrap_x(new_anchor.0 + crate::life::Organism::offset_x_for(facing, cell.offset));
            targets.push((x, new_anchor.1 + cell.offset.1 as i32));
        }

        for &(x, y) in &targets {
            if !world.in_bounds(x as isize, y as isize) {
                return false;
            }
            if world.is_occupied(x as isize, y as isize) {
                // 目标格被占：只有"那本来就是自己身体的一部分"才允许。
                if !old.iter().any(|&(ox, oy)| ox == x && oy == y) {
                    return false;
                }
            } else if !Self::passable(world, body, x, y) {
                return false;
            }
        }

        // 身体不能整个离开水（SPEC §2.0：生物默认水生）。
        //
        // 没有这一条，鱼会一路游到天上：光照按 `1 - y/height` 算，越往上越亮，
        // 而 `passable` 对空气是放行的——于是"往上"永远有奖励、从来没有代价。
        // 实测加大体型之后，5000 tick 就有个体停在水面上方 25 格，
        // 能量晒满、既不能动也不能繁殖，整个种群随之冻死。
        //
        // 判据用"贴着水"而不是"目标格里有水"，理由见 [`Self::touches_water`]。
        if !Self::touches_water(world, org.width, &targets) {
            return false;
        }

        // 执行：放手 → 顶开流体 → 占位。
        // 中间这一小段"身体不在世界上"是安全的：生命层是单线程的，
        // 而材质层要等整个生命层跑完才轮到，没有人会看到这个中间态。
        org.release(world);

        // 身体腾出来的格子——只有这些位置才接得住被挤开的流体。
        //
        // 这里有一个很容易写错的陷阱：不能拿 `old[k]` 当第 k 个细胞的落点。
        // 身体内部会重叠——右移一格时，0 号细胞的新位置正是 1 号细胞的旧位置。
        // 照着下标配对的写法会把水换进**新身体内部**（那格随即被 claim 成细胞），
        // 于是鱼肚子里藏了一格水；等它死掉、那格被写成残骸，水就凭空没了。
        // 实测 512² 跑 8000 tick 少了 371 格水，根因就在这里。
        let vacated: Vec<(i32, i32)> = old
            .iter()
            .copied()
            .filter(|p| !targets.contains(p))
            .collect();
        let mut next_vacated = 0usize;
        for &(x, y) in &targets {
            if old.contains(&(x, y)) {
                // 身体内部的格子：它本来就是 Empty，没什么可挤的。
                continue;
            }
            // 水和藻都会被身体挤开（藻是悬浮物，不是障碍）。
            // 只认 `is_fluid` 的话，藻会留在被 claim 的格子里——
            // 材质和细胞同时占着一格，违反"非材质即细胞"这条地基。
            if !Self::body_displaces(world.get(x as isize, y as isize)) {
                continue;
            }
            if next_vacated >= vacated.len() {
                break;
            }
            let (ox, oy) = vacated[next_vacated];
            next_vacated += 1;
            // 小生物顶开水：水被挤到身体刚让出来的那一格。
            // 用 `swap` 而不是"复制 + 清空"，水才不会凭空消失。
            world.swap(ox as isize, oy as isize, x as isize, y as isize);
        }

        {
            let org = &mut self.organisms[i];
            org.anchor = new_anchor;
            org.energy -= cost;
            // 朝向必须在 `claim` **之前**写进去：`claim` 靠 `offset_x` 算落点，
            // 而镜像由朝向决定。写晚了，claim 出来的还是旧朝向那批格子，
            // 于是验过的落点没人占、没验过的格子被占——身体直接穿模。
            org.facing = facing;
            org.claim(world);
        }
        // 移动消耗的能量变成热离开生物可用池，必须记账。
        self.ledger.dissipated += cost as u64;
        true
    }

    /// 繁殖：把一个有机体拆成亲代 + 子代。
    ///
    /// M2 只在**附近**生：子代的身体形状与亲代相同，能量拿走
    /// [`crate::life::Organism::birth_energy`]。
    /// 真正的"找个空位生下来"要等身体能压缩/扩展之后再说——
    /// 现在强行找位置只会让出生失败率取决于地图，掩盖掉代谢本身的问题。
    ///
    /// 返回子代编号；能量不够或没地方放则返回 `None`。
    pub fn reproduce(&mut self, world: &mut World, id: OrganismId) -> Option<OrganismId> {
        let idx = self.organisms.iter().position(|o| o.id == id)?;
        let parent = &self.organisms[idx];
        // 门槛按身体大小算：大身体造子代更贵，必须攒得更久。
        // 固定 400 会让"变大"变成纯赚——见 `SMALL_BODY_MAX` 上面那段注释。
        let birth_energy = parent.birth_energy();
        if parent.energy < parent.reproduction_energy() {
            return None;
        }

        // 拥挤：附近同类太多就不生。
        //
        // 这是第二道负反馈（第一道是光照遮挡）。光遮挡让"密处更穷"，
        // 但一条鱼仍可以在别人都饿着的时候自己攒够能量——所以还需要
        // 一条"地盘"规则，否则种群会在能量总量受限的同时把个体数堆到很高。
        let (ax, ay) = parent.anchor;
        let width = parent.width;
        // 水平距离也要按环面算：贴着左右边界的两条鱼其实是邻居。
        let ring_dx = |a: i32, b: i32| {
            let d = (a - b).abs();
            if width > 0 {
                d.min(width - d)
            } else {
                d
            }
        };
        let nearby = self
            .organisms
            .iter()
            .filter(|o| o.id != id)
            .filter(|o| {
                ring_dx(o.anchor.0, ax) <= CROWD_RADIUS && (o.anchor.1 - ay).abs() <= CROWD_RADIUS
            })
            .count();
        if nearby >= CROWD_LIMIT {
            return None;
        }

        // 先把亲代身上要用的东西拷出来，再动账——否则 `&mut self` 的借用
        // 会一路顶到循环里，`self.fits` 就没法调了。
        let anchor = parent.anchor;
        let cells = parent.cells.clone();
        let facing = parent.facing;
        let generation = parent.generation + 1;
        let mut genome = parent.genome.clone();
        genome.mutate(&mut self.rng, MUTATION_RATE, MUTATION_SIGMA);

        let child = Organism {
            id: 0, // spawn 会分配
            anchor,
            width,
            cells,
            energy: birth_energy,
            age: 0,
            facing,
            genome,
            parent: Some(id),
            generation,
            acted: true, // 出生这一 tick 不再行动
            // 子代的"上一 tick 感知"是空的：它还没感知过。
            // 检查器在这种情况下显示"刚出生"，而不是一堆看起来像数据的 0。
            last_inputs: [0.0; INPUTS],
            last_out: [0.0; OUTPUTS],
            energy_hist: [0; crate::life::ENERGY_HIST],
            hist_head: 0,
        };

        // 子代与亲代不能占同一批格子。候选位置在切比雪夫距离 6 以内，
        // **随机挑一个**而不是"从最近的开始试"。
        //
        // 这一条是被实测逼出来的：按距离排序时，子代永远生在亲代右上角，
        // 于是十几代之后整个种群挤成一团（ASCII 视图里就是一块 6×3 的 @）。
        // 挤成一团之后拥挤规则生效，种群卡在 10 条不再增长——
        // 表面看是"生态平衡"，实际是"所有鱼叠在一起"。
        let mut offsets: Vec<(i32, i32)> = Vec::with_capacity(168);
        for dy in -6i32..=6 {
            for dx in -6i32..=6 {
                if dx != 0 || dy != 0 {
                    offsets.push((dx, dy));
                }
            }
        }
        for i in (1..offsets.len()).rev() {
            let j = self.rng.gen_range_usize(0, i + 1);
            offsets.swap(i, j);
        }

        for (dx, dy) in offsets {
            let mut candidate = child.clone();
            candidate.anchor = (child.wrap_x(anchor.0 + dx), anchor.1 + dy);
            if !self.fits(world, &candidate) {
                continue;
            }
            if let Ok(cid) = self.spawn(world, candidate) {
                // 能量从亲代转给子代，一进一出，总量不变，所以**不记注入**。
                self.organisms[idx].energy -= birth_energy;
                self.stats.born += 1;
                return Some(cid);
            }
        }
        None
    }

    /// 一组格子是不是**贴着**水或气（四邻）。
    ///
    /// 这是"生物默认水生"（SPEC §2.0）唯一需要的那条不变量。
    ///
    /// 为什么不是"这组格子里有一格是水"：身体格的材质**永远是 `Empty`**
    /// （材质层与生命层的互斥约定），所以"身体自己身上有水"这件事根本无从谈起。
    /// 而如果改成"目标位置里有一格水"，就会漏掉一整类崩坏——实测过的：
    ///
    /// 一条鱼贴着水面生孩子，出生时那一格水会被**借走**再被顶到身体上方，
    /// 而那格悬空的水成了下一次出生的"锚"，子代再往上生一格……
    /// 实测 5000 tick 之后 4 条叶背鱼停在 y≈128，而水面线在 y=153——
    /// 它们飘在水面上方 25 格，能量晒满、动不了也生不了，世界就此冻住。
    /// （当时水会被"挤"到身体上方；现在改成借水模型，见 `spawn`，
    /// 但这条不变量仍然要留着——否则子代照样能生在空气里。）
    ///
    /// 换成"贴着水"之后，往上爬一格就会失去与水的接触，梯子从根上断掉。
    fn touches_water(world: &World, width: i32, cells: &[(i32, i32)]) -> bool {
        cells.iter().any(|&(x, y)| {
            [(0, 0), (1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|&(dx, dy)| {
                    let nx = if width > 0 {
                        (x + dx).rem_euclid(width)
                    } else {
                        x + dx
                    };
                    matches!(
                        world.get(nx as isize, (y + dy) as isize),
                        MaterialKind::Water | MaterialKind::Gas | MaterialKind::Algae
                    )
                })
        })
    }

    /// 这个有机体的身体能不能完整落在这个位置。
    ///
    /// 比 [`Self::can_place`] 多一条：**水格上方必须有空气**。
    /// 繁殖出来的子代必须遵守它，因为子代出生时那一格的水要被推走
    /// （见 [`displace_fluid`]），而水下没有空位。
    ///
    /// 玩家手动投放不受这条限制——想往海沟里扔鱼是玩家的自由，
    /// 那条鱼会因为没有光而饿死，那本身就是要展示的东西。
    fn fits(&self, world: &World, org: &Organism) -> bool {
        let mut cells = Vec::with_capacity(org.cells.len());
        for c in &org.cells {
            let (x, y) = org.cell_at(c);
            if !Self::can_place(world, x, y) || !Self::water_has_air_above(world, x, y) {
                return false;
            }
            cells.push((x, y));
        }
        // 生下来的身体必须贴着水——见 [`Self::touches_water`] 的注释，
        // 这一条就是拦住"靠繁殖一代代爬进天空"的那道门。
        Self::touches_water(world, org.width, &cells)
    }

    /// 如果这一格是水，它的上方整列里得有空位——否则这格水无处可去。
    ///
    /// 范围必须和 [`displace_fluid`] 的兜底搜索一致，否则会出现
    /// "验证说上方有空气、真要搬的时候那格已经被占了"的情形，
    /// 然后水就被清掉。实测 512² 跑 8000 tick 少了 405 格水，根因就是这两处范围不一致。
    fn water_has_air_above(world: &World, x: i32, y: i32) -> bool {
        let (xs, ys) = (x as isize, y as isize);
        if world.get(xs, ys) != MaterialKind::Water {
            return true;
        }
        (0..ys)
            .rev()
            .any(|ny| !world.is_occupied(xs, ny) && world.get(xs, ny) == MaterialKind::Empty)
    }

    /// 单格能不能放细胞：在界内、无主、不是固体。
    ///
    /// 藻放行：它是悬浮物，不是地形。挡住的话，海面上那层藻会让
    /// "在这一列投一条鱼"直接失败，玩家看到的是"水面有一段没鱼"。
    fn can_place(world: &World, x: i32, y: i32) -> bool {
        let (xs, ys) = (x as isize, y as isize);
        if !world.in_bounds(xs, ys) || world.is_occupied(xs, ys) {
            return false;
        }
        matches!(
            world.get(xs, ys),
            MaterialKind::Empty | MaterialKind::Gas | MaterialKind::Water | MaterialKind::Algae
        )
    }

    /// 吃掉某一格残骸，能量转给指定有机体。返回是否真的吃到了。
    ///
    /// 吃不下的时候**只吃一部分**，剩下的留在残骸里。
    /// 早先的写法是 `energy = (energy + gain).min(MAX_ENERGY)`，
    /// 超过上限的那部分凭空蒸发——账面上看不出来，直到某条鱼在满能量时
    /// 咬了一口尸体，守恒断言就差了 20 点。
    pub fn eat_remains(&mut self, world: &mut World, id: OrganismId, x: isize, y: isize) -> bool {
        let kind = world.get(x, y);
        if !matches!(
            kind,
            MaterialKind::Remains | MaterialKind::Corpse | MaterialKind::Algae
        ) {
            return false;
        }
        let Some(idx) = self.organisms.iter().position(|o| o.id == id) else {
            return false;
        };
        let gain = world.flags(x, y) as u32;
        let room = self.organisms[idx]
            .max_energy()
            .saturating_sub(self.organisms[idx].energy);
        let taken = gain.min(room);
        if taken == 0 {
            return false;
        }
        if taken == gain {
            // 吃掉之后变回什么，取决于这格东西**原本是不是水**。
            //
            // - 尸体：那一格原本是身体格，身体出生时借走了这一格的水
            //   （见 `spawn`）。尸体被吃掉等于身体彻底消失，水就该还回来。
            // - 藻：藻是水里的悬浮物。吃掉藻、留下水，正是"把藻从水里滤掉"。
            //   写成空位的话，每吃掉一格藻就少一格水——而"鱼吃藻"是常态，
            //   于是体积会随生态活跃度单调减少（实测就是这么漂了几百格）。
            // - 天然碎屑：它不是从身体借来的，也不是水，吃掉之后只留下空位。
            //   早先三者都还水，于是水位随"吃了多少碎屑"漂移了二十格。
            world.set(
                x,
                y,
                match kind {
                    MaterialKind::Corpse | MaterialKind::Algae => MaterialKind::Water,
                    _ => MaterialKind::Empty,
                },
            );
        } else {
            // 留一部分在尸体上，下一顿再来。
            world.set_flags(x, y, (gain - taken) as u8);
        }
        let org = &mut self.organisms[idx];
        org.energy += taken;
        self.stats.eaten += 1;
        true
    }

    /// 手工加一个细胞（调试与手工生物用）。
    pub fn push_cell(org: &mut Organism, kind: CellType, offset: (i16, i16)) {
        org.cells.push(Cell::new(kind, offset));
    }

    /// 按手工体型造一个有机体。
    ///
    /// `energy` 是初始能量，调用方给。注意它应该跟着体型走——
    /// 十几格的鱼拿三格鱼的那点启动能量，会在攒够繁殖门槛之前先饿死
    /// （见 [`Self::starter_energy`]）。
    pub fn from_plan(plan: &BodyPlan, anchor: (i32, i32), genome: Genome, energy: u32) -> Organism {
        let mut org = Organism::new(0, anchor, plan.cells(), genome);
        org.energy = energy;
        org
    }

    /// 让基因组**自己长**出身体来（M3 · 3b）。
    ///
    /// 这是 `from_plan` 的对应物：那边身体是给定的，这边身体是发育出来的。
    /// 差别看起来只在"多调一次 `develop()`"，但意义完全不同——
    /// 子代继承的是**规则**，不是形状，所以突变能改到身体。
    pub fn from_genome(genome: Genome, anchor: (i32, i32), energy: u32) -> Organism {
        let cells = genome.grn.develop();
        let mut org = Organism::new(0, anchor, cells, genome);
        org.energy = energy;
        org
    }

    /// 给一个体型配一份"够活到能繁殖"的启动能量。
    ///
    /// 不能所有体型都给 300：三格鱼 300 点是 75% 的繁殖门槛，
    /// 而叶背鱼 300 点只有 16%——它会在攒到门槛之前先把维持成本烧光。
    /// 取繁殖门槛的 60%：留四成要自己挣，否则"投放"就等于"白送一次繁殖"。
    pub fn starter_energy(plan: &BodyPlan) -> u32 {
        let cells = plan.len().max(1) as u32;
        crate::life::BIRTH_ENERGY_PER_CELL * cells * 6 / 5
    }

    /// 造一条最小的"鱼"：一个移动细胞 + 一个嘴 + 一个生产者。
    ///
    /// 形状是竖着的三格，因为它要在水里上下移动；横着三格会更容易卡在
    /// 岛坡上。这不是最优形状，是**第一个能证明整条链路通的**形状。
    pub fn simple_fish(anchor: (i32, i32), genome: Genome, energy: u32) -> Organism {
        let cells = vec![
            Cell::new(CellType::Mover, (0, 0)),
            Cell::new(CellType::Mouth, (1, 0)),
            Cell::new(CellType::Producer, (0, -1)),
        ];
        let mut org = Organism::new(0, anchor, cells, genome);
        org.energy = energy;
        org
    }
}

/// 两个基因组的平均权重距离（每个权重的平均绝对差）。
fn genome_distance(a: &Genome, b: &Genome) -> f32 {
    let n = a.weights.len().min(b.weights.len());
    if n == 0 {
        return 0.0;
    }
    let sum: f32 = a
        .weights
        .iter()
        .zip(&b.weights)
        .map(|(x, y)| (x - y).abs())
        .sum();
    sum / n as f32
}

/// 从网络输出里挑一个移动方向。
///
/// 取**激活最强**的那一路，而不是"四个都大于阈值就都试一遍"：
/// 同时往右和往上走没有定义，而"最强的那一路胜出"至少是稳定的。
/// 全部低于 [`MOVE_THRESHOLD`] 就不动——这一步很重要，见那个常量的注释。
fn pick_direction(out: &[f32; OUTPUTS]) -> Option<(i32, i32)> {
    let candidates = [
        (OUT_LEFT, -1, 0),
        (OUT_RIGHT, 1, 0),
        (OUT_UP, 0, -1),
        (OUT_DOWN, 0, 1),
    ];
    let mut best: Option<(f32, i32, i32)> = None;
    for (idx, dx, dy) in candidates {
        let v = out[idx];
        if v <= MOVE_THRESHOLD {
            continue;
        }
        if best.map_or(true, |(bv, _, _)| v > bv) {
            best = Some((v, dx, dy));
        }
    }
    best.map(|(_, dx, dy)| (dx, dy))
}

/// 手工基因组的"社交倾向"：**有没有牙决定往哪边走**。
///
/// 这不是替进化写策略，而是给两套完全不同的身体一个**能活的起点**：
/// 有杀手细胞的身体如果一见到别人就躲，它永远吃不上一口；
/// 没牙的身体如果一见到别人就凑上去，它是自己在送死。
/// 之后怎么调，交给突变。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tactic {
    /// 有牙：靠近别的生物。
    Hunt,
    /// 没牙：远离别的生物。
    ///
    /// **手工起点不用它，留给进化。** 实测：猎物一躲，捕食者就在三种尺度下
    /// 全部灭绝（512² 起步 24 条 / 120 条、128² 起步 20 条，16000 tick 时
    /// `杀手 0 枚`）——因为捕食者和猎物**速度一样**，追的一方永远追不上，
    /// 只能吃被逼到角落的，而那种机会少到养不活自己。
    ///
    /// 这不是"逃"写错了，是"逃"太有效了：它把捕食这个生态位变得需要**技巧**，
    /// 而手工基因组没有技巧。所以手工起点只给"追"，"逃"作为一条通道
    /// 留给突变——真到了那一天，捕食者也该跟着变快或变聪明。
    Flee,
    /// 不理会别的生物。手工起点给非捕食者用。
    Ignore,
}

/// 一条手工设计的"食腐鱼"的大脑。
///
/// 四件事：往前游、撞墙就掉头、看到什么就吃、攒够能量就生。
/// 加上一条社交倾向（[`Tactic`]）：有牙的靠近、没牙的躲开。
///
/// "撞墙掉头"不是装饰：只给"往右"的偏置时，所有鱼都会游到右边界堆成一团，
/// 于是互相拥挤、停止繁殖，整个种群卡在 9 条（实测）。一条能被墙推回来的鱼
/// 才会把水域用满——这是 M2 里第一次看到"行为影响生态"的地方。
pub fn handcrafted_fish_genome(tactic: Tactic) -> Genome {
    let mut g = Genome::blank();
    g.weights[Genome::b_o(OUT_RIGHT)] = 1.0;
    g.weights[Genome::b_o(OUT_EAT)] = 2.0;
    g.weights[Genome::b_o(OUT_REPRODUCE)] = 1.5;
    // 撞到哪一侧的墙，就往另一侧走。**不能**写成"前方被挡就往左"——
    // 那样鱼会在墙边左右抖：被挡住→往左走一格→前方空出来→又想往右。
    // 分左右两路之后，它才会真的走完整个水池再折返。
    g.connect(IN_BLOCKED_RIGHT, OUT_LEFT, 10.0);
    g.connect(IN_BLOCKED_LEFT, OUT_RIGHT, 10.0);

    // --- 觅食：闻着藻席的味道走 -------------------------------------------
    //
    // 这一层是 M3 的"食物分布不均匀"落地的**唯一行为改动**：
    // 藻席长在海底，而光只从上往下射，所以"哪儿有吃的"和"哪儿晒得到"
    // 是两个不同的地方。鱼必须自己决定什么时候下潜、什么时候上来。
    //
    // 权重只给 1.5，和"往前游"的偏置（1.0）同一量级——**刻意不给大**。
    //
    // 第一版给 8.0（远距离也能压过巡航偏置），实测是**负收益**：
    // 鱼闻着味道去追，花掉的移动成本比吃到的高，种群从 52 掉到 37、
    // 细胞数从 688 掉到 442。
    //
    // 想清楚之后发现那根本不是权重的问题：**手工基因组不该被硬塞一个觅食策略。**
    // 它的职责是"会游、会吃、会生"这条基线，不是"最优觅食"。正确的分层是
    // 把"闻到食物"这个**能力**接进去，让突变去决定要不要用、用多强——
    // 那才是选择该做的事，而不是我在常数里替它决定。
    g.connect(IN_FOOD_DIR_X, OUT_RIGHT, 1.5);
    g.connect(IN_FOOD_DIR_X, OUT_LEFT, -1.5);
    g.connect(IN_FOOD_DIR_Y, OUT_DOWN, 1.5);
    g.connect(IN_FOOD_DIR_Y, OUT_UP, -1.5);

    // --- 捕食：前方有别的生物就往那边靠 --------------------------------
    //
    // 同样是"只给通道，不给策略"：权重和觅食一样是 1.5。
    // 对掠食者它意味着"扑上去"，对其他体型它意味着"旁边有东西"——
    // 具体是好事还是坏事，取决于它自己有没有牙。
    g.connect(IN_ALIEN_AHEAD, OUT_RIGHT, 1.5);
    g.connect(IN_ALIEN_AHEAD, OUT_LEFT, -1.5);

    // --- 社交：有牙的靠过去，没牙的躲开 -----------------------------------
    //
    // 这是"追 / 逃"的全部实现，也是捕食第一次不只是"碰巧撞上"。
    // 权重 3.0 比觅食（1.5）强：别人比食物更紧急。
    // 但仍然压不过"撞墙掉头"（10.0）——贴到墙上时先脱身再说。
    // 权重不对称：**追得狠，逃得迟**。
    //
    // 对称（两边都 3.0）时捕食者会全灭——猎物和捕食者速度一样，
    // 一旦猎物在 20 格就开始躲，追的一方永远追不上，只能吃被逼到角落的。
    // 实测 512² 起步 120 条、16000 tick：`杀手 0 枚`。
    // 猎物只在该反应的时候反应（约 6 格内）才留得出捕食的余地。
    let (toward, away) = match tactic {
        Tactic::Hunt => (3.0, -3.0),
        Tactic::Flee => (-3.0, 3.0),
        Tactic::Ignore => (0.0, 0.0),
    };
    g.connect(IN_ORGANISM_DIR_X, OUT_RIGHT, toward);
    g.connect(IN_ORGANISM_DIR_X, OUT_LEFT, away);
    g.connect(IN_ORGANISM_DIR_Y, OUT_DOWN, toward);
    g.connect(IN_ORGANISM_DIR_Y, OUT_UP, away);

    // --- 续命：暗了就往上 ------------------------------------------------
    //
    // 光随深度线性衰减，海底几乎是 0。只往下追食物会饿死在下面，
    // 所以要有一条"这儿太暗了，上去"的回路。
    //
    // 写法是**负向抑制**而不是正向驱动：默认有一点想往上（偏置 1.2），
    // 光越强越不往上（−3）。正面驱动会让鱼贴着水面抖——
    // 上去一格变亮，又想上去，最后卡在水面那一行。
    //
    // 拐点落在光 ≈ 0.4：比这暗就开始往上。第一版写 −6（拐点在 0.08）
    // 等于"只有快贴到海底才想起来上浮"，于是下潜觅食的鱼有去无回——
    // 实测种群从 52 掉到 37，细胞数从 688 掉到 442。
    g.weights[Genome::b_o(OUT_UP)] = 1.2;
    g.connect(IN_LIGHT, OUT_UP, -3.0);
    g
}

/// 在水面附近投放若干条鱼。
///
/// "水面附近"= 让体型**最上面那一格**贴着水面。位置不是随便挑的：
/// 生产者靠光吃饭，而光随深度线性衰减，所以鱼必须待在水面附近才活得下去
/// （见 [`PRODUCER_YIELD`] 的推导）。把它们扔到海床上，几秒就全饿死。
///
/// 投放的体型按 [`crate::body::PLANS`] 轮换，所以同一个种子里
/// "第几条鱼长什么样"是确定的——这是可复现性的一部分，不是装饰。
///
/// `energy` 是启动能量的**下限**：体型越大，被抬到
/// [`LifeSim::starter_energy`] 越高。统一给 300 的话，十几格的鱼会在
/// 攒够繁殖门槛之前先把维持成本烧光。
///
/// 返回实际投放成功的编号。放不下的（水太浅、格子被占、身体超出边界）
/// 会被跳过，而不是让整批失败——玩家按一下"放鱼"不该因为某一列地形不行
/// 就全都不出现。
pub fn stock_surface(
    world: &mut World,
    sim: &mut LifeSim,
    count: usize,
    energy: u32,
    morph: bool,
) -> Vec<OrganismId> {
    let mut ids = Vec::new();
    if count == 0 || world.width() == 0 || PLANS.is_empty() {
        return ids;
    }
    let width = world.width();
    let step = (width / count).max(1);
    // 倾向按**长出来的身体**决定，不按体型表：形态发生模式下
    // "这是不是捕食者"只有长完才知道。
    let tactic_of = |cells: &[Cell]| {
        if cells.iter().any(|c| c.kind == CellType::Killer) {
            Tactic::Hunt
        } else {
            Tactic::Ignore
        }
    };

    for k in 0..count {
        let plan = &PLANS[k % PLANS.len()];

        // **先算出身体，再配基因组。**
        //
        // 顺序不能反过来：社交倾向（追 / 逃）要看"这条鱼有没有牙"，
        // 而形态发生模式下"有没有牙"只有长完才知道。
        //
        // 形态发生模式：基因组里塞一份**从手工体型反推出来的生长程序**，
        // 身体由它长出来。这一步只是热启动——之后子代继承的是规则，
        // 突变改到的就是形状本身了。
        let grn = morph.then(|| {
            Grn::from_plan(plan)
                .or_else(|| Grn::from_plan(&PLANS[0]))
                .unwrap_or_default()
        });
        let cells = match &grn {
            Some(g) => g.develop(),
            None => plan.cells(),
        };

        // 每个个体都轻微不同：给自然选择一点原材料。
        //
        // 全都一模一样的话，第一代没有任何可选的差异，种群只能靠"繁殖时突变"
        // 慢慢产生变异——那要等很多代。播种时给一点抖动，选择从第一 tick 就开始。
        // 幅度刻意压小（sigma 0.12）：大一点就会把手工设计的"会游、会吃"打散，
        // 鱼连基本行为都没有，选择也就无从谈起。
        let mut genome = handcrafted_fish_genome(tactic_of(&cells));
        genome.mutate(
            &mut Rng::new(0xF15E_0000 ^ (k as u64).wrapping_mul(0x9E37_79B9)),
            0.35,
            0.12,
        );
        if let Some(g) = grn {
            genome.grn = g;
        }

        // 身体在 x 方向的左右伸出量，用来把锚点夹在界内——
        // 否则贴边的个体整个投放会失败，水面看起来"有一段没鱼"。
        let reach_left = cells.iter().map(|c| -c.offset.0).max().unwrap_or(0) as i32;
        let reach_right = cells.iter().map(|c| c.offset.0).max().unwrap_or(0) as i32;
        let x = ((k * step + step / 2) as i32).clamp(reach_left, width as i32 - 1 - reach_right);

        let mut surface = None;
        for y in 0..world.height() as i32 {
            if world.get(x as isize, y as isize) == MaterialKind::Water {
                surface = Some(y);
                break;
            }
        }
        let Some(y) = surface else { continue };

        // 让最上面那一格贴着水面：生产者要光。
        let top = cells.iter().map(|c| c.offset.1).min().unwrap_or(0) as i32;
        let anchor = (x, y - top);
        // 启动能量按**实际长出来的格数**算，不是按体型表的格数——
        // 形态发生模式下这两者可能不一样，而"够不够活到能繁殖"只看前者。
        let n = cells.len().max(1) as u32;
        let energy = energy.max(crate::life::BIRTH_ENERGY_PER_CELL * n * 6 / 5);
        let mut org = Organism::new(0, anchor, cells, genome);
        org.energy = energy;
        if let Ok(id) = sim.spawn_injected(world, org) {
            ids.push(id);
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> World {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 60, 64, 4, MaterialKind::Stone);
        w.fill_rect(0, 30, 64, 30, MaterialKind::Water);
        w
    }

    /// 「水 + 活细胞 + 残骸」是一个守恒量。
    ///
    /// 身体出生时**借走**它占住的那一格水（见 [`LifeSim::spawn`]），
    /// 变成尸体时水还欠着，尸体腐烂或被吃掉时还回来。
    /// 所以水位不会因为出生次数而漂移——这是"海平面不会慢慢下降"的新说法。
    ///
    /// 旧说法是"出生只能把水挤开、不能把水变没"，而那正是
    /// "水被顶到身体上方 → 子代站在那格水上 → 一代代爬进天空"的源头。
    fn water_volume(world: &World) -> usize {
        world.count_of(MaterialKind::Water)
            + world.occupied_count()
            + world.count_of(MaterialKind::Remains)
            + world.count_of(MaterialKind::Corpse)
            + world.count_of(MaterialKind::Algae)
    }

    #[test]
    fn spawn_claims_every_cell() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let org = LifeSim::simple_fish((20, 25), Genome::blank(), 100);
        let id = sim.spawn_injected(&mut w, org).unwrap();
        assert_eq!(w.occupied_count(), 3);
        assert_eq!(w.owner(20, 25).organism, id);
        assert_eq!(w.owner(21, 25).organism, id);
        assert_eq!(w.owner(20, 24).organism, id);
    }

    #[test]
    fn spawn_rejects_occupied_and_out_of_bounds() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 25), Genome::blank(), 10))
            .unwrap();
        let again = sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 25), Genome::blank(), 10));
        assert_eq!(again.unwrap_err(), SpawnError::CellOccupied);

        let edge = sim.spawn_injected(&mut w, LifeSim::simple_fish((0, 0), Genome::blank(), 10));
        assert_eq!(edge.unwrap_err(), SpawnError::OutOfBounds);
    }

    #[test]
    fn starvation_turns_the_body_into_remains() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 生产者放进深水（光很弱）+ 没有嘴 = 必然饿死。
        let org = LifeSim::simple_fish((20, 50), Genome::blank(), 6);
        let id = sim.spawn_injected(&mut w, org).unwrap();
        let spots: Vec<_> = sim.organism(id).unwrap().footprint().collect();

        for _ in 0..200 {
            sim.step(&mut w);
            if sim.organisms().is_empty() {
                break;
            }
        }

        assert!(sim.organisms().is_empty(), "必须饿死");
        assert_eq!(w.occupied_count(), 0, "尸体不能继续占格");
        for (x, y) in spots {
            assert_eq!(
                w.get(x as isize, y as isize),
                MaterialKind::Corpse,
                "({x}, {y}) 应该是尸体"
            );
        }
    }

    #[test]
    fn energy_is_conserved_through_life_and_death() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 这条测的是能量账，不是藻席。海底长出来的藻会把"残骸格数"搅乱，
        // 所以关掉它——不是因为它不可信，是因为它属于另一个测试。
        sim.set_food_growth(false);
        sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 45), Genome::blank(), 120))
            .unwrap();

        for _ in 0..500 {
            sim.step(&mut w);
            let audit = sim.audit(&w);
            assert!(
                audit.is_balanced(),
                "第 {} tick 能量账不平：{:?}，差额 {}",
                w.tick(),
                audit,
                audit.leak()
            );
            if sim.organisms().is_empty() {
                break;
            }
        }
        assert!(sim.organisms().is_empty(), "这条鱼应该已经饿死");
        assert_eq!(
            w.count_of(MaterialKind::Corpse),
            3,
            "尸体必须以尸体材质回到环境"
        );
        // 饿死的尸体是被自己烧光的，能量可能已经接近 0——
        // 这里只要求账平，不要求残骸里还有能量（见 `bury` 的注释）。
        assert!(sim.audit(&w).is_balanced());
    }

    /// 老死的尸体**带着能量**。这是"吃尸体"能成立的前提：
    /// 如果所有尸体都是空的，残骸就只是地形，不是食物。
    #[test]
    fn an_old_organism_leaves_energy_in_its_remains() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // 必须放在**水里**（pool 的水面在 y=30）：身体格泡在空气里时
        // 出生那一刻没借过水，死后也就不留尸体（见 `Cell::wet`）。
        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 45), Genome::blank(), 300))
            .unwrap();
        sim.organisms_mut()[0].age = MAX_AGE - 1;

        sim.step(&mut w);

        assert!(sim.organism(id).is_none(), "到寿数必须死");
        let audit = sim.audit(&w);
        assert!(audit.remains > 0, "尸体应该带着能量：{audit:?}");
        assert!(audit.is_balanced(), "差额 {}", audit.leak());
        assert_eq!(w.count_of(MaterialKind::Corpse), 3);
    }

    #[test]
    fn natural_remains_are_counted_as_initial_energy() {
        let mut w = pool();
        w.set(10, 20, MaterialKind::Remains);
        w.set(11, 20, MaterialKind::Remains);
        let sim = LifeSim::new(&mut w);
        assert_eq!(sim.ledger().injected, 2 * REMAINS_ENERGY as u64);
        assert_eq!(w.flags(10, 20), REMAINS_ENERGY as u8);
        assert!(sim.audit(&w).is_balanced());
    }

    #[test]
    fn eating_moves_energy_from_remains_into_the_body() {
        let mut w = pool();
        // 残骸放在嘴够得着、但身体压不到的地方。
        w.set(22, 25, MaterialKind::Remains);
        w.set_flags(22, 25, REMAINS_ENERGY as u8);
        let mut sim = LifeSim::new(&mut w);
        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 25), Genome::blank(), 50))
            .unwrap();

        let before = sim.audit(&w);
        assert!(sim.eat_remains(&mut w, id, 22, 25));
        // 这里吃的是**天然碎屑**（`Remains`），不是尸体：它不是从身体借来的，
        // 吃掉之后只留下空位，不还水。尸体才还水。
        assert_eq!(w.get(22, 25), MaterialKind::Empty);
        assert_eq!(sim.organism(id).unwrap().energy, 50 + REMAINS_ENERGY);
        let after = sim.audit(&w);
        assert_eq!(after.accounted(), before.accounted(), "进食只是搬运");
    }

    #[test]
    fn sunlight_feeds_producers_only_in_shallow_water() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let shallow = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((10, 2), Genome::blank(), 100))
            .unwrap();
        let deep = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((40, 58), Genome::blank(), 100))
            .unwrap();

        sim.step(&mut w);

        let s = sim.organism(shallow).unwrap().energy;
        let d = sim.organism(deep).unwrap().energy;
        assert!(s > d, "浅水的生产者应该拿到更多光：{s} vs {d}");
        assert!(sim.audit(&w).is_balanced());
    }

    #[test]
    fn reproduction_needs_energy_and_a_free_spot() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let poor = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 25), Genome::blank(), 100))
            .unwrap();
        assert!(sim.reproduce(&mut w, poor).is_none());

        // 测试直接给这条鱼加能量。加多少就得补记多少注入，
        // 否则账面上会凭空多出 300 点——这正是"所有能量进出都要走记账点"的意思。
        let need = sim.organisms()[0].reproduction_energy();
        let deficit = need - sim.organisms()[0].energy;
        sim.organisms_mut()[0].energy = need;
        sim.ledger.injected += deficit as u64;

        let child = sim.reproduce(&mut w, poor);
        assert!(child.is_some(), "能量够了就该生得出来");
        assert_eq!(sim.organisms().len(), 2);
        assert!(sim.audit(&w).is_balanced(), "繁殖不能造能量");
    }

    // -----------------------------------------------------------------------
    // 行为
    // -----------------------------------------------------------------------

    /// 偏置让"往右"与"想吃"恒为 0.96，所以这条鱼不看任何东西也会一直往右游、
    /// 遇到尸体就吃。这不是"聪明"，是**可控**——测试需要一个不会随机改主意的对象。
    fn forward_genome() -> Genome {
        let mut g = Genome::blank();
        g.weights[Genome::b_o(OUT_RIGHT)] = 2.0;
        g.weights[Genome::b_o(OUT_EAT)] = 2.0;
        g
    }

    /// 两格身体：移动 + 嘴。没有生产者，所以能量只减不增。
    fn swimmer(anchor: (i32, i32), energy: u32) -> Organism {
        let mut org = Organism::new(
            0,
            anchor,
            vec![
                Cell::new(CellType::Mover, (0, 0)),
                Cell::new(CellType::Mouth, (1, 0)),
            ],
            forward_genome(),
        );
        org.energy = energy;
        org
    }

    /// 在 [`forward_genome`] 基础上再打开繁殖输出。
    ///
    /// 单独拆出来而不是直接加进 `forward_genome`，是因为**大多数测试不希望
    /// 被测对象半路生孩子**——数量一变，断言里的坐标、占格数、能量账全都对不上。
    fn breeder_genome() -> Genome {
        let mut g = forward_genome();
        g.weights[Genome::b_o(OUT_REPRODUCE)] = 2.0;
        g
    }

    // -----------------------------------------------------------------------
    // 水平环绕
    // -----------------------------------------------------------------------

    #[test]
    fn a_fish_wraps_around_the_right_edge() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 64 宽的世界，锚点 63 向右走一格应当回到 0。
        let id = sim.spawn_injected(&mut w, swimmer((63, 40), 500)).unwrap();
        sim.step(&mut w);
        assert_eq!(sim.organism(id).unwrap().anchor.0, 0, "右边界应当接回左边");
        assert_eq!(w.occupied_count(), 2, "环绕时身体不能散架");
    }

    #[test]
    fn a_fish_wraps_around_the_left_edge() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let id = sim.spawn_injected(&mut w, swimmer((0, 40), 500)).unwrap();
        // 强行让它往左走一步。
        {
            let org = &mut sim.organisms_mut()[0];
            org.genome = Genome::blank();
            org.genome.weights[Genome::b_o(OUT_LEFT)] = 2.0;
        }
        sim.step(&mut w);
        assert_eq!(sim.organism(id).unwrap().anchor.0, 63, "左边界应当接回右边");
        assert_eq!(w.occupied_count(), 2);
    }

    #[test]
    fn wrapping_keeps_the_body_inside_the_world() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let id = sim.spawn_injected(&mut w, swimmer((62, 40), 5000)).unwrap();

        for _ in 0..200 {
            sim.step(&mut w);
            let org = sim.organism(id).unwrap();
            for (x, y) in org.footprint() {
                assert!(
                    (0..w.width() as i32).contains(&x),
                    "环绕之后坐标必须在界内，实际 x={x}"
                );
                assert!((0..w.height() as i32).contains(&y));
            }
            assert_eq!(w.occupied_count(), 2, "身体始终完整");
        }
    }

    #[test]
    fn wrapping_never_loses_water() {
        let mut w = pool();
        let before = water_volume(&w);
        let mut sim = LifeSim::new(&mut w);
        let id = sim.spawn_injected(&mut w, swimmer((62, 40), 5000)).unwrap();

        for _ in 0..300 {
            sim.step(&mut w);
            assert_eq!(water_volume(&w), before, "环绕移动只能搬水，不能吞水");
        }
        assert!(sim.organism(id).is_some());
    }

    /// 隔着接缝的两条鱼在领地判定里应当算邻居。
    #[test]
    fn crowding_counts_across_the_seam() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.spawn_injected(&mut w, swimmer((0, 40), 500)).unwrap();
        // 身体两格，所以第二条放 62 才不会和第一条跨接缝重叠。
        sim.spawn_injected(&mut w, swimmer((62, 40), 500)).unwrap();
        // 两者水平距离是 2（跨接缝算），远小于 CROWD_RADIUS。
        let need = sim.organisms()[0].reproduction_energy();
        sim.organisms_mut()[0].energy = need;
        sim.ledger.injected += need as u64;
        // 附近只有 1 条同类，达不到 CROWD_LIMIT，所以能生。
        assert!(
            sim.reproduce(&mut w, 1).is_some(),
            "跨接缝的邻居不该被当成很远"
        );
    }

    // -----------------------------------------------------------------------
    // 谱系与观测
    // -----------------------------------------------------------------------

    #[test]
    fn a_child_remembers_its_parent_and_generation() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 能量按体型现算，不写死 400：三格鱼的繁殖门槛是 402，
        // 写 400 会让这条测试在门槛改动之后莫名其妙地红。
        let mut fish = LifeSim::simple_fish((20, 25), Genome::blank(), 0);
        fish.energy = fish.reproduction_energy();
        let parent = sim.spawn_injected(&mut w, fish).unwrap();
        assert_eq!(sim.organism(parent).unwrap().generation, 0);
        assert!(sim.organism(parent).unwrap().parent.is_none());

        let child = sim.reproduce(&mut w, parent).expect("能量够，应当生得出来");
        let child = sim.organism(child).unwrap();
        assert_eq!(child.parent, Some(parent));
        assert_eq!(child.generation, 1);
    }

    #[test]
    fn generations_keep_counting_up() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let mut fish = LifeSim::simple_fish((20, 25), Genome::blank(), 0);
        fish.energy = fish.reproduction_energy();
        let mut id = sim.spawn_injected(&mut w, fish).unwrap();

        for expected in 1..=4 {
            let next = sim.reproduce(&mut w, id).expect("应当生得出来");
            assert_eq!(sim.organism(next).unwrap().generation, expected);
            // 给子代足够能量，让它继续生。
            let need = sim.organism(next).unwrap().reproduction_energy();
            let deficit = need.saturating_sub(sim.organism(next).unwrap().energy);
            sim.organisms_mut()
                .iter_mut()
                .find(|o| o.id == next)
                .unwrap()
                .energy = need;
            sim.ledger.injected += deficit as u64;
            id = next;
        }
    }

    #[test]
    fn the_founder_is_exactly_the_first_organism() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        assert!(sim.founder().is_none());
        sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 25), Genome::blank(), 100))
            .unwrap();
        assert!(sim.founder().is_some());
        assert_eq!(sim.gene_distance(), 0.0, "初代自己的距离必须是 0");
    }

    #[test]
    fn gene_distance_grows_as_mutations_accumulate() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 25), Genome::blank(), 100))
            .unwrap();
        assert_eq!(sim.gene_distance(), 0.0);

        // 手动把一条鱼"变异"成完全不同的基因组，距离必须立刻涨上去。
        sim.organisms_mut()[0].genome.weights.fill(1.0);
        assert!(
            sim.gene_distance() > 0.5,
            "权重全改成 1.0 之后距离应当接近 1，实际 {}",
            sim.gene_distance()
        );
    }

    #[test]
    fn direction_picking_respects_the_threshold() {
        let mut out = [0.0f32; OUTPUTS];
        assert!(pick_direction(&out).is_none(), "全零不该动");
        out[OUT_UP] = MOVE_THRESHOLD + 0.01;
        assert_eq!(pick_direction(&out), Some((0, -1)));
        out[OUT_RIGHT] = 0.9;
        assert_eq!(pick_direction(&out), Some((1, 0)), "更强的方向胜出");
    }

    #[test]
    fn a_fish_swims_forward_on_its_own() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let id = sim.spawn_injected(&mut w, swimmer((10, 40), 200)).unwrap();

        for _ in 0..10 {
            sim.step(&mut w);
        }

        let anchor = sim.organism(id).unwrap().anchor;
        assert_eq!(anchor, (20, 40), "十 tick 应该正好游十格");
    }

    #[test]
    fn movement_costs_energy_and_the_upkeep() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim.spawn_injected(&mut w, swimmer((10, 40), 100)).unwrap();

        sim.step(&mut w);

        // 移动 1 × 质量(2+1) = 3；维持 4(移动) + 2(嘴) = 6。合计 9。
        assert_eq!(sim.organism(id).unwrap().energy, 91);
        assert!(sim.audit(&w).is_balanced());
    }

    #[test]
    fn a_fish_stops_at_stone() {
        let mut w = pool();
        w.set(16, 40, MaterialKind::Stone);
        let mut sim = LifeSim::new(&mut w);
        let id = sim.spawn_injected(&mut w, swimmer((10, 40), 200)).unwrap();

        for _ in 0..20 {
            sim.step(&mut w);
        }

        // 身体是 [移动, 嘴]，所以锚点停在 14 时嘴正好顶在 15，
        // 再往前一步就要把嘴塞进 16 的石头里。
        assert_eq!(sim.organism(id).unwrap().anchor.0, 14);
    }

    #[test]
    fn a_swimming_fish_never_tears_its_body() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.spawn_injected(&mut w, swimmer((10, 40), 500)).unwrap();

        for _ in 0..50 {
            sim.step(&mut w);
            assert_eq!(w.occupied_count(), 2, "身体必须始终是完整的两格");
        }
    }

    #[test]
    fn being_born_borrows_water_and_dying_returns_it() {
        let mut w = pool();
        let before = water_volume(&w);
        let mut sim = LifeSim::new(&mut w);
        let id = sim.spawn_injected(&mut w, swimmer((10, 40), 5000)).unwrap();

        // 出生"借走"两格水：水位会掉两格，但总量守恒。
        assert_eq!(
            w.count_of(MaterialKind::Water),
            before - 2,
            "两格身体借走两格水"
        );
        assert_eq!(water_volume(&w), before, "借不是丢");

        // 后半段是回归测试：早先按"第 k 个细胞的旧位置 → 第 k 个细胞的新位置"
        // 搬运流体，身体内部重叠时会把水换进**新身体内部**。那格随即被占成细胞，
        // 于是鱼肚子里藏了一格水；鱼死后那格被写成残骸，水就凭空没了。
        // 所以这里同时断言身体格是空的。
        for _ in 0..200 {
            sim.step(&mut w);
            if let Some(org) = sim.organism(id) {
                for (x, y) in org.footprint() {
                    assert_eq!(
                        w.get(x as isize, y as isize),
                        MaterialKind::Empty,
                        "身体格 ({x}, {y}) 里不该有材质"
                    );
                }
            }
            assert_eq!(water_volume(&w), before, "游动只能搬水，一格都不能少");
        }
    }

    #[test]
    fn a_fish_eats_what_its_mouth_touches() {
        let mut w = pool();
        w.set(16, 40, MaterialKind::Remains);
        w.set_flags(16, 40, 20);
        // 再往前一格是石头，于是鱼吃完就停在那儿，不会游过头。
        w.set(17, 40, MaterialKind::Stone);
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        sim.spawn_injected(&mut w, swimmer((10, 40), 400)).unwrap();

        for _ in 0..20 {
            sim.step(&mut w);
        }

        assert_eq!(w.count_of(MaterialKind::Remains), 0, "残骸应该被吃掉");
        assert!(sim.audit(&w).is_balanced());
    }

    #[test]
    fn a_body_too_big_cannot_push_through_water() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 关掉藻：藻是可以穿过去的，长在这条鱼前面会让它走起来，
        // 于是测到的就不是"水挡住了大生物"。
        sim.set_food_growth(false);

        // 造一条比 `SMALL_BODY_MAX` 更长的鱼，塞在水的正中间。
        let cells: Vec<Cell> = (0..(SMALL_BODY_MAX as i16 + 2))
            .map(|i| Cell::new(CellType::Mover, (i, 0)))
            .collect();
        let mut big = Organism::new(0, (10, 40), cells, forward_genome());
        // 18 个移动细胞每 tick 要 72 维持，给少了它会在跑满 10 tick 之前先饿死，
        // 于是测试测到的是"饿死"而不是"被水挡住"。
        big.energy = 5_000;

        // 出生时水被挤开，所以它现在悬在一个空腔里；
        // 空腔之外全是水，它一格也走不动。
        let id = sim.spawn_injected(&mut w, big).unwrap();
        let start = sim.organism(id).unwrap().anchor;

        for _ in 0..10 {
            sim.step(&mut w);
        }
        assert_eq!(sim.organism(id).unwrap().anchor, start, "大生物被水挡住");
    }

    // -----------------------------------------------------------------------
    // 手工体型
    // -----------------------------------------------------------------------

    /// 每个手工体型都得能**完整地**游过一片水。
    ///
    /// 这条测试盯的是三件同时成立的事：身体不散架（占格数恒定）、
    /// 水一格不少（推水逻辑对多格身体仍然成立）、能量账仍然平。
    /// 十几格的身体在这三件事上都比两格身体容易出错：搬运时的 `vacated`
    /// 配对、镜像翻转时的落点、以及 O(n²) 的自重叠判定。
    #[test]
    fn every_plan_swims_without_tearing_or_losing_water() {
        for plan in PLANS {
            let mut w = pool();
            let mut sim = LifeSim::new(&mut w);
            let volume = water_volume(&w);
            let id = sim
                .spawn_injected(
                    &mut w,
                    LifeSim::from_plan(plan, (30, 40), forward_genome(), 20_000),
                )
                .expect("应当放得下");
            assert_eq!(
                w.occupied_count(),
                plan.len(),
                "「{}」没占满身体",
                plan.name
            );

            for tick in 0..120 {
                sim.step(&mut w);
                assert_eq!(
                    w.occupied_count(),
                    plan.len(),
                    "「{}」在第 {tick} tick 散架了",
                    plan.name
                );
                assert_eq!(
                    water_volume(&w),
                    volume,
                    "「{}」在第 {tick} tick 把水弄丢了",
                    plan.name
                );
            }
            assert!(sim.organism(id).is_some(), "「{}」饿死了", plan.name);
            assert!(sim.audit(&w).is_balanced(), "「{}」的能量账不平", plan.name);
        }
    }

    /// 掉头时整个身体沿 x 翻过来，而且不留下没人认领的格子。
    #[test]
    fn turning_around_flips_the_body_in_place() {
        let plan = &crate::body::SILVERFISH;
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::from_plan(plan, (30, 40), forward_genome(), 20_000),
            )
            .unwrap();

        // 朝右：锚点 30，嘴在 31 与 32。
        let right: Vec<i32> = sim.organism(id).unwrap().footprint().map(|p| p.0).collect();
        assert!(right.contains(&32), "朝右时嘴应当在锚点右侧两格");

        // 把"往右"的偏置换成"往左"，让它掉头。
        {
            let org = &mut sim.organisms_mut()[0];
            org.genome = Genome::blank();
            org.genome.weights[Genome::b_o(OUT_LEFT)] = 2.0;
        }
        sim.step(&mut w);

        let org = sim.organism(id).unwrap();
        assert_eq!(org.anchor.0, 29, "应当向左走了一格");
        assert_eq!(org.facing.x, -1);
        assert_eq!(w.occupied_count(), plan.len(), "翻面之后身体不能散");
        let left: Vec<i32> = org.footprint().map(|p| p.0).collect();
        assert!(left.contains(&27), "朝左时嘴必须翻到锚点左侧两格");
        assert!(!left.contains(&32), "旧朝向上的格子必须被让出来");
    }

    /// 探针修正之前，手工鱼其实一直在**往左**游。
    ///
    /// 原因是身体占住锚点右侧那一格，于是"右边被挡"恒为真，
    /// 而基因组里"右边被挡就往左"的权重是 10.0——它压过了往右的偏置。
    /// 这条 bug 之所以没被发现，是因为撞到左墙会触发反方向，
    /// 看起来就像一条正常游动的鱼。
    #[test]
    fn the_handcrafted_fish_swims_toward_its_face() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 关掉长藻。这条测的是"朝哪边游"，而手工基因组里有觅食权重
        // （闻到背后的食物会掉头），藻一长出来它就会来回追——
        // 那是对的行为，只是不该混进这条断言里。
        sim.set_food_growth(false);
        let mut genome = handcrafted_fish_genome(Tactic::Ignore);
        // 关掉繁殖，否则它会半路生孩子，锚点就不好断言了。
        genome.weights[Genome::b_o(OUT_REPRODUCE)] = 0.0;
        let mut fish = LifeSim::simple_fish((20, 40), genome, 2_000);
        fish.facing = crate::life::Facing::RIGHT;
        let id = sim.spawn_injected(&mut w, fish).unwrap();

        for _ in 0..30 {
            sim.step(&mut w);
        }
        let moved = sim.organism(id).unwrap().anchor.0;
        assert!(moved > 25, "朝右的手工鱼应当往右游，实际只走到 {moved}");
    }

    // -----------------------------------------------------------------------
    // 藻席与嗅觉
    // -----------------------------------------------------------------------

    /// 藻长在**水里**、带着能量（鱼才愿意吃）。
    #[test]
    fn algae_grow_in_the_water_and_carry_energy() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let before = sim.audit(&w).injected;

        // 两层都要推进：`tick_organics` 按 `world.tick()` 取模决定扫不扫，
        // 而推进 tick 的是**材质层**（`MaterialSim::step`）。只跑生命层的话
        // tick 永远是 0，于是"每 25 tick 一次"变成"每 tick 一次"——
        // 这个坑在测试里比在游戏里更容易踩到。
        let mut material = crate::sim::MaterialSim::new();
        for _ in 0..600 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        let algae = w.count_of(MaterialKind::Algae);
        assert!(algae > 10, "水里应当长出藻，实际 {algae} 格");
        // 每一格藻都是新注入的能量，账上必须看得到。
        assert!(
            sim.audit(&w).injected > before,
            "藻的能量必须记进注入，否则守恒断言会报'多了 N 点'"
        );
        assert!(sim.audit(&w).is_balanced(), "长藻不能破坏能量账");
    }

    /// 藻**只长在水里**：空气里一格都不该有。
    ///
    /// 这条比"长在某个特定位置"更重要：藻是可移动的，所以它最终停在哪
    /// 由水流决定，断言"必须贴着海底"会随实现变化而失效。而"绝不在空气里"
    /// 是**不变量**——它保证藻不会飘到天上，变成一个谁也吃不到、
    /// 还把能量锁死的状态。
    #[test]
    fn algae_never_grow_in_the_air() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let mut material = crate::sim::MaterialSim::new();
        for _ in 0..600 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        let (ww, h) = (w.width(), w.height());
        let mut found = 0;
        for y in 0..h {
            for x in 0..ww {
                let (xs, ys) = (x as isize, y as isize);
                if w.get(xs, ys) != MaterialKind::Algae {
                    continue;
                }
                found += 1;
                // 四邻里至少得有一格是水或藻：藻是悬浮**在水里**的。
                //
                // 判据不能写成"上下左右全是水"：藻会一路被水顶到最上面那一行，
                // 于是它上面是空气、旁边是别的藻——那完全正常。
                // 真正要排除的是"四周全是空气"，也就是飘到天上了。
                let in_water = [(1, 0), (-1, 0), (0, 1), (0, -1)].iter().any(|&(dx, dy)| {
                    matches!(
                        w.get(xs + dx, ys + dy),
                        MaterialKind::Water | MaterialKind::Algae
                    )
                });
                assert!(in_water, "({x}, {y}) 的藻四周没有水——它飘出水面了");
            }
        }
        assert!(found > 0, "六百 tick 之后应当有藻");
    }

    /// 鱼**能游进**藻里，而且那格藻被挤到别处，不是被销毁。
    ///
    /// 这一条是"藻不能当墙"的验收：如果藻是固体，鱼会被挡住；如果藻是
    /// 液体但占格时被直接清掉，能量账会差一截（藻的能量记在 `flags` 里）。
    #[test]
    fn a_fish_swims_into_algae_and_pushes_it_aside() {
        let mut w = pool();
        // 在鱼正前方放一格藻。
        w.set(24, 40, MaterialKind::Algae);
        w.set_flags(24, 40, FOOD_ENERGY as u8);
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // 只往右游、**不吃**：`forward_genome` 的 `OUT_EAT` 是开的，
        // 用它的话测到的会是"鱼把藻吃了"，而不是"鱼把藻挤开了"。
        let mut grazer = Genome::blank();
        grazer.weights[Genome::b_o(OUT_RIGHT)] = 2.0;
        let mut fish = Organism::new(
            0,
            (20, 40),
            vec![
                Cell::new(CellType::Mover, (0, 0)),
                Cell::new(CellType::Mouth, (1, 0)),
            ],
            grazer,
        );
        fish.energy = 500;
        let id = sim.spawn_injected(&mut w, fish).unwrap();
        // 快照放在投放**之后**：投放会把鱼的能量记成注入，
        // 放在之前比出来的差值里会混进那 500 点。
        let before = sim.audit(&w);

        for _ in 0..6 {
            sim.step(&mut w);
        }

        let x = sim.organism(id).unwrap().anchor.0;
        assert!(x > 21, "鱼应当游过那一格藻，实际停在 x={x}");
        // 藻还在世界里，只是被挤开了——能量一格不少。
        assert_eq!(w.count_of(MaterialKind::Algae), 1, "藻被销毁了");
        // 比的是**相对量**：手工塞进世界的那格藻不在 `LifeSim::new` 的
        // 初始账里（它只把 worldgen 的碎屑折成注入），所以绝对值本来就差 120。
        // 这一条要验的是"挤开动作本身没有增减能量"，相对比较才是对的判据。
        let after = sim.audit(&w);
        assert_eq!(
            after.accounted(),
            before.accounted(),
            "挤开不该改变能量：{} → {}",
            before.accounted(),
            after.accounted()
        );
    }

    /// 嗅觉：闻得到水后面的食物，闻不到石头后面的。
    #[test]
    fn smell_goes_through_water_but_not_through_stone() {
        let mut w = pool();
        w.set(20, 45, MaterialKind::Remains);
        w.set_flags(20, 45, FOOD_ENERGY as u8);
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim.spawn_injected(&mut w, swimmer((20, 40), 400)).unwrap();
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();

        let inp = sim.sense(&w, i);
        assert!(inp[IN_FOOD_DIR_Y] > 0.1, "下方有食物，垂直分量应当为正");
        assert!(
            inp[IN_FOOD_DIR_X].abs() < 1e-6,
            "食物在正下方，不该有水平分量"
        );

        // 中间隔一层石头，味道就断了。
        w.set(20, 42, MaterialKind::Stone);
        let inp = sim.sense(&w, i);
        assert!(
            inp[IN_FOOD_DIR_Y].abs() < 1e-6,
            "石头应当挡住味道，实际垂直分量 {}",
            inp[IN_FOOD_DIR_Y]
        );
    }

    /// 嗅觉通道**接上了**：闻到下方有食物时，"往下"的输出确实更强。
    ///
    /// 这条刻意**不**断言"鱼会下潜"。第一版断言过，而且为了让它在 5 格外
    /// 成立，把权重调到了 8.0——结果是负收益（种群 52 → 37）。
    ///
    /// 想清楚之后发现那是在错的层解决问题：手工基因组的职责是"会游、会吃、会生"
    /// 这条基线，不是替进化决定"该不该觅食"。所以现在只保证通道存在且方向正确，
    /// 权重压到和巡航偏置同一量级——**要不要用它，交给突变。**
    #[test]
    fn the_smell_channel_is_wired_in_the_right_direction() {
        let mut g = handcrafted_fish_genome(Tactic::Ignore);
        g.weights[Genome::b_o(OUT_REPRODUCE)] = 0.0;

        let mut inputs = [0.0f32; INPUTS];
        let quiet = g.think(&inputs);

        inputs[IN_FOOD_DIR_Y] = 1.0; // 食物在下方
        let below = g.think(&inputs);
        assert!(
            below[OUT_DOWN] > quiet[OUT_DOWN] + 0.05,
            "闻到下方有食物时应当更想往下：{} vs {}",
            below[OUT_DOWN],
            quiet[OUT_DOWN]
        );
        assert!(below[OUT_UP] < quiet[OUT_UP] - 0.05, "同时应当更不想往上");

        // 反方向也要对：食物在上方就反过来。
        inputs[IN_FOOD_DIR_Y] = -1.0;
        let above = g.think(&inputs);
        assert!(above[OUT_UP] > quiet[OUT_UP] + 0.05, "食物在上方时想往上");
    }

    /// 在**真实地貌**里藻席也得长得出来。
    ///
    /// 单元测试里用 `pool()` 是平底，真实地貌有沙层、有起伏的海床、
    /// 还有岛坡。条件写得太紧（比如只认石头）时，平底能过长、真地貌不长。
    #[test]
    fn algae_grow_in_a_generated_sea() {
        let mut w = World::new(128, 128).unwrap();
        crate::worldgen::sea_and_islands(&mut w, &mut Rng::new(7));
        let mut sim = LifeSim::new(&mut w);
        let mut material = crate::sim::MaterialSim::new();
        let before = sim.food_cells(&w);

        for _ in 0..2000 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        let after = sim.food_cells(&w);
        assert!(
            after > before + 20,
            "真实的海里应当长出藻：{before} → {after}"
        );
        assert!(sim.audit(&w).is_balanced());
    }

    /// 尸体泡在水里会烂掉，而且**剩下的能量要记进耗散**。
    ///
    /// 这条规则从材质层搬到了生命层，理由是记账：材质层没有账本，
    /// 它悄悄清掉一格带能量的尸体，守恒断言就会报"多了 N 点"。
    /// 而"只烂 `flags == 0` 的尸体"那条老规则正是为了绕开这个——
    /// 代价是**带能量的尸体永远不烂**（实测残骸占比从 1.7% 爬到 3.3%）。
    #[test]
    fn corpses_rot_and_their_leftover_energy_is_dissipated() {
        let mut w = pool();
        w.set(30, 40, MaterialKind::Corpse);
        w.set_flags(30, 40, 60);
        let mut sim = LifeSim::new(&mut w);
        let injected = sim.audit(&w).injected;
        assert_eq!(injected, 60, "尸体的能量要算进初始注入");

        let mut material = crate::sim::MaterialSim::new();
        for _ in 0..2000 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        assert_eq!(w.count_of(MaterialKind::Corpse), 0, "尸体应当烂光");
        let audit = sim.audit(&w);
        assert!(
            audit.dissipated >= 60,
            "烂掉的尸体带走了 60 点能量，必须记进耗散：{audit:?}"
        );
        assert!(audit.is_balanced(), "差额 {}", audit.leak());
    }

    /// 天然碎屑**不会**烂——否则"没有生物的世界也能完全静止"就不成立了。
    #[test]
    fn natural_debris_never_rots() {
        let mut w = pool();
        w.set(30, 40, MaterialKind::Remains);
        w.set_flags(30, 40, 25);
        let mut sim = LifeSim::new(&mut w);
        // 关掉藻席：藻也是 `Remains`，会把"碎屑有几格"数乱。
        sim.set_food_growth(false);
        let mut material = crate::sim::MaterialSim::new();
        for _ in 0..4000 {
            sim.step(&mut w);
            material.step(&mut w);
        }
        assert_eq!(
            w.count_of(MaterialKind::Remains),
            1,
            "天然碎屑是沉积物，不该消失"
        );
    }

    /// 露在空气里的尸体也会烂，但慢得多——**不能是"永远不烂"**。
    ///
    /// 概率给 0 就是个吸收态：鱼会游到水面之上的天空里死掉，
    /// 那层尸体永远不消失，实测 8000 tick 堆了 4.3 万格。
    #[test]
    fn dry_corpses_rot_much_slower_than_submerged_ones() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 60, 64, 4, MaterialKind::Stone); // 海底
        w.fill_rect(0, 30, 64, 30, MaterialKind::Water); // 水体
        w.fill_rect(4, 20, 24, 1, MaterialKind::Stone); // 水面之上的石台
        w.fill_rect(4, 19, 24, 1, MaterialKind::Corpse); // 24 格干尸体
        w.fill_rect(36, 40, 24, 1, MaterialKind::Corpse); // 24 格湿尸体

        let mut sim = LifeSim::new(&mut w);
        let mut material = crate::sim::MaterialSim::new();
        for _ in 0..600 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        let dry = (4..28)
            .filter(|&x| w.get(x as isize, 19) == MaterialKind::Corpse)
            .count();
        let wet = w.count_of(MaterialKind::Corpse) - dry;
        assert!(wet <= 6, "泡在水里的尸体应当烂掉大半，还剩 {wet} 格");
        assert!(dry >= 12, "露在空气里的尸体应当还在，只剩 {dry} 格");
    }

    /// 堆到水面之上的尸体也必须能烂掉。
    ///
    /// 这是"吸收态"那条规则的回归测试：早期版本里堆顶露出空气就永远不烂，
    /// 于是堆越高 → 越多格子露在空气里 → 越不烂 → 堆越高。
    #[test]
    fn a_corpse_pile_above_the_waterline_still_rots() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 60, 64, 4, MaterialKind::Stone);
        w.fill_rect(0, 20, 64, 40, MaterialKind::Water);
        w.fill_rect(30, 0, 1, 60, MaterialKind::Corpse);
        assert_eq!(w.count_of(MaterialKind::Corpse), 60);

        let mut sim = LifeSim::new(&mut w);
        let mut material = crate::sim::MaterialSim::new();
        for _ in 0..6000 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        let left = w.count_of(MaterialKind::Corpse);
        assert!(left < 5, "堆到水面之上的尸体也必须分解，实际还剩 {left} 格");
    }

    /// **繁殖也不能让水位漂移。**
    ///
    /// 出生借水、尸体腐烂还水，一来一回必须严丝合缝。这条测试盯的就是那个"来回"：
    /// 只要有一边算错（比如子代生在空气里、没借到水，死后却还了一格），
    /// 水位就会随繁殖次数单调上涨——实测过：32000 tick 涨了 18000 格，
    /// 而能量账一分不差、生物数还在涨，从任何别的指标上都看不出来。
    #[test]
    fn reproduction_does_not_drift_the_water_level() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let before = water_volume(&w);
        // 一个只会生孩子、不会游的基因组：把繁殖路径压到最密。
        let mut genome = Genome::blank();
        genome.weights[Genome::b_o(OUT_REPRODUCE)] = 3.0;
        genome.weights[Genome::b_o(OUT_EAT)] = 3.0;
        let mut fish = LifeSim::simple_fish((20, 40), genome, 2000);
        fish.energy = fish.reproduction_energy() * 4;
        sim.spawn_injected(&mut w, fish).unwrap();

        for tick in 0..600 {
            sim.step(&mut w);
            assert_eq!(
                water_volume(&w),
                before,
                "第 {tick} tick 水位漂了：出生借的水和尸体还的水对不上"
            );
        }
    }

    // -----------------------------------------------------------------------
    // 追 / 逃
    // -----------------------------------------------------------------------

    /// 感知到"别的生物在哪边"——射线撞到别人就停。
    ///
    /// 判据必须是**占格表**而不是材质：身体格的材质是 `Empty`，
    /// 按材质判的话射线会"穿过"一条鱼继续往后看。
    #[test]
    fn a_body_senses_where_the_nearest_neighbour_is() {
        let sense_with_neighbour_at = |y: i32| {
            let mut w = pool();
            let mut sim = LifeSim::new(&mut w);
            sim.set_food_growth(false);
            let me = sim.spawn_injected(&mut w, swimmer((20, 40), 400)).unwrap();
            sim.spawn_injected(&mut w, swimmer((20, y), 400)).unwrap();
            let i = sim.organisms.iter().position(|o| o.id == me).unwrap();
            sim.sense(&w, i)
        };

        let below = sense_with_neighbour_at(50);
        assert!(
            below[IN_ORGANISM_DIR_Y] > 0.0,
            "别人在正下方，垂直分量应当为正：{}",
            below[IN_ORGANISM_DIR_Y]
        );
        assert!(
            below[IN_ORGANISM_DIR_X].abs() < 1e-6,
            "正下方不该有水平分量"
        );

        let above = sense_with_neighbour_at(30);
        assert!(
            above[IN_ORGANISM_DIR_Y] < 0.0,
            "别人在正上方，垂直分量应当为负：{}",
            above[IN_ORGANISM_DIR_Y]
        );
    }

    /// 有牙的靠过去，没牙的躲开——同一个输入，两种倾向给出相反的动作。
    ///
    /// 这条是"追 / 逃"的全部实现。它刻意做成**基因组里的权重**而不是引擎里的规则：
    /// 引擎只提供"别人在哪边"，怎么反应是基因的事，突变能改。
    #[test]
    fn hunters_approach_and_prey_flee() {
        let mut inputs = [0.0f32; INPUTS];
        inputs[IN_ORGANISM_DIR_Y] = 0.5; // 别人在下方

        let hunt = handcrafted_fish_genome(Tactic::Hunt).think(&inputs);
        assert!(
            hunt[OUT_DOWN] > hunt[OUT_UP],
            "有牙的应当往下（追）：down {} vs up {}",
            hunt[OUT_DOWN],
            hunt[OUT_UP]
        );

        let flee = handcrafted_fish_genome(Tactic::Ignore).think(&inputs);
        assert!(
            flee[OUT_UP] > flee[OUT_DOWN],
            "没牙的应当往上（逃）：up {} vs down {}",
            flee[OUT_UP],
            flee[OUT_DOWN]
        );
    }

    // -----------------------------------------------------------------------
    // 形状 → 能力
    // -----------------------------------------------------------------------

    /// 一格移动细胞的对照体型。
    const ONE_MOVER: crate::body::BodyPlan = crate::body::BodyPlan {
        name: "单动",
        cells: &[
            (CellType::Mover, 0, 0),
            (CellType::Mouth, 1, 0),
            (CellType::Producer, 0, -1),
        ],
    };

    /// 同上，但多一只眼：嗅觉半径 20 + 4 = 24。
    const ONE_MOVER_EYE: crate::body::BodyPlan = crate::body::BodyPlan {
        name: "单动带眼",
        cells: &[
            (CellType::Mover, 0, 0),
            (CellType::Mouth, 1, 0),
            (CellType::Producer, 0, -1),
            (CellType::Eye, 0, 1),
        ],
    };

    /// 四格移动细胞：按 `MOVERS_PER_EXTRA_STEP = 4`，它每 tick 该走 2 格。
    const FOUR_MOVERS: crate::body::BodyPlan = crate::body::BodyPlan {
        name: "四动",
        cells: &[
            (CellType::Mover, 0, 0),
            (CellType::Mover, -1, 0),
            (CellType::Mover, -2, 0),
            (CellType::Mover, -3, 0),
            (CellType::Mouth, 1, 0),
            (CellType::Producer, 0, -1),
        ],
    };

    /// 移动细胞多就跑得远——**一个 tick 走两格**，不是"看起来快"。
    #[test]
    fn more_movers_means_more_cells_per_tick() {
        let mut moved = Vec::new();
        for plan in [&ONE_MOVER, &FOUR_MOVERS] {
            let mut w = pool();
            let mut sim = LifeSim::new(&mut w);
            sim.set_food_growth(false);
            let id = sim
                .spawn_injected(
                    &mut w,
                    LifeSim::from_plan(plan, (10, 40), forward_genome(), 500),
                )
                .unwrap();
            sim.step(&mut w);
            moved.push(sim.organism(id).unwrap().anchor.0 - 10);
        }
        assert_eq!(moved[0], 1, "一格移动细胞只该走一格");
        assert_eq!(moved[1], 2, "四格移动细胞应当走两格，实际 {}", moved[1]);
    }

    /// 眼多就闻得远：食物在 22 格外，零只眼（半径 20）闻不到，一只眼（24）闻得到。
    #[test]
    fn more_eyes_means_a_longer_smell_ray() {
        let smell = |plan: &crate::body::BodyPlan, food: bool| {
            let mut w = pool();
            if food {
                // 正下方 22 格：基础半径 20 够不着，加一只眼刚好够。
                w.set(20, 52, MaterialKind::Algae);
                w.set_flags(20, 52, FOOD_ENERGY as u8);
            }
            let mut sim = LifeSim::new(&mut w);
            sim.set_food_growth(false);
            let id = sim
                .spawn_injected(
                    &mut w,
                    LifeSim::from_plan(plan, (20, 30), Genome::blank(), 500),
                )
                .unwrap();
            let i = sim.organisms.iter().position(|o| o.id == id).unwrap();
            sim.sense(&w, i)[IN_FOOD_DIR_Y]
        };

        assert_eq!(smell(&ONE_MOVER, false), 0.0, "没有食物时不该闻到东西");
        assert_eq!(
            smell(&ONE_MOVER, true),
            0.0,
            "零只眼、22 格外的食物——半径 20 不该闻到"
        );
        assert!(
            smell(&ONE_MOVER_EYE, true) > 0.0,
            "加一只眼（半径 24）就该闻到 22 格外的食物"
        );
    }

    // -----------------------------------------------------------------------
    // 捕食
    // -----------------------------------------------------------------------

    /// 摆一对"捕食者在左、猎物在右"的邻居。
    ///
    /// 位置是算出来的，不是试出来的：掠食者的两枚杀手在锚点 `+1`/`+2`，
    /// 所以它的身体占 `x ∈ [18, 22]`；猎物锚点放 25 时身体从 23 起，
    /// 正好让 `(22, 45)` 的杀手贴住 `(23, 45)` 的移动细胞。
    fn hunter_and_prey(plan: &crate::body::BodyPlan) -> (World, LifeSim, OrganismId, OrganismId) {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let hunter = sim
            .spawn_injected(
                &mut w,
                LifeSim::from_plan(&crate::body::HUNTER, (20, 45), Genome::blank(), 100),
            )
            .expect("掠食者应当放得下");
        let prey = sim
            .spawn_injected(
                &mut w,
                LifeSim::from_plan(plan, (25, 45), Genome::blank(), 1000),
            )
            .expect("猎物应当放得下");
        (w, sim, hunter, prey)
    }

    /// 杀手细胞咬邻格的**别的生物**，能量是转移不是凭空产生。
    #[test]
    fn killer_cells_bite_neighbours_and_energy_only_moves() {
        let (mut w, mut sim, hunter, prey) = hunter_and_prey(&crate::body::SILVERFISH);

        // 先单独跑一次 `predate`：不掺阳光、不掺代谢，账面上必须分毫不差。
        let before = sim.audit(&w).accounted();
        let prey_before = sim.organism(prey).unwrap().energy;
        let hunter_before = sim.organism(hunter).unwrap().energy;
        sim.predate(&w);
        let prey_after = sim.organism(prey).unwrap().energy;
        let hunter_after = sim.organism(hunter).unwrap().energy;
        assert!(prey_after < prey_before, "猎物应当掉能量");
        assert!(hunter_after > hunter_before, "捕食者应当涨能量");
        assert_eq!(
            sim.audit(&w).accounted(),
            before,
            "捕食是转移：生物 + 残骸的总量必须不变（咽不下的进耗散）"
        );

        for _ in 0..5 {
            sim.step(&mut w);
        }
        assert!(sim.stats().bitten > 0, "应当发生过咬");
        assert!(sim.audit(&w).is_balanced(), "差额 {}", sim.audit(&w).leak());
    }

    /// 装甲挡住一部分咬——这是装甲细胞存在的理由。
    #[test]
    fn armor_cells_absorb_part_of_the_bite() {
        let (mut w, mut sim, _hunter, soft) = hunter_and_prey(&crate::body::SILVERFISH);
        for _ in 0..5 {
            sim.step(&mut w);
        }
        let soft_loss = 1000 - sim.organism(soft).map_or(0, |o| o.energy);

        let (mut w2, mut sim2, _hunter2, hard) = hunter_and_prey(&crate::body::LEAFBACK);
        for _ in 0..5 {
            sim2.step(&mut w2);
        }
        let hard_loss = 1000 - sim2.organism(hard).map_or(0, |o| o.energy);

        assert!(
            hard_loss * 2 < soft_loss,
            "叶背鱼有三格装甲，挨的咬应当远少于银鱼：{hard_loss} vs {soft_loss}"
        );
        assert!(hard_loss > 0, "装甲不该完全免疫");
    }

    /// 没有杀手细胞的生物不会咬人——哪怕贴在一起。
    #[test]
    fn a_body_without_killer_cells_never_bites() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        sim.spawn_injected(
            &mut w,
            LifeSim::from_plan(&crate::body::SILVERFISH, (20, 45), Genome::blank(), 1000),
        )
        .unwrap();
        sim.spawn_injected(
            &mut w,
            LifeSim::from_plan(&crate::body::SILVERFISH, (25, 45), Genome::blank(), 1000),
        )
        .unwrap();

        for _ in 0..5 {
            sim.step(&mut w);
        }
        assert_eq!(sim.stats().bitten, 0, "银鱼没有杀手细胞，不该咬到任何东西");
    }

    // -----------------------------------------------------------------------
    // 生态
    // -----------------------------------------------------------------------

    /// 浅池：水面在 y=8，池底是石头。水面附近光足够强。
    fn shallow_pool() -> World {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 60, 64, 4, MaterialKind::Stone);
        w.fill_rect(0, 8, 64, 52, MaterialKind::Water);
        w
    }

    /// **M2 的验收锚点之一**：一条手工设计的鱼，放进浅水能自己活下去并繁殖。
    ///
    /// 这条测试真正在验证的是三件事同时成立：
    /// 1. 生产者的能量产出够付维持成本（平衡没有调歪）
    /// 2. 繁殖链路通（能量 → 子代 → 占格）
    /// 3. 能量账在几百次繁殖之后仍然一分不差
    ///
    /// 它挂了通常不是"某个函数写错"，而是某个常数把生态调到了活不下去的一侧。
    #[test]
    fn a_handcrafted_fish_lives_and_reproduces_in_shallow_water() {
        let mut w = shallow_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.spawn_injected(
            &mut w,
            LifeSim::simple_fish((10, 12), breeder_genome(), 200),
        )
        .unwrap();

        for i in 0..800 {
            sim.step(&mut w);
            // 每 tick 都扫一遍整张网格太贵（audit 是 O(格子数)），
            // 每 100 tick 抽查一次足够抓住任何一处漏账。
            if i % 100 == 0 {
                assert!(
                    sim.audit(&w).is_balanced(),
                    "第 {} tick 能量账不平：{:?}",
                    w.tick(),
                    sim.audit(&w)
                );
            }
        }
        assert!(sim.audit(&w).is_balanced(), "收尾时能量账必须还是平的");

        // 不断言"那一条鱼还活着"：这条鱼是 r 策略，生完一轮之后被自己
        // 的子代挡住光、耗尽能量而死是**正常结局**。要断言的是种群没有灭绝、
        // 而且确实繁殖过。
        assert!(!sim.organisms().is_empty(), "种群不该灭绝");
        assert!(
            sim.organisms().len() > 1,
            "八百 tick 足够繁殖一轮，实际只有 {} 条",
            sim.organisms().len()
        );
    }

    /// 反面：同一条鱼扔进深水，光不够，必然饿死。
    ///
    /// 两条测试合起来才说明"生产者必须待在浅水"是一条**真约束**，
    /// 而不是一句写在文档里的愿望。
    #[test]
    fn the_same_fish_starves_in_deep_water() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 60, 64, 4, MaterialKind::Stone);
        w.fill_rect(0, 8, 64, 52, MaterialKind::Water);

        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((10, 56), breeder_genome(), 200),
            )
            .unwrap();

        for _ in 0..800 {
            sim.step(&mut w);
            if sim.organism(id).is_none() {
                break;
            }
        }

        assert!(sim.organism(id).is_none(), "深水里应该饿死");
        assert!(sim.audit(&w).is_balanced());
    }
}
