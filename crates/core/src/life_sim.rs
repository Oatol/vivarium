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
use crate::life::{
    remains_of, Cell, CellType, Genome, Organism, OrganismId, INPUTS, IN_AGE, IN_ALIEN_AHEAD,
    IN_BLOCKED_AHEAD, IN_BLOCKED_LEFT, IN_BLOCKED_RIGHT, IN_BLOCKED_UP, IN_ENERGY, IN_FACING_X,
    IN_FACING_Y, IN_FOOD_AHEAD, IN_KIN_AHEAD, IN_LIGHT, IN_SUPPORTED_BELOW, OUTPUTS, OUT_DOWN,
    OUT_EAT, OUT_LEFT, OUT_REPRODUCE, OUT_RIGHT, OUT_UP, REMAINS_ENERGY,
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

/// 移动的基础能量成本（再按质量加成）。
pub const MOVE_COST: u32 = 1;

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
            if cell.kind != MaterialKind::Remains {
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
        }
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
        // ## 那一格水去哪了：被**借走**了，不是被挤走、也不是被销毁
        //
        // 早先的写法是"把水挤到附近的空位"，挤不下就顶到这一列的水面之上
        // （水面上升一格，阿基米德）。听着物理，但它有一个致命后果：
        // **被顶上去的水成了下一条鱼出生的落脚点。**
        //
        // 一条鱼贴着水面生孩子，子代的水被顶到亲代头顶；那条子代再生一个，
        // 新子代就站在刚才那格水上；水又被顶高一层……每代往上爬三四格。
        // 实测 512² 跑 8000 tick：水面从 y=153 被顶到 y=107，
        // 四十多条鱼骑在水面上，既不能动也不能繁殖，整个种群冻死在那儿。
        //
        // 现在的规则简单得多：**一格身体占住一格水，那格水记在身体头上。**
        // 身体变成尸体时水还欠着（尸体占着同一格），尸体腐烂（
        // `sweep_remains`）或者被吃掉（`eat_remains`）时还回来。
        // 于是「水 + 活细胞 + 残骸」是个守恒量，水位不会因为出生而漂移，
        // 也没有任何水被搬到高处去。
        //
        // 代价是出生瞬间水位会掉一点点（借走了），尸体腐烂之后回补。
        // 这比"水位被永久顶高"好得多——后者是不可逆的。
        for cell in &org.cells {
            let (x, y) = org.cell_at(cell);
            if world.get(x as isize, y as isize).is_fluid() {
                world.set(x as isize, y as isize, MaterialKind::Empty);
            }
        }

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
            if cell.kind == MaterialKind::Remains {
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
    /// 顺序：太阳能 → 行为（移动 / 进食 / 繁殖）→ 代谢（可能饿死）→ 清理尸体 → 统计。
    ///
    /// 行为放在代谢之前，是因为"这一 tick 能动"应该由"这一 tick 开始时还剩多少能量"
    /// 决定。反过来的话，一个刚好付得起维持成本的生物会先被扣光、再判定能不能动，
    /// 于是永远动不了——那是一个很难从现象反推的死结。
    pub fn step(&mut self, world: &mut World) {
        self.stats = LifeStats::default();

        // 1) 太阳能。生产者是唯一的"环境 → 生物"入口（SPEC §3.3）。
        self.inject_sunlight(world);

        // 2) 行为。
        self.act(world);

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

        // 5) 统计。
        self.stats.organisms = self.organisms.len();
        self.stats.cells = self.organisms.iter().map(|o| o.cell_count()).sum();
        self.stats.stranded = self
            .organisms
            .iter()
            .filter(|o| {
                let cells: Vec<(i32, i32)> = o.footprint().collect();
                !Self::touches_water(world, o.width, &cells)
            })
            .count();
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
        //
        // 这不只是省事——`flags == 0` 是材质层"这具尸体可以被分解"的唯一信号
        // （见 `sim::tick_remains`）。不归零的话，一具还剩 3 点能量的饿死尸体
        // 会永远泡在海底不烂，因为它带着能量、分解规则不敢碰它。
        let per_cell = org.energy / n;
        let per_cell = if per_cell < 4 {
            0
        } else {
            per_cell.min(u8::MAX as u32)
        };
        for cell in &org.cells {
            let (x, y) = org.cell_at(cell);
            let (x, y) = (x as isize, y as isize);
            world.vacate(x, y);
            world.set(x, y, remains_of(cell.kind));
            // `set` 会把 flags 清 0，所以能量必须写在它之后。
            world.set_flags(x, y, per_cell as u8);
        }
        self.ledger.dissipated += (org.energy - per_cell * n) as u64;
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
            self.try_move(world, i, dx, dy);
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
        inp[IN_FOOD_AHEAD] =
            (world.get(fx as isize, fy as isize) == MaterialKind::Remains) as u8 as f32;
        inp[IN_BLOCKED_AHEAD] = (!Self::passable(world, body, fx, fy)) as u8 as f32;

        let owner = world.owner(fx as isize, fy as isize);
        inp[IN_KIN_AHEAD] = (owner.is_some() && owner.organism != org.id) as u8 as f32;
        // M2 没有物种概念，"异种"暂时恒为 0。留着这个通道是为了让网络形状
        // 从第一天起就固定，M3 加上物种之后它才有值。
        inp[IN_ALIEN_AHEAD] = 0.0;

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
            _ => false,
        }
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
                if world.get(nx as isize, ny as isize) == MaterialKind::Remains {
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
        let cost = MOVE_COST * org.mass().max(1);
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
            if !world.get(x as isize, y as isize).is_fluid() {
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
                        MaterialKind::Water | MaterialKind::Gas
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
    fn can_place(world: &World, x: i32, y: i32) -> bool {
        let (xs, ys) = (x as isize, y as isize);
        if !world.in_bounds(xs, ys) || world.is_occupied(xs, ys) {
            return false;
        }
        matches!(
            world.get(xs, ys),
            MaterialKind::Empty | MaterialKind::Gas | MaterialKind::Water
        )
    }

    /// 吃掉某一格残骸，能量转给指定有机体。返回是否真的吃到了。
    ///
    /// 吃不下的时候**只吃一部分**，剩下的留在残骸里。
    /// 早先的写法是 `energy = (energy + gain).min(MAX_ENERGY)`，
    /// 超过上限的那部分凭空蒸发——账面上看不出来，直到某条鱼在满能量时
    /// 咬了一口尸体，守恒断言就差了 20 点。
    pub fn eat_remains(&mut self, world: &mut World, id: OrganismId, x: isize, y: isize) -> bool {
        if world.get(x, y) != MaterialKind::Remains {
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
            // 吃掉之后**变回水**，和尸体腐烂走同一条路。
            //
            // 那一格原本是身体格——身体出生时借走了这一格的水（见 `spawn`）。
            // 尸体被吃掉等于身体彻底消失，水就该还回来。
            // 写成 `Empty` 的话每吃掉一具尸体就凭空少一格水，
            // 而"鱼吃尸体"是常态，于是水会随生态活跃度单调减少。
            world.set(x, y, MaterialKind::Water);
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

/// 一条手工设计的"食腐鱼"的大脑。
///
/// 三件事：往前游、撞墙就掉头、看到什么就吃、攒够能量就生。
/// 它不会躲、不会追、不会转向猎物——**这就是 M2 想要的水平**：
/// 先证明"感知 → 决策 → 移动 → 进食 → 繁殖 → 死亡 → 残骸"这条链是通的，
/// 再谈网络能学到什么。一个会追猎物的基因组在没有猎物时和这个没有区别。
///
/// "撞墙掉头"不是装饰：只给"往右"的偏置时，所有鱼都会游到右边界堆成一团，
/// 于是互相拥挤、停止繁殖，整个种群卡在 9 条（实测）。一条能被墙推回来的鱼
/// 才会把水域用满——这是 M2 里第一次看到"行为影响生态"的地方。
pub fn handcrafted_fish_genome() -> Genome {
    let mut g = Genome::blank();
    g.weights[Genome::b_o(OUT_RIGHT)] = 1.0;
    g.weights[Genome::b_o(OUT_EAT)] = 2.0;
    g.weights[Genome::b_o(OUT_REPRODUCE)] = 1.5;
    // 撞到哪一侧的墙，就往另一侧走。**不能**写成"前方被挡就往左"——
    // 那样鱼会在墙边左右抖：被挡住→往左走一格→前方空出来→又想往右。
    // 分左右两路之后，它才会真的走完整个水池再折返。
    g.connect(IN_BLOCKED_RIGHT, OUT_LEFT, 10.0);
    g.connect(IN_BLOCKED_LEFT, OUT_RIGHT, 10.0);
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
) -> Vec<OrganismId> {
    let mut ids = Vec::new();
    if count == 0 || world.width() == 0 || PLANS.is_empty() {
        return ids;
    }
    let width = world.width();
    let step = (width / count).max(1);
    let base = handcrafted_fish_genome();

    for k in 0..count {
        let plan = &PLANS[k % PLANS.len()];
        // 身体在 x 方向的左右伸出量，用来把锚点夹在界内——
        // 否则贴边的个体整个投放会失败，水面看起来"有一段没鱼"。
        let reach_left = plan.cells.iter().map(|c| -c.1).max().unwrap_or(0) as i32;
        let reach_right = plan.cells.iter().map(|c| c.1).max().unwrap_or(0) as i32;
        let x = ((k * step + step / 2) as i32).clamp(reach_left, width as i32 - 1 - reach_right);

        let mut surface = None;
        for y in 0..world.height() as i32 {
            if world.get(x as isize, y as isize) == MaterialKind::Water {
                surface = Some(y);
                break;
            }
        }
        let Some(y) = surface else { continue };

        // 每个个体都轻微不同：给自然选择一点原材料。
        //
        // 全都一模一样的话，第一代没有任何可选的差异，种群只能靠"繁殖时突变"
        // 慢慢产生变异——那要等很多代。播种时给一点抖动，选择从第一 tick 就开始。
        // 幅度刻意压小（sigma 0.12）：大一点就会把手工设计的"会游、会吃"打散，
        // 鱼连基本行为都没有，选择也就无从谈起。
        let mut genome = base.clone();
        genome.mutate(
            &mut Rng::new(0xF15E_0000 ^ (k as u64).wrapping_mul(0x9E37_79B9)),
            0.35,
            0.12,
        );

        // 让最上面那一格贴着水面：生产者要光。
        let top = plan.cells.iter().map(|c| c.2).min().unwrap_or(0) as i32;
        let anchor = (x, y - top);
        let energy = energy.max(LifeSim::starter_energy(plan));
        let org = LifeSim::from_plan(plan, anchor, genome, energy);
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
                MaterialKind::Remains,
                "({x}, {y}) 应该是残骸"
            );
        }
    }

    #[test]
    fn energy_is_conserved_through_life_and_death() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
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
            w.count_of(MaterialKind::Remains),
            3,
            "尸体必须以残骸形式回到环境"
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
        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 25), Genome::blank(), 300))
            .unwrap();
        sim.organisms_mut()[0].age = MAX_AGE - 1;

        sim.step(&mut w);

        assert!(sim.organism(id).is_none(), "到寿数必须死");
        let audit = sim.audit(&w);
        assert!(audit.remains > 0, "尸体应该带着能量：{audit:?}");
        assert!(audit.is_balanced(), "差额 {}", audit.leak());
        assert_eq!(w.count_of(MaterialKind::Remains), 3);
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
        // 尸体被吃掉 = 那格身体彻底消失，出生时借走的水还回来。
        assert_eq!(w.get(22, 25), MaterialKind::Water);
        assert_eq!(sim.organism(id).unwrap().energy, 50 + REMAINS_ENERGY);
        let after = sim.audit(&w);
        assert_eq!(after.accounted(), before.accounted(), "进食只是搬运");
    }

    #[test]
    fn sunlight_feeds_producers_only_in_shallow_water() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
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
        let mut genome = handcrafted_fish_genome();
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
