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
use crate::field::AirField;
use crate::life::{
    remains_of, Cell, CellType, Genome, Organism, OrganismId, BIRTH_ENERGY_PER_CELL, INPUTS,
    IN_AGE, IN_ALIEN_AHEAD,
    IN_BLOCKED_AHEAD, IN_BLOCKED_LEFT, IN_BLOCKED_RIGHT, IN_BLOCKED_UP, IN_ENERGY, IN_FACING_X,
    IN_FACING_Y, IN_FOOD_AHEAD, IN_FOOD_DIR_X, IN_FOOD_DIR_Y, IN_KIN_AHEAD, IN_LIGHT,
    IN_ORGANISM_DIR_X, IN_ORGANISM_DIR_Y, IN_PREV_DOWN, IN_PREV_LEFT, IN_PREV_RIGHT, IN_PREV_UP,
    IN_CORPSE_DIR_X, IN_CORPSE_DIR_Y, IN_STUCK, IN_SUPPORTED_BELOW, OUTPUTS, OUT_DOWN, OUT_EAT,
    OUT_LEFT,
    OUT_REPRODUCE, OUT_RIGHT, OUT_UP, REMAINS_ENERGY,
};
use crate::material::MaterialKind;
use crate::moisture::Moisture;
use crate::organelle::{ARMOR, CHLORO_SHIFT, DIGEST, ROOT, STING};
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

/// 一次移动最多能顶开几格水。
///
/// ## 这个常量守的门已经不存在了（v0.26 记一笔）
///
/// 它原来是 `SMALL_BODY_MAX = 16`，用途是 `passable()` 里的
/// `Water => body <= SMALL_BODY_MAX`——**17 格以上的身体在水里，水是固体**。
///
/// 那条判据的理由是"1000 格的生物移动会推走 1000 格水，行为和性能都会崩"。
/// 但身体格数上限是 [`crate::morph::MAX_BODY_CELLS`] = 24，
/// 所以一次移动最多 swap 24 格水——**1000 格的情形根本不可能发生**。
/// 那个数字守的是一道已经关掉的闸门，而它自己变成了一道新的墙：
/// 大身体出生在水腔里，一格也走不动，然后掉到海底饿死。
///
/// 现在这个常量只剩**注释上的作用**：它把"一次移动最多顶开多少格水"这个
/// 上界写死在这里，好让 `try_move` 的 O(格数²) 有一个明确的天花板
/// （24² = 576 次整数比较，可以忽略）。**它不是通行判据，也不是玩法上限。**
/// 真正的玩法上限是 [`crate::morph::MAX_BODY_CELLS`]。
pub const MAX_DISPLACED: usize = crate::morph::MAX_BODY_CELLS as usize;

/// 自然寿命上限（tick）。到点就死，不商量。
pub const MAX_AGE: u32 = 20_000;

/// 突变率：每个权重被扰动的概率。
///
/// ## v0.34：5% → 2%
///
/// 370 个权重 × 5% = **每胎改 18.5 个**。在一个 370 维空间里，一次动 18 个
/// 坐标不是"爬坡"，是**随机游走**——子代和亲代的适应度几乎无关，
/// 选择也就没有东西可筛。这正是"进化看着很随机"的直接来源。
///
/// 2% 是每胎 7.4 个。再低（1%）会让一条谱系几十代才动一次，
/// 实测在 8000 tick（约 150 代）里就攒不出方向了。
///
/// **注意它和 [`crate::life::MORPH_OPS_PER_BIRTH`] 走的是相反方向**：
/// 形态基因组只有几十个基因，2 个算子太少、6 个刚好。两者的判据不同，
/// 不该共用一个数。
pub const MUTATION_RATE: f32 = 0.02;

/// `IN_LIGHT` 的衰减尺度：水面下多少格降到 0。
///
/// **这个数字现在是光场的物理参数，不再是感知的换算系数。**
/// 早先它只活在 `sense` 里（"头顶往上数多少格到水面"），
/// 有了真正的光场之后，它变成 [`crate::field::SUN_ATTEN`] 的别名——
/// 同一个数只能有一个来源，否则"光场里 180 格全黑、而鱼觉得自己还在 0.4 的光里"
/// 这种不一致会一直存在，且没有任何测试会发现它。
pub const LIGHT_DEPTH: i32 = crate::field::SUN_ATTEN as i32;


/// 手工基因组瞄准的**等光线**：`IN_LIGHT` 的设定点。
///
/// 这个数字**不是"目标深度"，是"竞争平衡点"**——两者差得很远，因为
/// `pick_direction` 取的是"超过阈值的最大值"，而巡航偏置恒在。
///
/// 真正的平衡点是 `设定点 + 巡航偏置/增益`：
///
/// ```text
/// tanh(gain × (light − setpoint)) = tanh(cruise_bias)
///   ⇒ light = setpoint + tanh(bias) / gain
///          = 0.60 + 0.60 / 12 ≈ 0.65
///   ⇒ 深度 = (1 − 0.65) × LIGHT_DEPTH ≈ 63 格
/// ```
///
/// 上浮那一侧对称：`0.60 − 0.05 ≈ 0.55` → 81 格。
/// 所以鱼的活动带是**水面下 63 到 81 格**——一条 18 格高的走廊，
/// 它们会在这条走廊的两端之间**来回**：到 77 就上浮，到 67 就下潜。
///
/// 为什么是水面下 63 格而不是贴着水面：贴着水面时鱼有一半身体在空气里，
/// 视觉上像"浮着"；而且水面是光最多的地方，待在那里等于放弃
/// "在二维里游"这件事。往下 67 格的光是 0.63，生产者收益少一成，
/// 但鱼真的在水里了。
///
/// 为什么平衡点要贴近水面而不是"透光层中段"：生产者收益与光成正比，
/// 而水面处的光是 0.70、水面下 90 格只有 0.52。把平衡点设在深处，
/// 种群会掉到四分之一（实测 60 → 11）。**鱼待在水面附近是对的**，
/// 垂直移动留给觅食和躲避，不靠趋光硬赶。
///
/// 早先写 0.66 时平衡点算出来是 0.74（水面下 25 格），而水面本身的光就是
/// 0.70——**平衡点比水面还浅**，鱼一辈子没下潜过，看起来就是"只沿 X 轴游"。
///
/// 写成一个**设定点**而不是"趋光/避光"，是因为带设定点的行为有稳定点：
/// 光太亮会往下、太暗会往上，中间不产生垂直信号。而"越亮越想上"会让鱼
/// 贴着水面那一行抖——上去一格变亮、又想上去，最后卡死在那儿。
pub const LIGHT_SETPOINT: f32 = 0.90;

/// 趋光增益：光每偏离设定点 0.1，垂直方向就想移动多少。
///
/// `pick_direction` 取"超过阈值的**最大值**"，而巡航偏置恒为
/// `tanh(1.0) ≈ 0.76`。垂直信号压不过它，就等于没接——这正是
/// "只沿 X 轴移动"的根因。
///
/// 12.0 是被测出来的，不是拍的：要让水面（光 0.70）处的下沉信号压过 0.76，
/// 需要 `tanh(gain × 0.10) > 0.76`，即 `gain > 10`。第一版给 5.0，
/// 水面处的信号只有 0.46——鱼照样一路浮到水面之上，然后被水陆判据拦住，
/// 于是"全都在半空里漂着"。
pub const PHOTO_GAIN: f32 = 12.0;

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

// 这里原本还有四个常数：`SUN_SHADE_DEPTH` / `SUN_SHADE_RADIUS` /
// `SHADE_SATURATION` / `MAX_SHADE`，它们实现的是"上方的生物挡住下方的光"。
//
// 那是**光场还不存在时的替代品**：既然没有真正的光，就用"附近有多少格身体"
// 硬凑一个遮挡系数。有了 `field::AirField` 之后它必须被删掉，理由有三条：
//
// 1. 它和真实光场算的是同一件事，两份规则迟早不一致；
// 2. 用户明确要求"生物本身先不遮挡"——遮挡只在材质层算；
// 3. 它是个**隐形的全局负反馈**：调它的时候你不知道自己是在改物理还是在改平衡。
//
// 种群限制现在来自几何：生产者必须待在透光层，而透光层的格子是有限的。

/// 一个粗格里至少有多少比例是水，生产者才算"泡在水里"。
///
/// 0.35 而不是 1.0：一条三格的身体就占掉粗格 16 格里的 3 格，
/// 剩下 13 格是水（0.81）。要求满格水等于要求"身体不在这个粗格里"，
/// 那这条规则就永远不会触发。而 0.35 让"半截露出水面的身体"仍然算湿——
/// 贴着水面游的鱼不该因为头顶出水就断粮。
pub const SUBMERGED_MIN: f32 = 0.35;

/// 根能提供的湿度**起点**：一格根扎在水边时，整株按这个比例产能。
///
/// 0.20 ≈ 水里的五分之一。比 v0.37 那个 0.35 低，因为那时候它是**上限**
/// （一个常数），而现在它是**斜坡的起点**——一株植物长多少根，它就能多接近
/// [`LAND_DAMP_MAX`]。这是 v0.39 的全部改动：把"有根 / 没根"的开关
/// 换成"根系多厚"的连续量。
pub const LAND_DAMP_BASE: f32 = 0.20;

/// 每多一格根，岸上的产能提高多少。
///
/// 0.08：一格根 0.28、三格 0.44、五格封顶 0.60。
pub const LAND_DAMP_PER_ROOT: f32 = 0.08;

/// 根系能买到的产能上限。
///
/// 0.60：岸上光更强（没有水的衰减），`0.6 × 强光` 大致等于水里的满产。
/// **不再往上抬**——否则第一株长出厚根系的植物会铺满陆地，那是 v0.23
/// 海草 481 株吃光所有光的重演。陆地该是"能活，但更苦"。
pub const LAND_DAMP_MAX: f32 = 0.60;

// 湿度衰减到零的距离搬到了 `crate::moisture::SPAN`——它和"距离怎么算"
// 是同一件事，而距离怎么算现在住在那个模块里。

/// 土壤湿度每多少点回充 1 点含水量。
///
/// 40：湿度 0..255 分成 0..6 共 7 档回充速率，而 `HYDRATION_DRAIN` 是 1。
/// 所以**净收支为零的那一档是 40..79**——低于 40 的土壤，植物入不敷出。
/// 这条分档很粗（只有 7 档），但它换来的是整数运算：`hydration` 是 `u8`，
/// 用小数累积就得给它加一个进位字段，而那个字段要进存档。
const SOIL_REFILL_PER: u8 = 40;

/// 一株生产者这一 tick 收进来多少能量。
///
/// **整具身体一次截断，不要逐格截断再乘格数。**
///
/// 旧写法是 `(...) as u32 * producers`——先把单格产量截成整数再乘格数。
/// 单格 `24 × 0.77 × 0.8 × 0.32 = 4.72` 截成 4，一条 6 格的身体每 tick
/// 白丢 4.3 点（约 15%）。在"岸上产能只有水里三分之一"的地方，
/// 这 15% 是**生死差别**：实测同一株植物，逐格截断时离岸 40 列饿死，
/// 整具截断时活到 50 列。
///
/// 抽成函数是为了能被单元测试直接钉住——这条规则藏在 `step` 中间时，
/// 唯一的观测方式是跑生态，而生态里它和十几个别的因素混在一起。
fn producer_gain(light: f32, warmth: f32, damp: f32, wet: f32, producers: usize) -> u32 {
    (PRODUCER_YIELD as f32 * light * warmth * damp * wet * producers as f32) as u32
}

/// 一株植物的根能把它在岸上的产能抬到多少。
///
/// ## 为什么是函数而不是常数
///
/// v0.37 里 `LAND_DAMP` 是个常数：只要有**一格**根贴着水，整株就按 0.35 产。
/// 于是"多长一格根"没有任何收益——根只是一个开关，突变不会被推向它。
/// 实测 8000 tick 里**一株带根的植物都没出现**：机制在，但没有梯度。
///
/// 换成斜坡之后 `damp` 成了根格数的函数，**投资根系第一次有回报**。
/// 而根不是免费的（每格 1/tick 维持 + 占质量），所以最优格数由
/// "岸上光有多强"决定，不是无限长——**这是这条曲线唯一的依据。**
fn land_damp(roots: usize) -> f32 {
    (LAND_DAMP_BASE + LAND_DAMP_PER_ROOT * roots as f32).min(LAND_DAMP_MAX)
}

/// 完全上岸时，每一步移动贵几倍。
///
/// 3 倍：一条 6 格、速度 1 的鱼在水里每步 6 点，上岸后 24 点，而它的维持
/// 成本大约 20/tick——**在岸上走一步等于多活一 tick 的钱**。这个量级刚好让
/// "短距离横穿一条浅滩"可行，"住在岸上"不可行。
///
/// 为什么是"加价"而不是"禁止"：禁止会让上岸变成一件不可能的事，而
/// 陆地演化需要的是"很贵但可行"。贵到弱个体立刻饿死、强个体能撑住，
/// 才是"渐进"。
pub const LAND_COST: f32 = 3.0;

/// 陆地上每格质量每 tick 的支撑成本。
///
/// 0.5：一条 6 格的身体完全上岸要多付 3/tick，20 格要多付 10/tick。
/// 和 [`LAND_COST`] 分工不同——那是"走一步多贵"，这是"站着不动也要付"。
/// 只有前者的话，一条上岸的鱼可以一动不动地待着，于是搁浅会变成一种
/// 可行的休眠策略。
pub const LAND_UPKEEP: f32 = 0.5;

/// 低于这个水占比，叶绿体完全不工作。
///
/// 不设成 0：真正的叶绿体在干燥环境里会关闭气孔停止工作，而保留一点门槛
/// 也让"干透的石头"和"潮间带"在能量上分得开。
pub const DRY_MIN: f32 = 0.05;

/// 水占比 → 光合作用的产能折扣（0..=1）。
///
/// **这条斜坡是"陆地渐进"在能量侧的全部实现。**
///
/// 早先是 `water_frac < 0.35` 直接 `continue`，也就是"要么满产、要么零产"。
/// 那条悬崖正好落在海岸线上，于是"半上岸"在能量上根本不存在——陆地演化
/// 只能是一刀切。现在 0.05 以下是干的，0.05~0.35 线性爬升，于是潮间带
/// 有了 0~100% 之间的连续产能。
///
/// 抽成函数而不是内联，是因为它是**唯一一个可以单独测的判据**：内联在
/// `inject_sunlight` 里的话，要验证"水占比 0.2 时产量是一半"就得搭一个
/// 恰好 3/16 是水的粗格，还要控制光照和温度。
pub fn dampness(water_frac: f32) -> f32 {
    if water_frac <= DRY_MIN {
        return 0.0;
    }
    ((water_frac - DRY_MIN) / (SUBMERGED_MIN - DRY_MIN)).min(1.0)
}

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

/// 浮游藻撒在水面以下多少格之内。
///
/// 用"离水面多深"而不是"光照阈值"：光照是 `1 - y/世界高`，它把**世界尺寸**
/// 也编进去了——同一个阈值在 512² 和 128² 里对应完全不同的深度。
/// 而这里要表达的是"藻悬在透光层"，那是一个深度问题。
///
/// 24 格。**这个数字决定的是"密度"，不是"深度"。**
///
/// 藻 300 个封顶，撒在 60 格深里密度只有 0.9%——鱼随机游动撞上它的
/// 概率太低，而藻又占格（鱼不能穿进去），于是鱼在藻群里饿死。
/// 24 格把密度提到 2.4%，而且正好和鱼的活动带（水面下 9~27 格）重叠。
pub const PLANKTON_DEPTH: i32 = 24;

/// 浮游藻的存量上限（个）。
///
/// 藻是**生物**，理论上可以靠分裂自己涨到光照能支撑的极限。但那样会有
/// 两个问题：一是几千个单格生物会把每 tick 的遍历拖慢十倍；二是
/// "藻把整片海填满"在画面上不好看。所以给一个硬上限。
///
/// 600 个约占 512² 的 0.23%——视觉上是稀疏的绿点，而不是一层膜。
pub const PLANKTON_CAP: usize = 150;

/// 每次清扫撒几个藻。
///
/// 这是"光 → 藻 → 鱼"这条通路的**播种通量**。注意它**不是**藻的主要
/// 增长方式——藻靠分裂繁殖，播种只是让"被吃光的海域"能重新长起来。
///
/// 4 个 / 25 tick = 0.16 个/tick。对照：一条鱼一 tick 能吃一格。
/// 所以播种远远跟不上捕食，藻群主要还是靠自己的分裂维持。
pub const PLANKTON_PER_SWEEP: usize = 4;

/// 为撒一个藻，最多试几个位置。
///
/// 位置是随机撒的（透光层里空气占了大半），所以大部分尝试会落在不能长的地方。
const PLANKTON_TRIES: usize = 40;

/// 藻播种用的随机种子盐。
const SALT_PLANKTON_SPAWN: u64 = 0xA16A_E5EE_D000_0000;

/// 新藻出生时带多少能量。
///
/// 不能给 `BIRTH_ENERGY_PER_CELL`（67）：藻的繁殖门槛是 134，
/// 给 67 等于"一出生就攒了一半"，会立刻分裂。给 20 让它得自己晒几 tick 光。
const PLANKTON_START_ENERGY: u32 = 20;

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
            MaterialKind::Water => return true,
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

// 这里原本还有一个 `CARRYING_CAPACITY`：全世界生产者细胞数的软上限，
// 用来给光照乘一个 `1 / (1 + P / CAP)` 的竞争系数。
//
// 它被删掉了，因为它是**一道和物理无关的人造闸门**：那个 2400 是怎么定的？
// 是"这样种群会停在 240 条左右"。也就是说，它把"世界能养活多少生物"
// 直接写成了一个常数——而这恰恰是该由世界自己算出来的东西。
//
// 现在这个数由光场回答：生产者必须待在透光层，透光层的格子有限，
// 于是种群停在"把透光层占满"的位置。想改平衡，改地貌或者改光，
// 而不是改一个数字。


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
    /// 当前有多少个浮游藻（单细胞生产者）。
    ///
    /// 和 `organisms` 分开：藻是**几百个**，混进去之后"生物 640"这个数字
    /// 就不再是"生态里有几个动物"，而体型统计（min/max）也会被 1 格撑到没意义。
    pub plankton: usize,
    /// 本 tick 有多少个体**因为重力下落**了一格。
    ///
    /// 和 `moved` 分开：混在一起会把"垂直移动占比"污染成
    /// "有多少东西正在往下掉"，而那是两件完全不同的事。
    /// 正常状态下它应当接近 0——只有被放在半空、或者被水挤出水面的
    /// 个体才会掉。
    pub fell: u32,
    /// 本 tick 四个方向的移动次数：`[左, 右, 上, 下]`。
    ///
    /// 这个数组是"二维移动到底有没有发生"的唯一直接证据。
    /// 看深度范围是不够的——一群鱼全在水面那一层上下游三格，深度范围
    /// 也有 3，但那和"在二维里游"是两件事。
    ///
    /// 健康的比例大致是"水平明显多于垂直，但垂直不为零"：
    /// 水平是巡航，垂直是觅食与趋光。**垂直恒为 0 就是有 bug**。
    pub moved: [u32; 4],
    /// 本 tick 有多少次移动是**斜向**的（两个轴同时动）。
    ///
    /// 单独一个计数，因为 `moved` 只记"主导轴"：`(1, 1)` 会被记成"右"，
    /// 于是"斜向到底有没有发生"从 `moved` 里完全看不出来。
    ///
    /// 这条是 M2a 那条"顺坡移动"的**唯一直接证据**——如果它恒为 0，
    /// 说明斜向从来没被选中过，而"鱼能上岸"就只是个说法。
    pub diagonal: u32,
    /// 最浅的那个个体的锚点 y（越小越靠近水面）。空种群时为 0。
    ///
    /// 和 `deepest` 一起看，才知道"鱼是在水里散开的，还是全挤在水面一层"。
    /// 只报平均值是不够的：一半浮在水面、一半沉在海底，平均值看起来
    /// 完全正常。
    pub shallowest: i32,
    /// 最深的那个个体的锚点 y。
    pub deepest: i32,
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
    /// 本 tick 被吃掉的**尸体**格数（[`LifeStats::eaten`] 的子集）。
    ///
    /// ## 为什么单独一个数
    ///
    /// `eaten` 把碎屑、藻、尸体混在一起数，而三者的生态含义完全不同：
    /// 碎屑是"世界生成时撒的底货"，藻是"现长的"，只有**尸体**才是
    /// "食腐"这个生态位的证据。
    ///
    /// v0.35 把尸体的平均能量从 5 点提到 38 点，让食腐在能量上成立了；
    /// v0.39 又给了它一条独立的嗅觉通道。但**"成立"不等于"被占据"**——
    /// 这个数就是那条验收的唯一直接证据：它长期为 0，说明没有任何谱系
    /// 在吃尸体，不管机制多完整。
    ///
    /// 不能拿"尸体存量下降"代替它：存量同时受死亡率、腐烂速度、
    /// 材质模拟的影响，掉下来分不清是谁干的。
    pub scavenged: u32,
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
    /// **一格水都够不到**的个体数（"飘在天上"或"搁在干地上"）。
    ///
    /// 判据是 [`LifeSim::submerged_frac`] `== 0`：身体的每一列头顶向上
    /// 都扫不到水。不能看格子本身的材质——身体格的材质永远是 `Empty`，
    /// 那是材质层与生命层的互斥约定。
    ///
    /// **v0.31 换了判据。** 早先是 `!touches_water`，也就是"任何一列的
    /// 头顶出水"就算。那条判据在"移动是开关"的时代是准的（半露出水面
    /// 的身体根本动不了），但 v0.31 把移动换成了按水占比定价之后，
    /// **贴着水面游成了正常且常见的事**，于是这个数字会长期停在种群的
    /// 一半以上——它作为"有东西飘在天上"的警报就废了。
    ///
    /// 现在它只在"完全脱水"时涨。那些个体既不能动（或者极贵）也没有光合，
    /// 只会坐在原地把能量耗光，从"生物数"上看完全正常。
    ///
    /// 这个指标是被一次真实的崩坏逼出来的：没有它的时候，鱼靠繁殖
    /// 一代代往上爬进天空，种群从 145 掉到 11，而能量账一分不差、画面上
    /// 也看不出异常。
    pub stranded: usize,
    /// 当前有几个**物种**（按体型判同，见 [`Organism::body_fingerprint`]）。
    ///
    /// 界面"世界"窗口顶上那行要用它。刻意不按基因组判同：
    /// 那样报出来的是"有几种不同的神经权重"，而玩家想看的是"有几种长法"。
    pub species: usize,
}

/// 一次位移是**主动游动**还是**被重力拉下来**。
///
/// 两者的差别不只是"扣不扣能量"，还有一条更关键的：**水陆判据只拦前者**。
///
/// 没有这个区分时，玩家把鱼放在半空，它会**原地冻结**——因为
/// `touches_water` 要求身体在水里，而任何一步移动都在空气里，于是
/// 每个方向都被拒绝。表现是"放下去之后它一动不动，像被钉住了"，
/// 而那不是物理，是判据用错了地方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveKind {
    /// 主动游动：要花能量，且必须留在水里。
    Swim,
    /// 被重力拉着下落：不花能量，不受水陆判据约束。
    Fall,
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
    /// 四个方向的累计移动次数 `[左, 右, 上, 下]`。
    ///
    /// 和 `stats.moved` 的区别是**它不清零**：`stats` 每 tick 重建，
    /// 所以单 tick 的样本小到看不出比例（六十个生物里只有几次移动是常态，
    /// 大部分 tick 在转身、被挡、或者刚出生）。累计值才能回答
    /// "垂直方向到底有没有在用"。
    moved_total: [u64; 4],
    /// 累计下落次数。
    fell_total: u64,
    /// 当前所有藻的 id，**升序**。
    ///
    /// 每 tick 重建一次。用途只有一个：`sense` 里要判断"射线撞到的那个
    /// 生物是不是藻"，而那里每 tick 每生物要问 80 次（4 方向 × 20 格）——
    /// 每次去 `organisms` 里线性查一遍是 O(n²)，几百个藻时直接卡死。
    plankton_ids: Vec<OrganismId>,
    /// 被吃掉、等着从 `organisms` 里移除的藻。
    ///
    /// **不能当场 `remove`**：`act` 是按索引遍历的（`for i in 0..n`），
    /// 中途删元素会让后面的索引全部错位——某条鱼被跳过、另一条被处理两次。
    /// 那种 bug 在画面上看不出来，只会让"为什么这条鱼不动"变成谜。
    eaten_plankton: Vec<OrganismId>,
    /// 粗粒度的光场与温度场。
    ///
    /// 住在生命层，而不是外面那个主循环里：生产者产能**必须**读它，
    /// 而渲染器和界面只需要只读的一份（[`LifeSim::field`]）。
    /// 如果它住在主循环，`step` 就要多一个参数——而那个签名会被
    /// 几百个测试调用，改动成本远大于收益。
    field: AirField,
    /// 土壤湿度场（`0..=255`，**逐格**）。
    ///
    /// 它回答"这一格有多湿"——根从这里抽水，叶绿体按它打折。规则只有
    /// 一条：**到最近水格的距离**。细节见 [`crate::moisture`]。
    ///
    /// ## 为什么现在存进存档了
    ///
    /// v0.41 之前它是"每列一个数"，而那个数纯粹是世界布局的函数，
    /// 读档时重算一遍就行。换成二维之后它仍然是个纯函数，**但它会被读到旧值**：
    /// 水会流动（材质模拟），而重算只发生在开新世界 / 读档 / 画笔落笔这三处。
    /// 于是"存档那一刻的湿度"和"读档时重算出来的湿度"可能差一点点，
    /// 而两条鱼会因为这个差别游向不同的方向。
    ///
    /// 它是**积分状态**，和温度同一类：算过一次就留在那儿，直到下次重算。
    /// 那就得存。
    moisture: Moisture,
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
            moved_total: [0; 4],
            fell_total: 0,
            plankton_ids: Vec::new(),
            eaten_plankton: Vec::new(),
            moisture: Moisture::new(),
            // 空场：0×0，所有查询都返回默认值。`new` 会按真实尺寸重建。
            field: AirField::default(),
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
        let mut field = AirField::new(world);
        // 立刻算一遍光场：不然第一个 tick 之前整个世界都是黑的，
        // 而"开局那一瞬间的截图"恰恰是最常被拿来看的。
        field.refresh(world);
        Self {
            organisms: Vec::new(),
            next_id: 1,
            ledger,
            rng: Rng::new(LIFE_SEED),
            founder: None,
            stats: LifeStats::default(),
            food_growth: true,
            moved_total: [0; 4],
            fell_total: 0,
            plankton_ids: Vec::new(),
            eaten_plankton: Vec::new(),
            moisture: Moisture::new(),
            field,
        }
        .with_moisture(world)
    }

    /// 立刻按世界重算湿度场。
    ///
    /// **玩家用画笔改了材质之后必须调这个。** 否则"在内陆挖一个湖"
    /// 只会多出几格水，而根吸不到它、湿度视图也不变——看起来就是坏了。
    /// 主循环在落笔那一帧调一次。
    pub fn refresh_moisture(&mut self, world: &World) {
        self.moisture.rebuild(world);
    }

    /// 湿度场（只读）。渲染器画"湿度"滤镜、界面显示读数都从这里取。
    #[inline]
    pub fn moisture(&self) -> &Moisture {
        &self.moisture
    }

    /// 这一格有多湿（`0..=255`）。横向环绕，纵向越界返回 0。
    #[inline]
    pub fn moisture_at(&self, x: i32, y: i32) -> u8 {
        self.moisture.at(x, y)
    }

    /// 光场与温度场（只读）。渲染器和界面从这里取。
    #[inline]
    pub fn field(&self) -> &AirField {
        &self.field
    }

    /// 光场与温度场（可变）。放灯、删灯、以及"暂停时也要刷新"走这里。
    #[inline]
    pub fn field_mut(&mut self) -> &mut AirField {
        &mut self.field
    }

    /// 立刻重算光场和温度场。
    ///
    /// 用在"时钟不走但世界变了"的时刻：暂停时放了一盏灯、或者用橡皮
    /// 在海底挖开一个洞。按 `% INTERVAL` 的判据在这种情况下永远不会触发，
    /// 而用户会盯着一个没反应的世界。
    pub fn refresh_field(&mut self, world: &World) {
        self.field.refresh(world);
    }

    /// 按世界重算湿度场。
    ///
    /// 真正的算法在 [`crate::moisture`]：**到最近水格的二维距离**。
    /// 这里只负责把它挂到 `LifeSim` 的生命周期上（开新世界时调一次）。
    fn with_moisture(mut self, world: &World) -> Self {
        self.moisture.rebuild(world);
        self
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

    /// 下一个要分配的编号。
    ///
    /// 存档要存它：读档后新生的个体会拿到**和存档那一刻接续**的编号，
    /// 而不是从 1 重新数。编号同时进 [`World::hash`]（占格表里存着它），
    /// 所以"读档后再跑 200 tick 的哈希 == 没存过档跑 200 tick 的哈希"
    /// 这条断言，靠的就是这个值也被原样带回来。
    #[inline]
    pub fn next_id(&self) -> OrganismId {
        self.next_id
    }

    /// 随机数发生器的内部状态，见 [`Rng::state`]。
    #[inline]
    pub fn rng_state(&self) -> (u64, u64) {
        self.rng.state()
    }

    /// 藻席生长是否开着。
    #[inline]
    pub fn food_growth(&self) -> bool {
        self.food_growth
    }

    /// 读档：把一份完整状态装回一个新调度器。
    ///
    /// 和 [`LifeSim::new`] 的区别只有一处，但很关键：`new` 会**扫一遍世界**
    /// 把已有的残骸算成"注入"，而这里账本是从存档里原样带回来的。
    /// 走 `new` 再覆盖账本会做一遍白工，走 `new` 不覆盖则会凭空多出
    /// "残骸标称值之和"这笔注入——读一次档账本就崩一次，而画面毫无异样。
    ///
    /// 装完立刻重建占格表（[`Organism::claim`]）并重算光场：
    /// 渲染器和拾取都靠占格表，它们不该知道"刚才是读档"。
    pub fn restore(world: &mut World, save: crate::save::LifeSave) -> Self {
        let mut field = AirField::new(world);
        // 长度不符的温标会被 `restore` 整个拒掉，退回环境温度——
        // 一个坏档不值得让整个读档失败，但也不该装进去一半。
        field.restore(world, &save.sources, &save.temp);

        let mut sim = Self {
            organisms: Vec::new(),
            next_id: save.next_id.max(1),
            ledger: save.ledger,
            rng: Rng::new(LIFE_SEED),
            founder: save.founder,
            stats: LifeStats::default(),
            food_growth: save.food_growth,
            moved_total: [0; 4],
            fell_total: 0,
            plankton_ids: Vec::new(),
            eaten_plankton: Vec::new(),
            moisture: Moisture::new(),
            field,
        };
        // 湿度是**世界布局的纯函数**（到最近水格的距离），而且它每 tick
        // 都跟着 `step` 重算——所以读档时算一遍就够了，存档里一个字都不用加。
        // 存档那一刻世界是什么样，读回来就是什么样，重算的结果必然一致。
        sim.moisture.rebuild(world);
        sim.rng.set_state(save.rng_state.0, save.rng_state.1);

        for org in save.organisms {
            // 编号比 `next_id` 还大的有机体只可能来自一个被手工改过的档。
            // 与其让它和未来的新生儿撞号（撞号之后占格表会指向错误的身体，
            // 表现是"某条鱼动了另一条鱼"），不如当场把计数器顶上去。
            sim.next_id = sim.next_id.max(org.id + 1);
            org.claim(world);
            sim.organisms.push(org);
        }
        // **光场要在占格表填好之后**再刷一次。
        //
        // `AirField::restore` 在装完温度之后已经刷过一次，但那时有机体还没
        // `claim`——而 v0.33 起光场会按身体覆盖度打折（`BODY_SHADE`）。
        // 不补这一下，读档后的第一 tick 光和存档时差一截，两边立刻分叉，
        // 而症状是"读档之后世界跑出了另一个结果"，看上去像随机数没存对。
        // 只刷光，**不刷温度**：温度是积分状态，存档里存着，重算一遍就把
        // 存档里的那份覆盖掉了（症状是"温度场没原样回来"，差值还有几度）。
        sim.field.update_light(world);
        sim
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
        for (cell, &(x, y)) in org.cells.iter_mut().zip(&spots) {
            let kind = world.get(x as isize, y as isize);
            // 只有水才算"借"。气体、火被挤掉不算债务——它们本来就不是水。
            cell.wet = matches!(kind, MaterialKind::Water);
            if kind.is_fluid() {
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

    /// 四个方向的**累计**移动次数 `[左, 右, 上, 下]`。
    ///
    /// 这是"二维移动真的在发生吗"的直接证据。垂直两项长期为 0
    /// 就说明行为网络里根本没有垂直通路在起作用。
    #[inline]
    pub fn moved_total(&self) -> [u64; 4] {
        self.moved_total
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
            if matches!(cell.kind, MaterialKind::Remains | MaterialKind::Corpse) {
                remains += cell.flags as u64;
            }
        }
        EnergyAudit {
            organisms: self
                .organisms
                .iter()
                .map(|o| o.energy as u64 + o.structure as u64)
                .sum(),
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

        // 1) 含水量。放在太阳能**之前**：这一 tick 的光合用这一 tick 的水。
        self.tick_hydration(world);

        // 1b) 太阳能。生产者是唯一的"环境 → 生物"入口（SPEC §3.3）。
        self.inject_sunlight(world);

        // 1b) 尸体腐烂 + 藻的播种，见 `tick_organics`。
        self.tick_organics(world);

        // 1b2) 藻的分裂。放在产能之后：刚晒到的能量这一 tick 就能用来分裂。
        self.tick_plankton(world);

        // 1c) 重力：不在水里的先掉下来。
        //
        // 放在行为**之前**：一条刚被放进空气的鱼，这一 tick 先被拉下来一格，
        // 落水之后同一 tick 就能正常游。反过来（先行为）它会被水陆判据
        // 拦住一整轮，白等一 tick。
        self.apply_gravity(world);

        // 1d) 重建藻的索引。`act` 里的嗅觉要用它判断"闻到的是不是藻"。
        self.rebuild_plankton_ids();

        // 2) 行为。
        self.act(world);

        // 2a) 清理被吃掉的藻。必须在 `act` 之后、`predate` 之前：
        //     放在 `act` 里面会让索引错位，放在代谢之后会让它先饿死、
        //     变成一具本不该存在的尸体。
        self.clear_eaten_plankton(world);

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
            // ---- 陆地的支撑成本 ----
            //
            // 水里不用撑体重（浮力就是干这个的），陆地上要。按**质量 ×
            // 露出水面的比例**算，于是"大身体更难上岸"自动成立——不需要
            // 写任何"如果身体大就……"的规则。
            //
            // 这一项和 [`LAND_COST`] 是两件事：那是"走一步多贵"，这是
            // "站着不动也要付"。只有前者的话，一条上岸的鱼可以一动不动地
            // 待着，搁浅于是变成一种可行的休眠策略。
            //
            // 完全在水里时 `dry == 0`，这一项一分钱都不加，所以水里的
            // 能量账和 v0.30 逐 tick 相同——生态不该因为这条改动漂移。
            let fp: Vec<(i32, i32)> = org.footprint().collect();
            let dry = 1.0 - Self::submerged_frac(&*world, org.width, &fp);
            let land = (org.mass() as f32 * LAND_UPKEEP * dry) as u32;
            let upkeep = org.upkeep() + land;
            if org.age >= MAX_AGE || org.energy < upkeep {
                // 老死与饿死走同一条埋葬路径。这里**不**动 energy：
                // 剩下的那点（哪怕不够付一次维持成本）会随尸体进入残骸，
                // 由 `bury` 统一记账。在这里先扣一遍会让它被算两次。
                starved.push(i);
            } else {
                org.energy -= upkeep;
                dissipated += upkeep as u64;
                // ---- L2 表观遗传：压力累积 ----
                //
                // "饿"的判据不是"付不起"，而是"付完之后剩不下多少"——
                // 一个刚好够付的个体下一 tick 就要死，它这一 tick 就该紧张。
                //
                // 为什么要记这个数：它是"环境变差时种群自己加快变异"的载体
                // （见 `reproduce`）。真实生物里这叫 stress-induced mutagenesis，
                // 而它**不改变"遗传的是基因"这个前提**——`stress` 不进基因，
                // 它只调子代的变异率。
                if org.energy < upkeep * 4 {
                    org.stress = org.stress.saturating_add(1);
                } else {
                    // 缓过来就慢慢松下来。衰减比累积慢，所以一次饥荒的
                    // 影响会持续好几代——这正是"母体效应"该有的样子。
                    org.stress = org.stress.saturating_sub(1);
                }
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
        // **动物和藻分开统计。**
        //
        // 藻是几百个单格生物，混进 `organisms` 之后"生物 640"就不再是
        // "生态里有几个动物"；而体型统计（min/max）会被 1 格撑到没意义——
        // 一条 14 格的叶背鱼和一片藻放在同一个 `min` 里，那个数字永远报 1。
        let mut plankton = 0usize;
        let mut animals = 0usize;
        let mut cells = 0usize;
        let (mut shallow, mut deep) = (i32::MAX, i32::MIN);
        let (mut min_cells, mut max_cells) = (usize::MAX, 0usize);
        let mut stranded = 0usize;
        let mut killers = 0usize;
        let mut energy = 0u64;
        let mut max_gen = 0u32;
        // 物种：按体型指纹去重。**藻也算**——一片藻席里如果长出了两种
        // 不同大小的单细胞，那就是真的有两个物种，不该被"它们都是藻"抹平。
        let mut species: std::collections::HashSet<u64> = std::collections::HashSet::new();
        for org in &self.organisms {
            cells += org.cell_count();
            // 统计口径把生物量算进去：对玩家来说"它有多少本钱"是两者之和。
            energy += (org.energy + org.structure) as u64;
            max_gen = max_gen.max(org.generation);
            species.insert(org.body_fingerprint());
            if org.is_plankton() {
                plankton += 1;
                continue;
            }
            animals += 1;
            shallow = shallow.min(org.anchor.1);
            deep = deep.max(org.anchor.1);
            let n = org.cell_count();
            min_cells = min_cells.min(n);
            max_cells = max_cells.max(n);
            killers += org
                .cells
                .iter()
                .filter(|c| c.organelles.has(STING))
                .count();
            let fp: Vec<(i32, i32)> = org.footprint().collect();
            // **完全脱水才算搁浅。**
            //
            // 早先是 `!touches_water`，也就是"任何一列的头顶出水"就算。
            // v0.31 之后贴着水面游（头露出水面、身体在水里）是一件正常
            // 且常见的事，用那条判据会让这个数字长期停在种群的一半以上，
            // 而它作为"有东西飘在天上"的警报就废了。
            if Self::submerged_frac(world, org.width, &fp) <= 0.0 {
                stranded += 1;
            }
        }
        self.stats.plankton = plankton;
        self.stats.organisms = animals;
        self.stats.cells = cells;
        self.stats.shallowest = if animals == 0 { 0 } else { shallow };
        self.stats.deepest = if animals == 0 { 0 } else { deep };
        self.stats.min_cells = if animals == 0 { 0 } else { min_cells };
        self.stats.max_cells = max_cells;
        self.stats.stranded = stranded;
        self.stats.killers = killers;
        self.stats.energy = energy;
        self.stats.max_generation = max_gen;
        self.stats.species = species.len();
        // 平均代数只算**动物**：藻每分裂一次代数就 +1，几百个藻会把
        // "平均代数"拉到和鱼完全无关的量级上。
        let (sum, n) = self
            .organisms
            .iter()
            .filter(|o| !o.is_plankton())
            .fold((0f32, 0usize), |(s, n), o| (s + o.generation as f32, n + 1));
        self.stats.avg_generation = if n == 0 { 0.0 } else { sum / n as f32 };
        self.stats.gene_distance = self.gene_distance();

        // 6) 湿度。放在**最后**，因为它必须反映"这一 tick 结束时的世界"。
        //
        // 代价很小（粗格 128×128，两遍扫描几十微秒），而它买到的东西
        // 很大：**湿度永远等于当前世界的函数**。于是"水流动了但湿度停在
        // 旧值"这一类问题根本不存在，存档也不用存它（读档时算一遍就是
        // 同一个结果）。
        //
        // 放在最后而不是开头，是因为世界在 tick 里会变（尸体落地、藻被吃掉），
        // 而"tick 结束时的世界"才是下一次存档会写下去的那一个。
        self.moisture.rebuild(world);
    }

    /// 玩家刚画上去的有机物**要带能量**。
    ///
    /// ## 为什么需要这一步
    ///
    /// `world.set` 会把 `flags` 清零，而 `flags` 正是残骸/尸体的能量
    /// （`eat_remains` 读它）。于是**画笔画的碎屑和尸体是一格 0 能量的东西**
    /// ——能吃，但吃了等于没吃。用户把一具全身是嘴的生物埋进自己画的
    /// 碎屑堆里，它饿死，而画面上完全看不出原因。
    ///
    /// `LifeSim::new` 里有一段"给所有 `flags == 0` 的残骸补能量"，
    /// 但它只在**开世界那一刻**跑一次。玩家在运行中画的东西赶不上那一趟。
    ///
    /// ## 为什么要记进账本
    ///
    /// 那 25 点能量是新进世界的（画笔凭空造物，和画水一样）。不记进
    /// `ledger.injected` 的话，`audit()` 会在下一次统计时报"多出 N 点"
    /// ——而那个报错会指向生命层，查起来毫无头绪。
    ///
    /// 返回补了多少格。
    pub fn adopt_organics(&mut self, world: &mut World, cells: &[(i32, i32)]) -> u32 {
        let mut n = 0u32;
        for &(x, y) in cells {
            let (xs, ys) = (x as isize, y as isize);
            if !matches!(
                world.get(xs, ys),
                MaterialKind::Remains | MaterialKind::Corpse
            ) {
                continue;
            }
            // **只补"刚画上去的"**：`world.set` 清过 flags，所以刚画的那一格
            // 必然是 0。已经带能量的（天然碎屑、别人吃剩的）保持原值。
            if world.flags(xs, ys) != 0 {
                continue;
            }
            world.set_flags(xs, ys, REMAINS_ENERGY as u8);
            self.ledger.injected += REMAINS_ENERGY as u64;
            n += 1;
        }
        n
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
    /// 把一批有机体**整个**从世界里拿走，并归还它们借过的水。
    ///
    /// 这不是"杀死"：不产生尸体、不记账、不留能量。它对应的是玩家手里的
    /// **生物橡皮**——一个编辑动作，不是一次生态事件。
    ///
    /// ## 为什么不能复用 `bury`
    ///
    /// `bury` 是死亡路径：它会写尸体、把剩余能量记进耗散。橡皮抹掉的东西
    /// 从来没死过，凭空多出一具带能量的尸体会让能量账凭空多出几万点
    /// （`injected` 里没有这一份），守恒断言立刻报错。
    ///
    /// 归还水则是必须的：身体占住的每一格都是从水里借的（`Cell::wet`），
    /// 不还的话每抹一条鱼就从世界里挖掉几格水，而玩家会一直抹。
    ///
    /// 返回真正被移除的个数。
    pub fn remove_organisms(&mut self, world: &mut World, ids: &[OrganismId]) -> usize {
        // 先把要删的**分出来**再处理：`retain` 的闭包拿不到 `&mut World`，
        // 而归还水要写世界。
        let (doomed, kept): (Vec<Organism>, Vec<Organism>) = std::mem::take(&mut self.organisms)
            .into_iter()
            .partition(|o| ids.contains(&o.id));
        self.organisms = kept;
        for org in &doomed {
            for cell in &org.cells {
                let (x, y) = org.cell_at(cell);
                let (x, y) = (x as isize, y as isize);
                world.vacate(x, y);
                if cell.wet {
                    world.set(x, y, MaterialKind::Water);
                }
            }
        }
        doomed.len()
    }

    fn bury(&mut self, world: &mut World, org: &Organism) {
        let n = org.cells.len().max(1) as u32;
        // 每格分不到 4 点能量的尸体视为"已经被自己烧光了"：flags 直接写 0。
        // （腐烂已经不看 `flags` 了——那套判据被 `Corpse` 材质取代。
        // 这里仍然把零头归零，只是为了不留下"1 点能量的尸体"这种噪声。）
        // **尸体值的是"脂肪 + 身体"。**
        //
        // 早先只算 `energy`，于是饿死的身体（把脂肪烧光了）留下一具
        // 0 能量的尸体——而饿死是主要死因。实测 61% 的尸体格能量为 0，
        // 全部尸体只占账本注入的 0.18%，食腐这个生态位在数学上不存在。
        // `structure` 就是那一份"身体本身"，见 `life::STRUCTURE_SHARE`。
        let per_cell = (org.energy + org.structure) / n;
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
            if cell.wet {
                // 借过水的那几格：留**尸体**。尸体腐烂时把那一格水还回来，
                // 所以"1 格尸体 = 1 格借来的水"是这条规则的地基。
                world.set(x, y, remains_of(cell.kind));
                // `set` 会把 flags 清 0，所以能量必须写在它之后。
                world.set_flags(x, y, per_cell as u8);
                placed += 1;
            } else {
                // **没借过水的细胞：留碎屑，不留尸体。**
                //
                // 出生在空气里的身体一格水都没占过，它的死不该还出任何水
                // ——否则凭空多出几格水（见 `Cell::wet` 的注释）。但它**必须
                // 留下点东西**：早先这里直接跳过，于是"往陆地上一放，鱼掉下来、
                // 挣扎两秒、凭空消失"。玩家看到的是 bug，不是物理。
                //
                // 碎屑是粉末（`MoveRule::Falls`），所以半空里死的会掉到地上堆起来；
                // 它也是 `eat_remains` 的食物——**陆地有机质的入口**就在这一行。
                //
                // 能量**必须**写进去，和湿尸体一样多。v0.34 第一版漏了这一行
                // （`world.set` 会把 flags 清 0），于是干尸体是一格 0 能量的东西
                // ——能吃，但吃了等于没吃。同一个字段又一次只走了一半的路径。
                world.set(x, y, MaterialKind::Remains);
                world.set_flags(x, y, per_cell as u8);
                placed += 1;
            }
        }
        // 摊不尽的零头 + **没能落到任何一格上的那份**（干细胞的份额）都要记账。
        // 少记这一项，守恒断言会报"少了 N 点"——实测 32000 tick 少了 22657 点。
        self.ledger.dissipated += ((org.energy + org.structure) - per_cell * placed) as u64;
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
            let mut killers = org.cells.iter().filter(|c| c.organelles.has(STING));
            if killers.next().is_none() {
                continue;
            }
            for cell in org.cells.iter().filter(|c| c.organelles.has(STING)) {
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
                .filter(|c| c.organelles.has(ARMOR))
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
            // 捕食者吃的是**脂肪**（猎物的 `energy`），不是猎物的身体——
            // 身体留在尸体里给食腐者。这一条把两个生态位分开：
            // **捕食者拿现金，食腐者拿房产。**
            let org = &self.organisms[pi];
            let room = org.max_energy().saturating_sub(org.energy)
                + org.max_structure().saturating_sub(org.structure);
            let gained = self.organisms[pi].absorb(dealt.min(room));
            self.ledger.dissipated += (dealt - gained) as u64;
            self.stats.bitten += 1;
        }
    }

    /// 这具身体有没有**根贴着水**。
    ///
    /// 判据是"某一格根细胞的四邻里有水"——**根要够得着水，不是身体泡在水里**。
    /// 这就是根和"整具身体泡在水里"的区别：后者要求锚点所在的粗格有水，
    /// 前者只要求某一格根挨着一格水。
    /// 根能拿到的水：`None` = 一点都没有，`Some(255)` = 贴着水，
    /// `Some(m)` = 从土壤里吸，`m` 是那一列的湿度。
    ///
    /// ## 为什么根要能从土里吸水
    ///
    /// v0.37 的根只认"四邻是 `Water`"。那条判据让根变成了一个
    /// **"水边开关"**：内陆一格水都没有，于是根扎得再好也活不了，
    /// 植物永远上不了岸。而真实的根主要吸的是**土壤水**，不是泡在水里。
    ///
    /// 现在多一条路：四邻是石头或沙 → 按**那一列的湿度**吸
    /// （见 [`LifeSim::moisture_at`]）。湿度随离水距离衰减，
    /// 于是"能走多远"第一次变成了一个空间上的答案，而不是一道墙。
    ///
    /// 取所有根里**最湿的那一处**，不是平均：一棵树有一条根扎到了湿土，
    /// 整株就该活——根是并联的，不是串联的。
    fn root_water(world: &World, moisture: &Moisture, org: &Organism) -> Option<u8> {
        let mut best: Option<u8> = None;
        for cell in &org.cells {
            if !cell.organelles.has(ROOT) {
                continue;
            }
            let (x, y) = org.cell_at(cell);
            for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                let (nx, ny) = (x + dx, y + dy);
                match world.get(nx as isize, ny as isize) {
                    // 贴着水：满。直接返回，不用再看别的根。
                    MaterialKind::Water => return Some(u8::MAX),
                    // 土壤：按这一列的湿度。
                    MaterialKind::Stone | MaterialKind::Sand => {
                        // **读那一格自己的湿度**，不是那一列的。二维场之后
                        // 悬崖底下和悬崖顶上不是同一个数——而根扎在哪儿
                        // 就该读哪儿。
                        let m = moisture.at(nx, ny);
                        best = Some(best.map_or(m, |b| b.max(m)));
                    }
                    _ => {}
                }
            }
        }
        best
    }

    /// 每 tick 更新含水量。
    ///
    /// 放在 `inject_sunlight` **之前**：光合这一 tick 用的是这一 tick 的水。
    fn tick_hydration(&mut self, world: &World) {
        let max = crate::life::HYDRATION_MAX;
        let drain = crate::life::HYDRATION_DRAIN;
        // 先把湿度表借出来：循环里借的是 `organisms`，两个字段互不相干。
        let moisture = &self.moisture;
        for org in &mut self.organisms {
            // 整具身体泡在水里：直接满，和根无关。
            if dampness(self.field.water_frac(org.anchor.0 as f32, org.anchor.1 as f32)) > 0.5 {
                org.hydration = max;
                continue;
            }
            match Self::root_water(world, moisture, org) {
                // 根贴着水：满。
                Some(u8::MAX) => org.hydration = max,
                // 根在土里：按湿度回充，**扣掉蒸发**。
                //
                // 净收支 = `湿度 / 40` − 蒸发。所以湿度 40..79 那一档
                // 正好打平（活得住，但长不回来），再干就入不敷出。
                // 这条分档很粗（只有 7 档），换来的是全整数运算——
                // 用小数累积就得给 `hydration` 加一个进位字段，而那个字段
                // 要进存档，而"为了一个进位改存档版本"不值。
                Some(m) => {
                    let refill = m / SOIL_REFILL_PER;
                    if refill >= drain {
                        // **必须 `saturating_add`。** 写成 `(h + d).min(max)`
                        // 会在满水时先溢出：`255u8 + 5` 在 release 下回绕成 **4**，
                        // 于是"水最多的地方植物最渴"。而 debug 下它直接 panic，
                        // 所以单元测试（跑 debug）根本看不到——只有生态测量会看到，
                        // 而且看起来像"植物上岸就死"。
                        org.hydration = org.hydration.saturating_add(refill - drain).min(max);
                    } else {
                        org.hydration = org.hydration.saturating_sub(drain - refill);
                    }
                }
                // 一点水都够不到：干耗。
                None => org.hydration = org.hydration.saturating_sub(drain),
            }
        }
    }

    /// 生产者按**所在位置的光照**产能量。
    ///
    /// 这是世界里唯一一条"环境 → 生物"的能量入口（SPEC §3.3）。
    /// 光的分布由 [`crate::field::AirField`] 算：太阳从顶上照下来，
    /// 在水里按 `SUN_ATTEN` 衰减，被石头和沙挡住，点光源从自己那儿往外照。
    ///
    /// ## 为什么把"竞争系数"删了
    ///
    /// 旧版乘了一个 `1 / (1 + 生产者总数 / CARRYING_CAPACITY)`。那是光场
    /// 还不存在时的替代品：既然没有真正的光，"生产者太多就把光分薄了"
    /// 只能靠一个全局常数硬凑。现在光场是真的了，这条就成了**双重计费**——
    /// 而且那个 2400 是个没有物理来源的数字（它是"这样种群会停在 240 条"倒推的）。
    ///
    /// 现在的负反馈是**几何**的：生产者必须待在透光层，透光层的格子有限，
    /// 而一格只能站一个细胞。种群会停在"把透光层占满"的位置，
    /// 想改平衡就去改地貌或改光，而不是改一个常数。
    ///
    /// ## 温度也进来
    ///
    /// 产率再乘一个 [`crate::field::photosynthesis`] 的温度响应。
    /// 没有这一条，温度场就只是一张好看的图——而"好看但没用"的场
    /// 迟早会因为没人维护而悄悄烂掉。
    fn inject_sunlight(&mut self, world: &mut World) {
        // 光场是纯函数，低频重算就够；温度是状态，按自己的间隔积分。
        // 两者共用同一次世界扫描（见 `AirField::scan`）。
        let tick = world.tick();
        if tick % crate::field::LIGHT_INTERVAL == 0 {
            self.field.update_light(world);
        }
        if tick % crate::field::TEMP_INTERVAL == 0 {
            self.field.update_temp(world);
        }

        let mut injected = 0u64;
        // 湿度表和 `organisms` 是两个字段，可以同时借。
        let moisture = &self.moisture;
        for org in &mut self.organisms {
            let producers = org
                .cells
                .iter()
                .filter(|c| c.organelles.level(CHLORO_SHIFT) > 0)
                .count();
            if producers == 0 {
                continue;
            }
            let (ax, ay) = (org.anchor.0 as f32, org.anchor.1 as f32);
            // **光合作用需要水，但不是一个开关。**
            //
            // 这一条看着像常识，但它是必需的：没有它，一条被扔在岸上的鱼
            // 会顶着满格阳光疯狂产能量，"搁浅会死"在物理上就不成立了。
            // 而"离开水会死"是用户明确要的行为，不能因为光场变真了就丢掉。
            //
            // 判据用粗格的水占比而不是逐格材质：身体格自己的材质是 `Empty`，
            // 问"这一格是不是水"永远得到否。
            //
            // **v0.31：从门槛改成斜坡**（见 [`dampness`]）。原来是
            // `water_frac < 0.35` 直接跳过，于是"半上岸"在能量上不存在。
            let mut damp = dampness(self.field.water_frac(ax, ay));
            // **根贴着水 → 至少 `land_damp(根格数)`。**
            //
            // 这一条才是"植物能上岸"的全部：根扎在岸边的水里，
            // 身体长在岸上，光合按根系厚度跑。没有根的话 `damp` 仍然是
            // 粗格水占比——离开水面就是 0。
            //
            // 用 `max` 而不是赋值：半截泡在水里的身体本来就该按
            // 粗格水占比算，不该被根系"拉低"到岸上的地板。
            // 根的水源决定**岸上产能的地板**，而且地板本身**随湿度打折**。
            //
            // 只按"有没有根"给地板的话，内陆和海岸产得一样多——那这条
            // 土壤湿度就只影响"回充快慢"，而回充快慢在满水时完全看不出来
            // （`hydration` 一直是 255）。**物理上真正发生的事是气孔关闭**：
            // 土越干，同样一片叶子能固定的碳越少。所以地板要乘湿度。
            //
            // 这一条和 `hydration` 是两件事，两个都要有：
            //   - `damp` 打折 → **产能**立刻降（当天就少长）；
            //   - `hydration` 掉 → 撑一段时间后**彻底停**（旱久了就死）。
            // 只有前者，植物在沙漠里也能慢慢攒能量；只有后者，
            // 满水状态下看不出任何梯度。
            if let Some(m) = Self::root_water(&*world, moisture, org) {
                let roots = org.cells.iter().filter(|c| c.organelles.has(ROOT)).count();
                let supply = m as f32 / u8::MAX as f32;
                damp = damp.max(land_damp(roots) * supply);
            }
            if damp <= 0.0 {
                continue;
            }
            // 再乘含水量：离水之后它靠身体里存的那点水撑一阵。
            let wet = org.hydration as f32 / crate::life::HYDRATION_MAX as f32;
            if wet <= 0.0 {
                continue;
            }
            let light = self.field.light_at(ax, ay);
            if light <= 0.0 {
                continue;
            }
            let warmth = crate::field::photosynthesis(self.field.temp_at(ax, ay));
            let gain = producer_gain(light, warmth, damp, wet, producers);
            if gain == 0 {
                continue;
            }
            // 收进来的这份能量按 `STRUCTURE_SHARE` 分成"生物量"和"脂肪"。
            // `absorb` 返回真正收下的量——两个池子都满了就收不下，
            // 那部分不该记进"注入"。
            let gain = org.absorb(gain);
            if gain == 0 {
                continue;
            }
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
                // 尸体烂掉之后**永远变回水**，不看出现在哪儿。
                //
                // 为什么不能按"现在泡不泡在水里"决定：**每一格尸体都对应一格
                // 借来的水**（`bury` 只给 `Cell::wet` 的细胞留尸体）。而"现在泡在
                // 水里吗"是腐烂那一刻的状态，不是出生那一刻的状态——一条贴着
                // 水面出生的鱼死后，尸体在 y=水面那一行，上方是空气，
                // `underwater` 判它"干的"，于是烂成空位，那一格水就**永久消失**了。
                //
                // 实测：600 tick 的繁殖压力测试里，第 423 tick 水位少了 1 格，
                // 而能量账一分不差、生物数还在涨——从任何别的指标上都看不出来。
                // 这正是"事后按当前状态倒推当初发生了什么"那一类错误。
                //
                // 代价是"水面之上的尸体会烂出一格水然后落下来"——那不是 bug，
                // 那是体积守恒本身：水不会凭空消失，只会换个地方待着。
                world.set(xs, ys, MaterialKind::Water);
            }
        }

        // 腐烂的能量是热量：离开生物可用池，但仍在账上。
        self.ledger.dissipated += rotted;
        self.stats.corpses_rotted = rotted_cells;

        if self.food_growth {
            self.seed_plankton(world);
        }
    }

    /// 播种浮游藻：在水里撒几个**单细胞生产者**。
    ///
    /// ## 藻为什么是生物，不是材质
    ///
    /// 早先 `Algae` 是一种材质（悬浮、可吃、被水挤开）。那套东西能跑，
    /// 但它有三条别扭的地方，每一条都在逼着补丁：
    ///
    /// 1. **能量要单独记一条注入通道**。藻的能量写在材质的 `flags` 里，
    ///    每长一格就要 `ledger.injected += FOOD_ENERGY`——而 `inject_sunlight`
    ///    明明已经在按生产者细胞算能量了。两条通道算同一件事。
    /// 2. **"被吃掉"要走 `eat_remains`**，和碎屑、尸体挤在同一个函数里，
    ///    于是"该不该还水"要靠材质种类区分。
    /// 3. **密度是个没法调对的数**：比水轻会被一路顶到水面（鱼也跟着挤上去），
    ///    和水同重就变成水流的半固体障碍（实测世界冻住）。
    ///
    /// 变成生物之后这三条全部消失：它靠 `inject_sunlight` 产能（自动有
    /// 容纳量制约）、被吃掉是"捕食"、它想待在哪儿由自己的行为决定（不动）。
    ///
    /// ## 为什么是"撒点"而不是"每格掷骰子"
    ///
    /// 透光层有两万格，而每次只撒几个——扫一遍的 99.9% 是白跑。
    /// 随机撒点是 O(尝试次数)，和世界大小无关，而且**通量是常数**：
    /// 决定"藻能养活多少东西"的是这个常数，不是世界有多大。
    fn seed_plankton(&mut self, world: &mut World) {
        // **不能用 `self.stats.plankton`**：`stats` 在 `step` 开头刚被重置成
        // 默认值，这里的 `plankton` 永远是 0，上限检查形同虚设——实测藻
        // 一路涨到 1156 个（上限是 600），把光全吃光，鱼饿死绝种。
        let mut count = self.organisms.iter().filter(|o| o.is_plankton()).count();
        if count >= PLANKTON_CAP {
            return;
        }
        let (w, h) = (world.width(), world.height());
        if w == 0 || h == 0 {
            return;
        }
        let mut rng = Rng::new(world.tick().wrapping_mul(0x9E37_79B9) ^ SALT_PLANKTON_SPAWN);
        for _ in 0..PLANKTON_PER_SWEEP {
            if count >= PLANKTON_CAP {
                break;
            }
            for _ in 0..PLANKTON_TRIES {
                let x = rng.gen_range_usize(0, w);
                // 先找这一列的水面：从顶往下第一格水。
                let Some(surface) =
                    (0..h).find(|&y| world.get(x as isize, y as isize) == MaterialKind::Water)
                else {
                    continue;
                };
                let depth = rng.gen_range_usize(0, PLANKTON_DEPTH as usize);
                let y = surface + depth;
                if y >= h {
                    continue;
                }
                let (xs, ys) = (x as isize, y as isize);
                if world.get(xs, ys) != MaterialKind::Water || world.is_occupied(xs, ys) {
                    continue;
                }
                let mut org = Organism::new(
                    0,
                    (x as i32, y as i32),
                    vec![Cell::new(CellType::Producer, (0, 0))],
                    plankton_genome(),
                );
                org.energy = PLANKTON_START_ENERGY;
                if self.spawn_injected(world, org).is_ok() {
                    count += 1;
                    break;
                }
            }
        }
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
                    MaterialKind::Remains | MaterialKind::Corpse
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
    /// 浮游藻分裂：能量够了的单细胞生产者一分为二。
    ///
    /// ## 为什么不用 `reproduce`
    ///
    /// `reproduce` 要过三道关：`fits`（每一格都能放 + 水陆判据）、
    /// `nearby` 拥挤检查（O(n) 扫所有生物）、基因组突变。
    ///
    /// 对鱼来说这三道都必要。对藻来说**全都是浪费**：藻只有一格、不需要
    /// 变形状（形状永远是"一格生产者"）、而拥挤由光照容纳量自动制约
    /// （`inject_sunlight` 里的 `CARRYING_CAPACITY`）。
    ///
    /// 几百个藻每 tick 各走一遍 `reproduce` 会让帧时间翻几倍，而换来的
    /// 只是"藻的基因在漂移"——那不是这一层想展示的东西。
    fn tick_plankton(&mut self, world: &mut World) {
        let mut count = self.organisms.iter().filter(|o| o.is_plankton()).count();
        if count >= PLANKTON_CAP {
            return;
        }
        // 先收集再结算：分裂会 push 新生物，边遍历边 push 借不过编译器。
        // 正序安全：新生物追加在末尾，不在这个列表里。
        let ready: Vec<usize> = self
            .organisms
            .iter()
            .enumerate()
            .filter(|(_, o)| o.is_plankton() && o.energy >= o.reproduction_energy())
            .map(|(i, _)| i)
            .collect();
        for i in ready {
            if count >= PLANKTON_CAP {
                break;
            }
            if self.split_plankton(world, i) {
                count += 1;
            }
        }
    }

    /// 重建藻的 id 表。每 tick 一次，O(n log n)。
    fn rebuild_plankton_ids(&mut self) {
        self.plankton_ids.clear();
        self.plankton_ids.extend(
            self.organisms
                .iter()
                .filter(|o| o.is_plankton())
                .map(|o| o.id),
        );
        self.plankton_ids.sort_unstable();
    }

    /// 让第 `i` 个藻分裂一次。返回是否真的生出来了。
    fn split_plankton(&mut self, world: &mut World, i: usize) -> bool {
        if i >= self.organisms.len() {
            return false;
        }
        let (ax, ay) = self.organisms[i].anchor;
        let width = self.organisms[i].width;
        let half = self.organisms[i].birth_energy();
        if self.organisms[i].energy < half {
            return false;
        }
        let generation = self.organisms[i].generation + 1;
        let id = self.organisms[i].id;

        // 四邻找一格空水。**只找紧邻**：藻没有运动器官，子代只能长在
        // 身边。这一条让藻群是"从中心慢慢铺开"，而不是瞬间出现在别处。
        for (dx, dy) in NEIGHBORS_4 {
            let nx = if width > 0 {
                (ax + dx).rem_euclid(width)
            } else {
                ax + dx
            };
            let ny = ay + dy;
            if !world.in_bounds(nx as isize, ny as isize) {
                continue;
            }
            if world.get(nx as isize, ny as isize) != MaterialKind::Water
                || world.is_occupied(nx as isize, ny as isize)
            {
                continue;
            }
            let mut child = Organism::new(
                0,
                (nx, ny),
                vec![Cell::new(CellType::Producer, (0, 0))],
                plankton_genome(),
            );
            child.energy = half;
            child.parent = Some(id);
            child.generation = generation;
            if self.spawn(world, child).is_ok() {
                // 能量是**转移**，不是注入：亲代少一半，子代多一半。
                self.organisms[i].energy -= half;
                return true;
            }
        }
        false
    }

    /// 重力：**不在水里的身体会往下掉**。
    ///
    /// ## 为什么必须有这一条
    ///
    /// 玩家把鱼放在半空时，它会原地冻结——因为 `touches_water` 拦住了
    /// 所有主动移动（任何一步都在空气里）。那不是物理，是判据用错了地方：
    /// 水陆判据回答的是"它能不能**游**"，不是"它能不能**存在**"。
    ///
    /// ## 为什么掉一格，不加速
    ///
    /// 真正的自由落体是加速的。但在这个世界里一格就是一次位置更新，
    /// 512 格高的世界里从水面掉到海床也只要几百 tick——加上加速度
    /// 反而会让"看着它掉下去"变成"一眨眼就没了"。而且每 tick 一格
    /// 不会穿模：每一格都单独验通行性。
    ///
    /// ## 落在固体上会怎样
    ///
    /// 停住，然后**搁浅**。它仍然不在水里，所以既不能游也不能再生，
    /// 但维持成本照扣——于是它会在原地饿死、变成尸体。那正是
    /// "鱼离开水会死"该有的样子，比"悬在空中冻结"诚实得多。
    fn apply_gravity(&mut self, world: &mut World) {
        for i in 0..self.organisms.len() {
            let cells: Vec<(i32, i32)> = self.organisms[i].footprint().collect();
            let width = self.organisms[i].width;
            if Self::touches_water(world, width, &cells) {
                continue;
            }
            // 往下掉一格。`MoveKind::Fall` 会跳过水陆判据、也不扣能量。
            self.try_move(world, i, 0, 1, MoveKind::Fall);
        }
    }

    /// 累计下落次数。这是"重力有没有在工作"的直接证据。
    #[inline]
    pub fn fell_total(&self) -> u64 {
        self.fell_total
    }

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
        // 藻没有脑。几百个藻每 tick 各跑一遍 `sense` + `think` 是纯浪费，
        // 而它的行为（不动、只繁殖）已经在 `tick_plankton` 里处理完了。
        if self.organisms[i].is_plankton() {
            self.organisms[i].acted = true;
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
            let id = self.organisms[i].id;
            // 先吃材质（碎屑、尸体），再吃藻。
            //
            // 顺序有意义：材质格不会跑，藻会（被水流挤开）。先吃确定的那个，
            // 少一次查表。而且材质是"现成的尸体"，藻是"活着的猎物"——
            // 饿的时候先吃白捡的。
            if let Some((x, y)) = self.food_target(world, i) {
                if self.eat_remains(world, id, x, y) {
                    eaten = 1;
                }
            } else if let Some(prey) = self.plankton_target(world, i) {
                if self.eat_plankton(id, prey) {
                    eaten = 1;
                }
            }
        }

        // 移动 + 撞墙 fallback + `stuck` 计数。
        //
        // ## 为什么要有 fallback
        //
        // 旧写法撞墙就 `break`——本 tick 剩下的位移全部作废，而且**不会
        // 退而求其次试第二名的方向**。于是一条"向右"权重稍大的鱼撞上崖壁
        // 之后，会一直向右、一直撞、一直向右。玩家看到的是"只朝一个方向游"。
        //
        // 现在：主方向失败时，如果它是斜向，就退到两个单向里各试一次。
        // 15 行改动，把"撞墙即停"变成"沿岸蹭着走"。
        let mut moved_any = false;
        if let Some((dx, dy)) = pick_direction(&out) {
            let speed = self.organisms[i].speed();
            for _ in 0..speed {
                if self.try_move(world, i, dx, dy, MoveKind::Swim) {
                    moved_any = true;
                    continue;
                }
                let mut fell_back = false;
                if dx != 0 && dy != 0 {
                    for (ax, ay) in [(dx, 0), (0, dy)] {
                        if self.try_move(world, i, ax, ay, MoveKind::Swim) {
                            fell_back = true;
                            moved_any = true;
                            break;
                        }
                    }
                }
                if !fell_back {
                    break;
                }
            }
        }
        // 一个 tick 里一格也没走成，就记一次"卡住"。
        //
        // 这是 `IN_STUCK` 的来源，也是"这个个体是不是卡住了"的直接证据——
        // 画面上一条鱼在原地不动，和一条鱼在慢慢蠕动，看起来是同一件事。
        if moved_any {
            self.organisms[i].stuck = 0;
        } else {
            self.organisms[i].stuck = self.organisms[i].stuck.saturating_add(1);
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
        let mut inp = [0.0f32; INPUTS];

        let (fx, fy) = org.front();
        inp[IN_FOOD_AHEAD] = matches!(
            world.get(fx as isize, fy as isize),
            MaterialKind::Remains | MaterialKind::Corpse
        ) as u8 as f32;
        inp[IN_BLOCKED_AHEAD] = (!Self::passable(world, fx, fy)) as u8 as f32;

        let owner = world.owner(fx as isize, fy as isize);
        inp[IN_KIN_AHEAD] = (owner.is_some() && owner.organism != org.id) as u8 as f32;
        // "前方有别的生物"——对捕食者就是"前方有猎物"。
        //
        // 还没有物种概念，所以"异种"就是"别的个体"。这和 `IN_KIN_AHEAD`
        // 目前同值，但两路的**用途**不同：亲缘那一路将来要喂给"别吃同类"，
        // 异种那一路喂给"扑上去"。现在合并着，等有了物种再分开。
        inp[IN_ALIEN_AHEAD] = inp[IN_KIN_AHEAD];

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
        inp[IN_BLOCKED_LEFT] = (!Self::passable(world, lx, ly)) as u8 as f32;
        inp[IN_BLOCKED_RIGHT] = (!Self::passable(world, rx, ry)) as u8 as f32;
        inp[IN_BLOCKED_UP] = (!Self::passable(world, ux, uy)) as u8 as f32;
        inp[IN_SUPPORTED_BELOW] = (!Self::passable(world, dx_, dy_)) as u8 as f32;

        inp[IN_ENERGY] = org.energy as f32 / org.max_energy().max(1) as f32;
        inp[IN_AGE] = org.age as f32 / MAX_AGE as f32;
        inp[IN_FACING_X] = org.facing.x as f32;
        inp[IN_FACING_Y] = org.facing.y as f32;

        // 光照：**直接读光场**。
        //
        // 这里原本是"从身体顶端往上数，到第一个非水格有多远 / LIGHT_DEPTH"。
        // 那是个**代理量**：光场还不存在时，深度是唯一能代表光的几何量。
        // 现在光场是真的了，代理量必须换成真值，否则两件事会分家——
        // 比如一条鱼站在岛的阴影里，代理量说它"贴着水面所以很亮"，
        // 而它其实一分光都拿不到。这种不一致不会有任何报错，
        // 只会表现为"某些鱼莫名其妙饿死"。
        //
        // 取**身体顶端**而不是锚点：光从上面来，从头顶那一格进入身体。
        // 一条贴着海床、但头顶伸出十几格的生物，读到的仍然是它头顶的光。
        let (ax, ay) = (org.anchor.0, org.anchor.1);
        let top = org
            .cells
            .iter()
            .map(|c| ay + c.offset.1 as i32)
            .min()
            .unwrap_or(ay);
        inp[IN_LIGHT] = self.field.light_at(ax as f32, top as f32).clamp(0.0, 1.0);

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
                    // **藻是食物**：闻到它的时候也记进食物方向。
                    //
                    // 这一条是"藻从材质变成生物"之后必须补的：材质时代
                    // 射线找的是"含能量的 `Remains`"，藻变成生物之后
                    // 射线看不见它了——于是鱼**闻不到食物**，而藻又占格
                    // 不让进，结果是鱼在藻群里饿死（实测 500 tick 全灭）。
                    if self.plankton_ids.binary_search(&owner.organism).is_ok() {
                        smell_x += dx as f32 / d as f32;
                        smell_y += dy as f32 / d as f32;
                    }
                    break;
                }
                if matches!(
                    world.get(sxs, sys),
                    MaterialKind::Remains | MaterialKind::Corpse
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
        // ---- 尸体味：单独一条射线（v0.39）----
        //
        // 判据和上面那条**故意不同**（见 `IN_CORPSE_DIR_X` 的文档）：
        // 活体和水都不挡（腐烂的化学信号溶在水里），只有石头和沙挡。
        //
        // 为什么必须是第二条射线：上面那条遇到**第一个**食物就 break，
        // 而海里第一个食物几乎总是藻。共用的话这一路会恒为 0。
        let (mut corpse_x, mut corpse_y) = (0.0f32, 0.0f32);
        for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            for d in 1..=smell_radius {
                let sx = org.wrap_x(org.anchor.0 + dx * d);
                let sy = org.anchor.1 + dy * d;
                let (sxs, sys) = (sx as isize, sy as isize);
                if !world.in_bounds(sxs, sys) {
                    break;
                }
                let kind = world.get(sxs, sys);
                // 只在**带能量的**尸体上停：0 能量的尸体吃了也没有意义，
                // 而让食腐者去追一具空壳比闻不到更糟。
                if kind == MaterialKind::Corpse && world.flags(sxs, sys) > 0 {
                    corpse_x += dx as f32 / d as f32;
                    corpse_y += dy as f32 / d as f32;
                    break;
                }
                // 水 / 气 / 空位 / 碎屑 / 活体（材质是 `Empty`）都能闻过去。
                if !matches!(
                    kind,
                    MaterialKind::Water
                        | MaterialKind::Gas
                        | MaterialKind::Empty
                        | MaterialKind::Remains
                ) {
                    break;
                }
            }
        }
        inp[IN_CORPSE_DIR_X] = corpse_x.clamp(-1.0, 1.0);
        inp[IN_CORPSE_DIR_Y] = corpse_y.clamp(-1.0, 1.0);

        inp[IN_FOOD_DIR_X] = smell_x.clamp(-1.0, 1.0);
        inp[IN_FOOD_DIR_Y] = smell_y.clamp(-1.0, 1.0);
        inp[IN_ORGANISM_DIR_X] = org_x.clamp(-1.0, 1.0);
        inp[IN_ORGANISM_DIR_Y] = org_y.clamp(-1.0, 1.0);

        // ---- M2a：递归状态 ----
        //
        // 把上一 tick 的输出喂回来。这是**记忆**，不是学习：
        // 遗传的仍然只有基因，一生的经历不进基因。
        //
        // 它买到的东西正好是原来缺的三样：惯性巡航（上一 tick 在向右 →
        // 这一 tick 继续）、撞墙后转向（`IN_BLOCKED_*` × `IN_PREV_*`）、
        // 以及节律（能表达"走两格停一格"这种模式，纯前馈网络表达不了）。
        inp[IN_PREV_LEFT] = org.last_out[OUT_LEFT];
        inp[IN_PREV_RIGHT] = org.last_out[OUT_RIGHT];
        inp[IN_PREV_UP] = org.last_out[OUT_UP];
        inp[IN_PREV_DOWN] = org.last_out[OUT_DOWN];
        // 8 tick 封顶再归一化：超过 8 tick 的"卡住"没有更多信息，
        // 而一个无上限的计数会让权重的量级随世界大小漂。
        inp[IN_STUCK] = (org.stuck as f32 / 8.0).min(1.0);
        inp
    }

    /// 这一格能不能进（SPEC §2.2）。
    ///
    /// 石 / 沙 / 残骸一律挡；水和气放行；火在 M2 里也挡
    /// （伤害规则推迟到有装甲细胞的时候一起做，免得先做一套没法验证的伤害）。
    ///
    /// ## 水曾经是"大身体的墙"（v0.26 拆掉）
    ///
    /// 这里原来是 `MaterialKind::Water => body <= SMALL_BODY_MAX`，而那个常量是
    /// **16**。也就是说 17 格以上的身体在水里，**水是固体**——不是"推不动"，是
    /// "进不去"。症状是一条完整的链：通行判定全部失败 → `touches_water` 也失败
    /// → 重力接管 → 一路掉到海底 → 搁浅 → 维持成本照扣 → 饿死。
    /// 玩家看到的是"细胞数过多无法在水中浮起"，而真相是**一条性能约束被写成了
    /// 世界法则**（和"世界尺寸必须是 64 的倍数"是同一类错误）。
    ///
    /// 那个常量给的理由是"1000 格的生物移动会推走 1000 格水，行为和性能都会崩"。
    /// 这个担心**当时**成立，现在不成立：身体格数上限是
    /// [`crate::morph::MAX_BODY_CELLS`] = 24，一次移动最多 swap 24 格水
    /// （见 [`Self::body_displaces`]），是 O(n) 不是 O(n²)。
    /// **那个数字守的是一道已经不存在的门。**
    ///
    /// 大身体"在水里更累"这件事由 `move_cost` 的质量项表达，不由通行性表达——
    /// 前者是连续的代价，后者是一道墙。墙会让玩家看到"鱼掉到海底饿死"，
    /// 代价会让玩家看到"大身体游得慢、吃得少、活得短"，后者才是生态。
    fn passable(world: &World, x: i32, y: i32) -> bool {
        if !world.in_bounds(x as isize, y as isize) {
            return false;
        }
        if world.is_occupied(x as isize, y as isize) {
            return false;
        }
        match world.get(x as isize, y as isize) {
            MaterialKind::Empty | MaterialKind::Gas => true,
            // 水和身体同重：任何体型都能进。
            MaterialKind::Water => true,
            // 藻是悬浮的食物，**不是障碍**：鱼要能游进去吃。
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
        kind.is_fluid()
    }

    /// 嘴的四邻里有没有**藻**（单细胞生产者）。
    ///
    /// 藻是生物不是材质，所以吃它要走"捕食"那条路，而不是 `eat_remains`。
    /// 但触发条件不一样：吃藻是**吃**（`OUT_EAT`），不是咬（Killer 细胞）。
    /// 一条只有嘴没有牙的鱼照样该能吃藻——那是初级消费者的定义。
    fn plankton_target(&self, world: &World, i: usize) -> Option<OrganismId> {
        let org = &self.organisms[i];
        for cell in &org.cells {
            if !cell.organelles.has(DIGEST) {
                continue;
            }
            let (x, y) = org.cell_at(cell);
            for (dx, dy) in NEIGHBORS_4 {
                let nx = org.wrap_x(x + dx);
                let ny = y + dy;
                let owner = world.owner(nx as isize, ny as isize);
                if owner.organism == 0 || owner.organism == org.id {
                    continue;
                }
                if let Some(qi) = self.organisms.iter().position(|o| o.id == owner.organism) {
                    if self.organisms[qi].is_plankton() {
                        return Some(owner.organism);
                    }
                }
            }
        }
        None
    }

    /// 吃掉一个藻：能量转移，藻消失。
    ///
    /// **不留尸体**——整个被吞下去了。留尸体的话，一条鱼吃藻会在原地
    /// 吐出一具 1 格的尸体，然后那具尸体烂成水，等于"吃藻会造水"。
    fn eat_plankton(&mut self, eater: OrganismId, prey: OrganismId) -> bool {
        if eater == prey {
            return false;
        }
        let Some(qi) = self.organisms.iter().position(|o| o.id == prey) else {
            return false;
        };
        let Some(pi) = self.organisms.iter().position(|o| o.id == eater) else {
            return false;
        };
        // **藻的身体也要算进"可吃的"**：`structure` 是它长出来的组织。
        // 只算 `energy` 的话，那具身体会随着 `organisms.remove` 一起消失，
        // 而账本上什么都没记——症状是"吃掉一个藻，能量账少几点"。
        let available = self.organisms[qi].energy + self.organisms[qi].structure;
        if available == 0 {
            return false;
        }
        // 吃不下就浪费掉——和 `eat_remains` 一样：超过上限的部分变成热。
        let org = &self.organisms[pi];
        let room = org.max_energy().saturating_sub(org.energy)
            + org.max_structure().saturating_sub(org.structure);
        let gained = self.organisms[pi].absorb(available.min(room));
        if gained == 0 {
            return false;
        }
        self.ledger.dissipated += (available - gained) as u64;
        // 清空它的能量，然后**推迟**移除（见 `eaten_plankton` 的注释）。
        // 能量清零是为了让同一 tick 里别的鱼不会再吃一口。
        self.organisms[qi].energy = 0;
        self.organisms[qi].structure = 0;
        self.eaten_plankton.push(prey);
        true
    }

    /// 清理这一 tick 被吃掉的藻。
    fn clear_eaten_plankton(&mut self, world: &mut World) {
        if self.eaten_plankton.is_empty() {
            return;
        }
        let ids = std::mem::take(&mut self.eaten_plankton);
        for id in ids {
            if let Some(i) = self.organisms.iter().position(|o| o.id == id) {
                let org = self.organisms.remove(i);
                // 兜底：`eat_plankton` 已经把两个池子清零了，这里正常是 0。
                // 但"移除一个有机体而不记账"是漏账的经典来源，所以留一道保险。
                self.ledger.dissipated += (org.energy + org.structure) as u64;
                // **借走的水要还。** 藻出生时占住的那格水被记成"借"
                // （`Cell::wet`），它整个被吞掉之后水就该回来。
                // 不还的话每吃一个藻世界就少一格水——而"鱼吃藻"是常态，
                // 于是水量会随生态活跃度单调下降。实测 600 tick 少了 1919 格。
                let wet: Vec<(i32, i32)> = org
                    .cells
                    .iter()
                    .filter(|c| c.wet)
                    .map(|c| org.cell_at(c))
                    .collect();
                org.release(world);
                for (x, y) in wet {
                    world.set(x as isize, y as isize, MaterialKind::Water);
                }
            }
        }
    }

    /// 找一个能吃的残骸格：任何嘴细胞的四邻。x 方向环绕。
    fn food_target(&self, world: &World, i: usize) -> Option<(isize, isize)> {
        let org = &self.organisms[i];
        for cell in &org.cells {
            if !cell.organelles.has(DIGEST) {
                continue;
            }
            let (x, y) = org.cell_at(cell);
            for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
                let nx = org.wrap_x(x + dx);
                let ny = y + dy;
                if matches!(
                    world.get(nx as isize, ny as isize),
                    MaterialKind::Remains | MaterialKind::Corpse
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
    fn try_move(&mut self, world: &mut World, i: usize, dx: i32, dy: i32, kind: MoveKind) -> bool {
        let org = &self.organisms[i];
        let body = org.cells.len();
        // 身体格数的上界在这里钉一次。
        //
        // `Grn::max_cells` 的 `clamp(1, 24)` 和存档的校验都盯着这条线，
        // 但它们是两处独立的代码——只要有一处先改，症状就是"读档之后
        // 这条鱼比存档时多了几格"，而画面上完全看不出来。
        // 放在 `try_move` 里是因为它是**每次移动都要经过**的那条路，
        // 而且是唯一一个 `body` 已经算好的地方。
        debug_assert!(
            body <= MAX_DISPLACED,
            "身体 {body} 格超过上限 {MAX_DISPLACED}：`Grn::max_cells` 的 clamp 或存档校验漏了一处"
        );
        // 成本由 `Organism::move_cost` 统一算：**速度每加一档，每 tick 的
        // 移动开销就往上跳一跳**（1.0 → 1.5 → 2.0 倍质量）。
        //
        // 早先的写法是 `质量 / 速度`，也就是总成本不随速度涨——那样"多长
        // 移动细胞"是纯赚，体型只能朝一个方向进化。现在快慢各有代价。
        // 主动游动要花钱；**下落不花**——重力做功不是鱼在使劲。
        // 让下落扣能量会得到一个荒谬的结果：一条掉进水里的鱼因为
        // "下落太贵"而卡在半空。
        //
        // 成本在**算完落点之后**才最终定下来：上岸要按水占比加价，
        // 而水占比只能在新位置上量。
        let mut cost = match kind {
            MoveKind::Swim => org.move_cost(),
            MoveKind::Fall => 0,
        };

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
            } else if !Self::passable(world, x, y) {
                return false;
            }
            // **斜向不能穿过对角缝。**
            //
            // 网格上斜着走，两个正交邻格可能都是墙，只有对角那一格是空的。
            // 直接放行的话生物会"穿墙缝"——这是所有 8 向网格移动的经典 bug，
            // 而且症状极具欺骗性：一条 1 格厚的墙看着完好，生物却过去了。
            //
            // 判据取"至少一个正交邻格能过"而不是"两个都能过"：
            // 身体是软的，挤过半开的缝隙是合理的。代价是 1 格厚的墙
            // 在拐角处有一个斜向漏洞——`diagonal_cannot_slip_through_a_wall`
            // 那条测试钉的就是这个。
            if dx != 0 && dy != 0 {
                let a = (x - dx, y);
                let b = (x, y - dy);
                let a_ok = Self::passable(world, a.0, a.1) || old.contains(&a);
                let b_ok = Self::passable(world, b.0, b.1) || old.contains(&b);
                if !a_ok && !b_ok {
                    return false;
                }
            }
        }

        // ---- 上岸的代价：按水占比加价，不是一道墙 ----
        //
        // 这里原来是一条硬判据：`!touches_water(...)` 直接 `return false`，
        // 也就是"身体必须整个在水下才游得动"。它挡住了一个真实的问题
        // （鱼会一路游到天上：光越往上越亮、空气可通行，于是"往上"永远
        // 有奖励、从来没有代价；实测 5000 tick 就有个体停在水面上方 25 格，
        // 能量晒满、不能动也不能繁殖，整个种群随之冻死），但**用错了手段**：
        // 它把"上岸"变成一件不可能的事，而不是一件昂贵的事。
        //
        // 现在换成成本：水占比每少一分，这一步就贵一分。它不会退化成
        // "免费上天"，因为另外两件事同时在管：
        //
        // 1. **光合作用要水**（[`dampness`] 按水占比给折扣），所以空气里
        //    没有任何能量收入；
        // 2. **陆地要付支撑成本**（代谢里那条 `LAND_UPKEEP`）。
        //
        // 于是"上岸"第一次是一个**能量问题**：弱个体上去立刻饿死，强个体
        // （小、省、带够叶绿体）能撑住。这正是"陆地渐进"的定义。
        //
        // 判据用 `targets` 而不是 `old ∪ targets`：**旧位置不能算进去**。
        // 合并之后旧位置（在水里）的表面会把新位置（在空中）的表面盖过去，
        // 于是鱼可以一格一格往上爬，每次检查时旧位置总有水——实测能爬到
        // 水面上方十几格，而 `stranded` 指标看得一清二楚、移动却从不被拦。
        //
        // 只拦**主动移动**。下落必须放行——否则一条被放在半空的鱼永远
        // 掉不下去，而那正是"原地冻结"的成因。
        if kind == MoveKind::Swim {
            let wet = Self::submerged_frac(world, org.width, &targets);
            if wet < 1.0 {
                cost = (cost as f32 * (1.0 + LAND_COST * (1.0 - wet))) as u32;
            }
        }
        if org.energy < cost {
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
        if kind == MoveKind::Fall {
            // 下落单独计数：混进 `moved` 会把"垂直移动占比"这个指标污染成
            // "有多少东西正在往下掉"，而那是两件完全不同的事。
            self.stats.fell += 1;
            self.fell_total += 1;
        } else {
            // 斜向单独记一笔：`moved` 只记主导轴，看不出来。
            if dx != 0 && dy != 0 {
                self.stats.diagonal += 1;
            }
            // 方向计数：`[左, 右, 上, 下]`。
            let dir = match (dx.signum(), dy.signum()) {
                (-1, _) => 0,
                (1, _) => 1,
                (_, -1) => 2,
                (_, 1) => 3,
                _ => 1,
            };
            self.stats.moved[dir] += 1;
            self.moved_total[dir] += 1;
        }
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
        // 固定 400 会让"变大"变成纯赚——见 `life::MAX_ENERGY_PER_CELL` 那段注释。
        //
        // 注意这是"**什么时候试**"的门槛，用的是亲代当前的身体；
        // "**这一个孩子值多少**"用的是子代发育出来的格数，在下面重算。
        // 两者不能合并：变异可以让子代比亲代大得多。
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
        let facing = parent.facing;
        let generation = parent.generation + 1;
        let parent_energy = parent.energy;
        let mut genome = parent.genome.clone();
        // L2 表观遗传：压力大的亲代生出变异更大的子代。
        //
        // **上限 ×2** 是必须的。这是一个正反馈（越差越乱变 → 越乱变越差），
        // 没有上限的话一次饥荒就能把整个种群推进"随机噪声"。
        let stress = parent.stress.min(64) as f32 / 64.0;
        let sigma = MUTATION_SIGMA * (1.0 + stress);
        genome.mutate(&mut self.rng, MUTATION_RATE, sigma);

        // **子代的身体由它自己的基因组长出来。**
        //
        // 这一行是"形态变异真的会改变身体"的唯一接线点，而它原来写的是
        // `parent.cells.clone()`——子代继承父代**当前**的身体（连被咬掉的
        // 缺口一起继承），基因组一路漂移但身体从不变。
        //
        // 症状极具欺骗性：8000 tick 后**表型只有 3 种，而基因组有 647 种**
        // （简并度 215）。看数字像"中性网络非常发达"，实际是
        // **基因型和表型脱钩**——那些"中性"根本不是简并，是没接线。
        let cells = genome.body.develop();
        if cells.is_empty() {
            // 变异把身体弄没了。不是错误，是这个基因型的命运。
            return None;
        }
        // 造价按**子代**的格数算：变异可能让身体变大，那就要付更多的钱。
        // 用亲代的 `birth_energy()` 会让"变大"变成白送。
        let birth_energy = BIRTH_ENERGY_PER_CELL * cells.len().max(1) as u32;
        if parent_energy < birth_energy {
            // 攒的钱不够养这个孩子。**这是"大身体繁殖更贵"的正确表达**——
            // 它让 `max_cells` 变异有代价，而不是单向的"越大越好"。
            return None;
        }

        let child = Organism {
            id: 0, // spawn 会分配
            anchor,
            width,
            cells,
            energy: birth_energy,
            hydration: crate::life::HYDRATION_MAX,
            // 子代的生物量从 0 开始：它还没长大。
            // 这一条让"杀幼崽"和"杀成体"有区别——成体的尸体更肥。
            structure: 0,
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
            stuck: 0,
            // 子代从 0 开始：压力是**这一生**的经历，不是继承来的。
            // （母体效应体现在变异率上，已经作用在 `genome` 里了。）
            stress: 0,
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

    /// 身体有多少比例的列**在水面之下**（0..=1）。
    ///
    /// 每一列的判据和 [`Self::touches_water`] 一样：取这一列最高的那一格，
    /// 从它上方往上扫，扫到 `Water` 就算这列在水下。
    ///
    /// ## 为什么要一个比例，而不是一个布尔
    ///
    /// v0.31 之前，"在水里"是一个布尔，而它被当成**移动的开关**：不满足就
    /// 一步都走不了。那条开关把"上岸"变成了"不可能"，而陆地演化需要的
    /// 是"很贵但可行"。于是这个比例被拿去定价：
    ///
    /// ```text
    /// 移动成本 = move_cost × (1 + LAND_COST × (1 - 水占比))
    /// ```
    ///
    /// 全在水下 → 原价；全在岸上 → 4 倍（`LAND_COST = 3`）。
    ///
    /// 用 `cells.iter().any(...)` 找每列最高的那一格，而不是先建一张
    /// `tops` 表：身体最多 24 格，O(格数²) 是 576 次比较，比一次
    /// `Vec` 分配便宜。
    fn submerged_frac(world: &World, width: i32, cells: &[(i32, i32)]) -> f32 {
        if cells.is_empty() {
            return 0.0;
        }
        let (mut wet, mut total) = (0usize, 0usize);
        for &(x, y) in cells {
            // 只数每列最高的那一格——同一列里更低的那几格不影响露不露头。
            if cells.iter().any(|&(ox, oy)| ox == x && oy < y) {
                continue;
            }
            total += 1;
            let xx = if width > 0 { x.rem_euclid(width) } else { x };
            // 藻**算**水：它是水里的悬浮物，而它比水轻，会被一路顶到
            // 水面那一行。不算的话，鱼在藻层下面时头顶是藻、扫不到水，
            // 整批鱼会被判成"不在水里"——而它们明明就在水里。
            //
            // 只认藻不认气：气泡不是介质。
            if (0..y)
                .rev()
                .any(|yy| world.get(xx as isize, yy as isize) == MaterialKind::Water)
            {
                wet += 1;
            }
        }
        if total == 0 {
            0.0
        } else {
            wet as f32 / total as f32
        }
    }

    /// 身体是不是**整具都在水面之下**。
    ///
    /// **v0.31 之后它不再是移动的判据**——那条判据换成了按水占比定价
    /// （见 [`Self::submerged_frac`]）。它现在只回答三件事：
    /// 重力要不要接管、统计里算不算"脱水"、繁殖时这个位置够不够湿。
    ///
    /// ## 判据：每个 x 列的**头顶**，往上必须能扫到水
    ///
    /// 取身体在每一列里最高的那一格，从它上方往上扫：扫到 `Water` 就算这列
    /// 在水下，扫到空气 / 固体就算露出来了。**所有列都必须在水下**。
    ///
    /// 这条判据换过三版，每一版都对应一次真实的故障：
    ///
    /// **1. "任意一格的四邻有水"** —— 一条 12 格的鱼可以整条浮在水面之上，
    /// 只有尾巴尖蹭到一格水。现象：`stranded` 报到十几个，但移动从不被拦。
    ///
    /// **2. "身体表面的邻格里水占一半"** —— 好一些，但**窄鱼**的侧面几乎
    /// 总是水：一条 3 列宽的鱼哪怕头顶出水 4 格，侧面 6 个邻格全是水，
    /// 占比照样过半。现象：鱼全都浮在水面那一层，`stranded` 高达 60%。
    ///
    /// **3. 看头顶**（现在这条）—— 露不露出来，看的是头顶，不是侧面。
    ///
    /// 附带一条：**藻算水，气体不算**。藻是水里的悬浮物，而它比水轻，
    /// 会被一路顶到水面那一行；不认它的话，鱼在藻层下面时头顶是藻、
    /// 扫不到水，整批鱼会被判成"不在水里"。气体（气泡）不是介质。
    ///
    /// 判据要求头顶至少在水面下 1 格（身体顶部那格本身可以是水面那一行，
    /// 因为那格水被身体借走了，向上扫就是空气）。所以投放时要把身体
    /// 往下压一格，见 [`stock_surface`]。
    fn touches_water(world: &World, width: i32, cells: &[(i32, i32)]) -> bool {
        // 用比例实现，保证两条判据**永远同源**。
        //
        // 早先它是独立实现（提前 `return false`），而比例是另一份代码——
        // 两份判据迟早会漂开，而"哪一列露头算不算数"这种问题一旦漂开，
        // 症状是"重力在拉它、移动却以为它还在水里"。
        // 全在水下时比例恰好是 1.0（整数除法得 `total/total`），
        // 所以 `>= 1.0` 和"每一列都在水下"严格等价。
        Self::submerged_frac(world, width, cells) >= 1.0
    }

    /// 这个有机体的身体能不能完整落在这个位置。
    ///
    /// 比 [`Self::can_place`] 多两条：**水格上方必须有空气**（子代出生时
    /// 那一格的水要被推走，见 [`displace_fluid`]），以及**子代必须站得住**
    /// （见 [`Self::can_stand`]）。
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
        // **暂时回到"整具身体都在水下"。**
        //
        // v0.44 试过换成 `can_stand`（够得着水 **或** 脚下有实体），
        // 单元测试全绿，而生态里炸了：
        //
        //   | | 基线（本判据） | can_stand |
        //   |---|---|---|
        //   | 悬空个体 | **0** | **887** |
        //   | 动物数 | 482 | **1230** |
        //
        // 病根**不在这道门**：陆地生物死后，尸体在原地烂成水
        // （`tick_organics`），而**身体会挡住水不让它落下来**——于是
        // 水托着身体、身体托着水，形成悬空水柱；水柱里能出生，就自持了。
        //
        // 也就是说：先得回答"尸体的水该去哪儿"，这道门才谈得上放宽。
        // 详见 `阶段目标.md` 的 M16。
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
        let kind = world.get(x, y);
        if !matches!(kind, MaterialKind::Remains | MaterialKind::Corpse) {
            return false;
        }
        let Some(idx) = self.organisms.iter().position(|o| o.id == id) else {
            return false;
        };
        let gain = world.flags(x, y) as u32;
        // 上限要把**两个池子**都算上：能量满了但生物量还有空位时，
        // 这口东西照样吃得下（进的是结构）。只算能量的话，
        // 一个吃饱的老个体会对着一具肥尸体干瞪眼。
        let org = &self.organisms[idx];
        let room = org.max_energy().saturating_sub(org.energy)
            + org.max_structure().saturating_sub(org.structure);
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
                    MaterialKind::Corpse => MaterialKind::Water,
                    _ => MaterialKind::Empty,
                },
            );
        } else {
            // 留一部分在尸体上，下一顿再来。
            world.set_flags(x, y, (gain - taken) as u8);
        }
        self.organisms[idx].absorb(taken);
        // 食腐的判据是**吃掉的是尸体**，不是碎屑。两者在 `eaten` 里同值，
        // 但一个是"捡底货"、一个是"吃别人"，生态含义完全不同。
        if kind == MaterialKind::Corpse {
            self.stats.scavenged += 1;
        }
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
        let cells = genome.body.develop();
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
    // **主方向仍然是 argmax**，和 M2a 之前一模一样。
    //
    // 这一点是刻意的：`handcrafted_fish_genome` 里那个 0.7 的巡航偏置、
    // 那一堆 `connect(IN_BLOCKED_*, OUT_*, 10.0)` 都是按"四选一取最大"
    // 调出来的。改成"水平垂直各自判阈值"会让它们全部失效，需要重新调参——
    // 而那是 M2b 之后、身体描述稳定下来才该做的事。
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
    let (_, dx, dy) = best?;

    // **次方向**：与主方向垂直的那一对里，如果也超过阈值，就斜着走。
    //
    // 于是"斜向"只在"垂直方向也足够强"时才发生——平地上巡航仍然是直线，
    // 而在 45° 的坡上（水平一格 + 垂直一格正好是坡度 1），趋光信号会
    // 自然地把次方向推过阈值，身体就顺着坡上去了。
    //
    // 这就是"顺坡移动不需要贴坡逻辑"的实现：**坡度 ≤ 1 + 允许斜向**。
    let (ax, ay, second) = if dx != 0 {
        let up = out[OUT_UP];
        let down = out[OUT_DOWN];
        if up >= down {
            (0, -1, up)
        } else {
            (0, 1, down)
        }
    } else {
        let left = out[OUT_LEFT];
        let right = out[OUT_RIGHT];
        if left >= right {
            (-1, 0, left)
        } else {
            (1, 0, right)
        }
    };
    if second > MOVE_THRESHOLD {
        Some((dx + ax, dy + ay))
    } else {
        Some((dx, dy))
    }
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

/// 浮游藻的基因组：**只繁殖，什么都不做**。
///
/// 没有方向偏置，所以 `pick_direction` 永远返回 `None`——藻不动。
/// 这不是"忘了给它行为"，是它就该静止：藻是浮游植物，没有运动器官。
///
/// 它照样会走 `reproduce`（分裂），只不过藻走的是 `tick_plankton` 里
/// 那条更便宜的路径——见 `LifeSim::split_plankton`。
pub fn plankton_genome() -> Genome {
    let mut g = Genome::blank();
    g.weights[Genome::b_o(OUT_REPRODUCE)] = 2.0;
    g
}

/// 固着生物的基因组：**不动，只繁殖**。
///
/// 和 [`plankton_genome`] 是同一个东西，分开写是因为用途不同——
/// 藻是单格，海草是多格，而"为什么它不动"这个理由在两边不一样：
/// 藻没有运动器官，海草**扎根**在海底。
pub fn rooted_genome() -> Genome {
    let mut g = Genome::blank();
    g.weights[Genome::b_o(OUT_REPRODUCE)] = 2.0;
    g
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
    // **巡航偏置 0.6，不是 1.0。**
    //
    // `pick_direction` 取"超过阈值的最大值"，所以巡航偏置就是**垂直方向的
    // 门槛**：垂直信号得比它大才会被选中。给 1.0 时门槛是 `tanh(1.0) ≈ 0.76`，
    // 于是趋光只能在很小的一段深度里赢——实测垂直移动只占全部移动的 3.5%，
    // 而且**上浮是 0 次**（鱼能下潜到 162 就再也够不到需要上浮的深度）。
    //
    // 它还决定**死区有多宽**：垂直信号只在 `light` 偏离设定点超过
    // `tanh(bias)/gain` 时才赢，所以死区 ≈ `2 × tanh(bias) / gain`（光），
    // 乘 `LIGHT_DEPTH` 就是深度。
    //
    // **但不能低于 0.7。** 试过 0.35，结果 85% 的 tick 里鱼根本不选方向：
    // `tanh(0.35) ≈ 0.34` 只比 `MOVE_THRESHOLD`（0.15）高一倍，而隐藏层的
    // 突变噪声可以贡献 ±0.5——噪声直接把巡航偏置压到阈值以下。
    // 现象是"鱼全都不动"，看起来像卡死，实际是**信号被噪声吃掉**。
    //
    // 0.7 → 死区约 18 格，`tanh(0.7) ≈ 0.60`，两倍于阈值、也高于噪声底。
    g.weights[Genome::b_o(OUT_RIGHT)] = 0.7;
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
    // **垂直方向给得更小（0.8 vs 水平 1.5），这是被实测逼出来的。**
    //
    // 藻比水轻，会被一路顶到水面那一行，所以"食物在上方"这个信号在浅水里
    // 恒为真。给 1.5 时它把趋光的下潜信号整个抵消掉——鱼被食物钉在水面，
    // 垂直方向一辈子不动，看起来就是"只沿 X 轴游"。
    //
    // 0.8 让它只起"微调"作用：食物确实在下面时才值得为它改变深度，
    // 而"上方有藻"这种常态不再能压过趋光。
    g.connect(IN_FOOD_DIR_Y, OUT_DOWN, 0.8);
    g.connect(IN_FOOD_DIR_Y, OUT_UP, -0.8);

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

    // --- 趋光性：瞄准一条"等光线" ----------------------------------------
    //
    // **这是二维移动的全部来源。**
    //
    // 在此之前垂直方向只有一路负向抑制（`OUT_UP = 1.2 − 3×光`），它在浅水里
    // 恒为负；而 `OUT_DOWN` 根本没有驱动，恒为 0。于是"往上是负、往下是零、
    // 往右是 +1"——鱼只有两个方向可选，看起来就是**只沿 X 轴移动**。
    //
    // 现在写成两条对称的**正向**驱动，交点在 `LIGHT_SETPOINT`：
    //
    //     光 > 设定点 → 想往下（太亮了，往深处走）
    //     光 < 设定点 → 想往上（太暗了，晒不到）
    //     光 = 设定点 → 垂直方向归零，专心巡航
    //
    // 增益必须压过巡航偏置，理由见 `PHOTO_GAIN` 的注释。
    g.weights[Genome::b_o(OUT_DOWN)] = -PHOTO_GAIN * LIGHT_SETPOINT;
    g.connect(IN_LIGHT, OUT_DOWN, PHOTO_GAIN);
    g.weights[Genome::b_o(OUT_UP)] = PHOTO_GAIN * LIGHT_SETPOINT;
    g.connect(IN_LIGHT, OUT_UP, -PHOTO_GAIN);
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
/// 从这一列的**第一格水**开始，往下 `need` 格是不是全是水（气泡也算）。
///
/// 返回那一格水的 y。找不到返回 `None`。
///
/// 为什么不是"第一格水在哪"：新地形是一半陆地一半海，靠岸那一带只有
/// 几格深，身体放下去下半截插进沙里，`spawn_injected` 直接 `Blocked`——
/// 而调用方看到的是"要 24 条只放下 9 条"，一点线索都没有。
fn water_top(world: &World, x: i32, need: i32) -> Option<i32> {
    (0..world.height() as i32).find(|&y| {
        (y..y + need).all(|yy| {
            matches!(
                world.get(x as isize, yy as isize),
                MaterialKind::Water | MaterialKind::Gas
            )
        })
    })
}

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
    // 倾向按**长出来的身体**决定，不按体型表：形态发生模式下
    // "这是不是捕食者"只有长完才知道。
    let tactic_of = |cells: &[Cell]| {
        if cells.iter().any(|c| c.organelles.has(STING)) {
            Tactic::Hunt
        } else {
            Tactic::Ignore
        }
    };

    // 预扫一遍：哪些列的水柱够深。
    //
    // `MIN_STOCK_DEPTH` 取 6：最矮的手工体型是 2 格高，加两格余量。
    // 用固定的下限而不是每具身体各算一遍，是因为这一步只为"有没有海"，
    // 具体放不放得下由下面 `water_top(need)` 再验一次。
    const MIN_STOCK_DEPTH: i32 = 6;
    let candidates: Vec<i32> = (0..width as i32)
        .filter(|&x| water_top(world, x, MIN_STOCK_DEPTH).is_some())
        .collect();
    if candidates.is_empty() {
        return ids;
    }

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
        // **身体描述无条件设成 `Body`，不只是形态发生模式。**
        //
        // 这里踩过一个坑：早先写的是 `morph.then(|| Body::from_plan(plan))`，
        // 于是 `morph == false` 时 `genome.body` 留着 `Genome::blank()` 的
        // 那个"一格只有线粒体"的默认值。亲代身体是 10 格，**子代却退化成 1 格**——
        // 实测平均体型从 7.8 掉到 8.8（一半是 10 格、一半是 1 格），
        // 而画面上看起来只是"鱼越来越小"。
        let mut body = crate::morph::Body::from_plan(plan);
        if morph {
            // 形态发生模式：给身体一点**结构**变异，第一代就有可选的差异。
            // 这和下面那份权重抖动是两件事——一个改形状，一个改行为。
            body.mutate(
                &mut Rng::new(0xB0D7_0000 ^ (k as u64).wrapping_mul(0x9E37_79B9)),
                2,
            );
        }
        let cells = body.develop();

        // 每个个体都轻微不同：给自然选择一点原材料。
        //
        // 全都一模一样的话，第一代没有任何可选的差异，种群只能靠"繁殖时突变"
        // 慢慢产生变异——那要等很多代。播种时给一点抖动，选择从第一 tick 就开始。
        // 幅度刻意压小（sigma 0.12）：大一点就会把手工设计的"会游、会吃"打散，
        // 鱼连基本行为都没有，选择也就无从谈起。
        // 没有移动细胞的身体是**固着的**（海草）。给它一份"不动、只繁殖"
        // 的基因组，而不是鱼的——鱼的那份里有一整套趋光和巡航权重，
        // 装进一株海草身上会让它试图游走，然后每一格都被水陆判据拦住。
        let mut genome = if cells.iter().any(|c| c.kind == CellType::Mover) {
            handcrafted_fish_genome(tactic_of(&cells))
        } else {
            rooted_genome()
        };
        genome.mutate(
            &mut Rng::new(0xF15E_0000 ^ (k as u64).wrapping_mul(0x9E37_79B9)),
            0.35,
            0.12,
        );
        genome.body = body;

        // 身体在 x 方向的左右伸出量，用来把锚点夹在界内。
        let reach_left = cells.iter().map(|c| -c.offset.0).max().unwrap_or(0) as i32;
        let reach_right = cells.iter().map(|c| c.offset.0).max().unwrap_or(0) as i32;

        // 身体最上面那一格压在水面**下**一格：生产者要光，但**不能露头**。
        //
        // `+1` 是必需的，不是保守：`touches_water` 要求每列的头顶向上能扫到水，
        // 而身体最上面那格的水被借走变成了 `Empty`——如果它就落在水面那一行，
        // 向上扫到的第一格就是空气，判据直接失败。实测贴着水面投放时，
        // 整批鱼在出生的第一个 tick 就被判成"不在水里"。
        let top = cells.iter().map(|c| c.offset.1).min().unwrap_or(0) as i32;
        // 水柱至少要有这么深才值得试。太浅的列放下去下半截插进沙里，
        // `spawn_injected` 返回 `Blocked`，这条鱼就被悄悄丢了。
        let need = (cells.iter().map(|c| c.offset.1).max().unwrap_or(0) as i32 - top + 1) + 2;

        // **从候选列里挑**，不再直接按 `k * step` 取。
        //
        // 早先的写法是"第 k 条分到第 k*step 列，那列没水就 continue"。
        // 在"一半陆地一半海"的地形里，这等于**一半的鱼被丢掉**：实测
        // `--fish 24` 只放下 9 条，而且丢掉的总是靠岸那几列，于是
        // "水面看起来有一段没鱼"。
        let base = k * candidates.len() / count.max(1);
        let mut placed = false;
        for t in 0..candidates.len() {
            let x = candidates[(base + t) % candidates.len()]
                .clamp(reach_left, width as i32 - 1 - reach_right);
            let Some(y) = water_top(world, x, need) else {
                continue;
            };
            let anchor = (x, y - top + 1);
            // 启动能量按**实际长出来的格数**算，不是按体型表的格数——
            // 形态发生模式下这两者可能不一样，而"够不够活到能繁殖"只看前者。
            let n = cells.len().max(1) as u32;
            let energy = energy.max(crate::life::BIRTH_ENERGY_PER_CELL * n * 6 / 5);
            let mut org = Organism::new(0, anchor, cells.clone(), genome.clone());
            org.energy = energy;
            if let Ok(id) = sim.spawn_injected(world, org) {
                ids.push(id);
                placed = true;
                break;
            }
        }
        let _ = placed;
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

    /// 深池：64 宽、256 高，水面在 y=8，底下 248 格全是水。
    ///
    /// `pool()` 只有 30 格深，而阳光的衰减尺度是 180 格——在 30 格深的水里
    /// 光还有 0.83，"深水"根本不存在。这不是 bug，是新光场**正确**了：
    /// 30 格深的池子确实很亮。所以"深水饿死"这一类测试必须换一个真的够深的世界，
    /// 否则它测到的是"旧公式碰巧给出的小数字"。
    fn deep_pool() -> World {
        let mut w = World::new(64, 256).unwrap();
        w.fill_rect(0, 252, 64, 4, MaterialKind::Stone);
        w.fill_rect(0, 8, 64, 244, MaterialKind::Water);
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
    /// **水的守恒量**：世界里的水 + 活细胞借走的水 + 尸体欠着的水。
    ///
    /// 早先这里写的是 `水 + 占用格数 + 碎屑 + 尸体`，那量的是**物质**而不是水。
    /// 两者在"所有细胞都出生在水里"时恰好相等，而 v0.31 之后身体可以出生在
    /// **空气**里——它占的那格是 `Empty`，凭空多出一格物质，于是这个和会 +1。
    /// 实测 600 tick 的繁殖压力测试在第 162 tick 报 +2，查下来是两格
    /// 空气里出生的细胞，**水一滴没漏**。
    ///
    /// 正确的判据只数"借过水的格"（`Cell::wet`）：尸体一定是湿的
    /// （`bury` 给干细胞写的是 `Remains`），所以 `Corpse` 的格数就是
    /// 欠着的水。碎屑和空气里出生的细胞都不进这个账。
    fn water_volume(world: &World, sim: &LifeSim) -> usize {
        let borrowed: usize = sim
            .organisms()
            .iter()
            .flat_map(|o| &o.cells)
            .filter(|c| c.wet)
            .count();
        world.count_of(MaterialKind::Water) + borrowed + world.count_of(MaterialKind::Corpse)
    }

    #[test]
    fn spawn_claims_every_cell() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 关掉播种：这条测的是几何，不是生态。藻一长出来占格数就变了。
        sim.set_food_growth(false);
        let org = LifeSim::simple_fish((20, 35), Genome::blank(), 100);
        let id = sim.spawn_injected(&mut w, org).unwrap();
        assert_eq!(w.occupied_count(), 3);
        assert_eq!(w.owner(20, 35).organism, id);
        assert_eq!(w.owner(21, 35).organism, id);
        assert_eq!(w.owner(20, 34).organism, id);
    }

    #[test]
    fn spawn_rejects_occupied_and_out_of_bounds() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 35), Genome::blank(), 10))
            .unwrap();
        let again = sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 35), Genome::blank(), 10));
        assert_eq!(again.unwrap_err(), SpawnError::CellOccupied);

        let edge = sim.spawn_injected(&mut w, LifeSim::simple_fish((0, 0), Genome::blank(), 10));
        assert_eq!(edge.unwrap_err(), SpawnError::OutOfBounds);
    }

    #[test]
    fn starvation_turns_the_body_into_remains() {
        let mut w = deep_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // 生产者放进深水（光很弱）+ 没有嘴 = 必然饿死。
        // 200 格深：阳光在 180 格处归零，这一格是**真的**黑的。
        let org = LifeSim::simple_fish((20, 200), Genome::blank(), 6);
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
        let mut w = deep_pool();
        let mut sim = LifeSim::new(&mut w);
        // 这条测的是能量账，不是藻席。海底长出来的藻会把"残骸格数"搅乱，
        // 所以关掉它——不是因为它不可信，是因为它属于另一个测试。
        sim.set_food_growth(false);
        sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 200), Genome::blank(), 120))
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
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 35), Genome::blank(), 50))
            .unwrap();

        let before = sim.audit(&w);
        assert!(sim.eat_remains(&mut w, id, 22, 25));
        // 这里吃的是**天然碎屑**（`Remains`），不是尸体：它不是从身体借来的，
        // 吃掉之后只留下空位，不还水。尸体才还水。
        assert_eq!(w.get(22, 25), MaterialKind::Empty);
        // **一部分进了生物量。** 判据要看两个池子的和——只断言 `energy`
        // 会把"1/8 转成组织"当成"少了 3 点能量"。
        let org = sim.organism(id).unwrap();
        assert_eq!(org.energy + org.structure, 50 + REMAINS_ENERGY);
        assert!(org.structure > 0, "吃进来的东西应当有一部分变成身体");
        let after = sim.audit(&w);
        assert_eq!(after.accounted(), before.accounted(), "进食只是搬运");
    }

    #[test]
    fn sunlight_feeds_producers_only_in_shallow_water() {
        // 必须用**深**池：光衰减尺度是 180 格，30 格深的池子里上下几乎一样亮。
        // 而且要放在水里——水面之上没有水，光合作用直接不启动。
        let mut w = deep_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let shallow = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((10, 12), Genome::blank(), 100))
            .unwrap();
        let deep = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((40, 200), Genome::blank(), 100))
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
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 35), Genome::blank(), 100))
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
        // 关掉播种：这条测的是几何，不是生态。藻一长出来占格数就变了。
        sim.set_food_growth(false);
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
        // 关掉播种：这条测的是几何，不是生态。藻一长出来占格数就变了。
        sim.set_food_growth(false);
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
        // 关掉播种：这条测的是几何，不是生态。藻一长出来占格数就变了。
        sim.set_food_growth(false);
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
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let before = water_volume(&w, &sim);
        let id = sim.spawn_injected(&mut w, swimmer((62, 40), 5000)).unwrap();

        for _ in 0..300 {
            sim.step(&mut w);
            assert_eq!(
                water_volume(&w, &sim),
                before,
                "环绕移动只能搬水，不能吞水"
            );
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
        let mut fish = LifeSim::simple_fish((20, 35), Genome::blank(), 0);
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
        let mut fish = LifeSim::simple_fish((20, 35), Genome::blank(), 0);
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
        sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 35), Genome::blank(), 100))
            .unwrap();
        assert!(sim.founder().is_some());
        assert_eq!(sim.gene_distance(), 0.0, "初代自己的距离必须是 0");
    }

    #[test]
    fn gene_distance_grows_as_mutations_accumulate() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.spawn_injected(&mut w, LifeSim::simple_fish((20, 35), Genome::blank(), 100))
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
        // **M2a 之后这条断言变了**：垂直方向仍然超过阈值，于是斜着走。
        // 主方向仍然是 argmax（`OUT_RIGHT` 赢了），但次方向够强就叠加。
        assert_eq!(
            pick_direction(&out),
            Some((1, -1)),
            "主方向 + 够强的次方向 = 斜向"
        );
        // 把垂直压回阈值以下，就回到纯水平巡航。
        out[OUT_UP] = 0.0;
        assert_eq!(pick_direction(&out), Some((1, 0)), "没有次方向时是纯水平");
    }

    #[test]
    fn diagonal_needs_a_second_axis_that_is_also_strong() {
        // 斜向不是白给的：次方向必须**自己也超过阈值**。
        //
        // 这条是"平地上巡航仍然是直线"的保证。如果斜向免费，所有生物都会
        // 退化成纯对角运动（8 向里只剩 4 个），而那会让"水平游"这个动作消失。
        let mut out = [0.0f32; OUTPUTS];
        out[OUT_RIGHT] = 0.9;
        out[OUT_DOWN] = MOVE_THRESHOLD * 0.5;
        assert_eq!(pick_direction(&out), Some((1, 0)), "次方向太弱不该斜");
        out[OUT_DOWN] = MOVE_THRESHOLD + 0.05;
        assert_eq!(pick_direction(&out), Some((1, 1)), "次方向够强就斜");
    }

    #[test]
    fn diagonal_cannot_slip_through_a_corner() {
        // 一个"对角台阶"：目标格空着，但两个正交邻格都是墙。
        //
        //       x=30 31 32
        // y=40   .   .  W      ← (32,40) 是墙
        // y=41   .   W  .      ← (31,41) 是墙，(32,41) 空
        //
        // 身体在 (31,40)，斜向右下想去 (32,41)。如果放行，它会"穿墙缝"——
        // 这是所有 8 向网格移动的经典 bug，症状极具欺骗性：墙看着完好，
        // 生物却过去了。
        let mut w = World::new(64, 64).unwrap();
        w.fill_all(MaterialKind::Water);
        w.set(32, 40, MaterialKind::Stone);
        w.set(31, 41, MaterialKind::Stone);

        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let mut org = Organism::new(
            0,
            (31, 40),
            vec![Cell::new(CellType::Mover, (0, 0))],
            forward_genome(),
        );
        org.energy = 10_000;
        let id = sim.spawn_injected(&mut w, org).unwrap();
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();

        assert!(
            !sim.try_move(&mut w, i, 1, 1, MoveKind::Swim),
            "斜向穿过了对角缝"
        );

        // 对照：把 (31,41) 打开，同一个斜向就该成功。
        // 没有这条对照，上面那句可能只是"斜向完全不能用"。
        w.set(31, 41, MaterialKind::Water);
        assert!(
            sim.try_move(&mut w, i, 1, 1, MoveKind::Swim),
            "对角缝打开之后斜向应当能过"
        );
    }

    #[test]
    fn a_caged_fish_accumulates_stuck() {
        // `IN_STUCK` 的来源。没有它，"撞墙了"这件事对基因组是不可见的——
        // 它只知道"前方被挡"，不知道"我已经卡了 6 个 tick"。
        let mut w = World::new(64, 64).unwrap();
        w.fill_all(MaterialKind::Water);
        // 把身体四周砌死，只留它自己那一格。
        for (dx, dy) in [(1isize, 0isize), (-1, 0), (0, 1), (0, -1)] {
            w.set(30 + dx, 40 + dy, MaterialKind::Stone);
        }

        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let mut org = Organism::new(
            0,
            (30, 40),
            vec![Cell::new(CellType::Mover, (0, 0))],
            forward_genome(),
        );
        org.energy = 10_000;
        let id = sim.spawn_injected(&mut w, org).unwrap();

        for _ in 0..6 {
            sim.step(&mut w);
        }
        let stuck = sim.organism(id).unwrap().stuck;
        assert!(stuck >= 5, "被围住的鱼应当累计 stuck，实际 {stuck}");
    }

    #[test]
    fn recursion_feeds_the_previous_output_back_in() {
        // `IN_PREV_*` 必须是**上一 tick 的输出**，不是零。
        // 这条钉的是"网络真的有记忆"——它是"撞墙后转向"能存在的前提。
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim.spawn_injected(&mut w, swimmer((10, 40), 5_000)).unwrap();

        sim.step(&mut w);
        let after_first = sim.organism(id).unwrap().last_out;
        sim.step(&mut w);

        let org = sim.organism(id).unwrap();
        // 第二 tick 的输入里，`IN_PREV_*` 应当等于第一 tick 的输出。
        assert_eq!(org.last_inputs[IN_PREV_LEFT], after_first[OUT_LEFT]);
        assert_eq!(org.last_inputs[IN_PREV_RIGHT], after_first[OUT_RIGHT]);
        assert_eq!(org.last_inputs[IN_PREV_UP], after_first[OUT_UP]);
        assert_eq!(org.last_inputs[IN_PREV_DOWN], after_first[OUT_DOWN]);
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
        // 关掉播种：这条测的是几何，不是生态。藻一长出来占格数就变了。
        sim.set_food_growth(false);
        sim.spawn_injected(&mut w, swimmer((10, 40), 500)).unwrap();

        for _ in 0..50 {
            sim.step(&mut w);
            assert_eq!(w.occupied_count(), 2, "身体必须始终是完整的两格");
        }
    }

    #[test]
    fn being_born_borrows_water_and_dying_returns_it() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 关掉播种：这条测的是几何与体积账，不是生态。
        sim.set_food_growth(false);
        let before = water_volume(&w, &sim);
        let id = sim.spawn_injected(&mut w, swimmer((10, 40), 5000)).unwrap();

        // 出生"借走"两格水：水位会掉两格，但总量守恒。
        assert_eq!(
            w.count_of(MaterialKind::Water),
            before - 2,
            "两格身体借走两格水"
        );
        assert_eq!(water_volume(&w, &sim), before, "借不是丢");

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
            assert_eq!(water_volume(&w, &sim), before, "游动只能搬水，一格都不能少");
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
    fn a_body_larger_than_sixteen_still_swims() {
        // v0.26 拆掉"水是墙"之后的反向断言。
        //
        // 旧代码里 `passable()` 对水是 `body <= SMALL_BODY_MAX`（16），
        // 所以 17 格以上的身体**进不了水**：出生时挤开的那一小块空腔之外
        // 全是固体，它一格也走不动，然后被重力拉到海底饿死。
        // 玩家看到的是"细胞数过多无法在水中浮起"。
        //
        // 现在这条测试盯三件事：它真的动了、身体没散、水没少。
        // 第三条最容易漏——多格身体顶开水时的 `vacated` 配对写错，
        // 水会一格一格地蒸发，而画面上什么都看不出来。
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        // 关掉藻：藻是可以穿过去的，长在这条鱼前面会让它走起来，
        // 于是测到的就不是"水挡住了大生物"。
        sim.set_food_growth(false);

        // 18 格：比旧上限 16 多两格，正好是当年被挡住的那一类。
        let cells: Vec<Cell> = (0..18).map(|i| Cell::new(CellType::Mover, (i, 0))).collect();
        let mut big = Organism::new(0, (10, 40), cells, forward_genome());
        // 18 个移动细胞每 tick 要 72 维持，给少了它会在跑满 10 tick 之前先饿死，
        // 于是测到的是"饿死"而不是"能不能游"。
        big.energy = 20_000;

        let id = sim.spawn_injected(&mut w, big).unwrap();
        let start = sim.organism(id).unwrap().anchor;
        let volume = water_volume(&w, &sim);

        for _ in 0..10 {
            sim.step(&mut w);
        }

        let org = sim.organism(id).expect("18 格的身体不该在这 10 tick 里死掉");
        assert_ne!(org.anchor, start, "18 格的身体一格也没动——水又变成墙了");
        assert_eq!(w.occupied_count(), 18, "身体散架了");
        assert_eq!(water_volume(&w, &sim), volume, "水少了：顶开水的时候漏了");
        assert!(sim.audit(&w).is_balanced(), "能量账不平");
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
            // 关掉播种：这条测的是几何与体积账，不是生态。
            sim.set_food_growth(false);
            let volume = water_volume(&w, &sim);
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
                    water_volume(&w, &sim),
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
        // 关掉播种：这条测的是几何与体积账，不是生态。
        sim.set_food_growth(false);
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
    // 浮游藻
    // -----------------------------------------------------------------------

    /// 藻是**生物**：播种之后世界里多出若干个单格生产者。
    #[test]
    fn plankton_is_seeded_as_single_cell_producers() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let mut material = crate::sim::MaterialSim::new();
        for _ in 0..600 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        assert!(
            sim.stats().plankton > 5,
            "水里应当长出藻，实际 {} 个",
            sim.stats().plankton
        );
        // 每一个藻都必须是**一格生产者**——那是它的定义，不是巧合。
        for org in sim.organisms() {
            if org.is_plankton() {
                assert_eq!(org.cells.len(), 1);
                assert_eq!(org.cells[0].kind, CellType::Producer);
            }
        }
        assert!(sim.audit(&w).is_balanced(), "播种不能破坏能量账");
    }

    /// 藻**只出现在水里**。
    ///
    /// 这条是"藻不再是材质"之后仍然要守的不变量：播种只在 `Water` 格上做，
    /// 而藻自己不动，所以它不可能跑到空气里。如果哪天它跑出去了，
    /// 说明有人给了它一条不该有的移动通路。
    #[test]
    fn plankton_never_ends_up_in_the_air() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let mut material = crate::sim::MaterialSim::new();
        for _ in 0..600 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        let mut found = 0;
        for org in sim.organisms() {
            if !org.is_plankton() {
                continue;
            }
            found += 1;
            let (x, y) = org.anchor;
            // **不能断言"锚点那格现在是水"**：藻的身体占住了那一格，
            // 材质已经变成 `Empty`（水被借走了）。要验的是"它借的是一格水"，
            // 而那记在 `Cell::wet` 上。
            assert!(
                org.cells[0].wet,
                "藻在 ({x}, {y}) 出生时没占到水——它不该长在那儿"
            );
        }
        assert!(found > 0, "六百 tick 之后应当有藻");
    }

    /// 鱼**吃**藻：能量从藻转到鱼，藻消失，而且不留尸体。
    ///
    /// 这是"藻是生物"之后最要紧的一条：它把 `eat_remains`（吃材质）
    /// 和"吃活的东西"分开了。藻被吃掉不留尸体——留的话，一条鱼吃藻
    /// 会在原地吐出一具 1 格尸体，尸体烂成水，等于"吃藻会造水"。
    #[test]
    fn a_fish_eats_a_plankton_cell() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // 鱼在 (20,40)，嘴朝右；藻放在 (22,40) —— 嘴的右邻。
        let mut alga = Organism::new(
            0,
            (22, 40),
            vec![Cell::new(CellType::Producer, (0, 0))],
            plankton_genome(),
        );
        alga.energy = 80;
        let prey = sim.spawn_injected(&mut w, alga).unwrap();

        let mut fish = LifeSim::simple_fish((20, 40), forward_genome(), 500);
        fish.facing = crate::life::Facing::RIGHT;
        let id = sim.spawn_injected(&mut w, fish).unwrap();
        let before = sim.audit(&w);
        let fish_before = sim.organism(id).unwrap().energy;

        for _ in 0..4 {
            sim.step(&mut w);
        }

        assert!(
            sim.organism(prey).is_none(),
            "藻应当被吃掉（它只有一格，吃完就没了）"
        );
        let fish_after = sim.organism(id).unwrap().energy;
        assert!(
            fish_after > fish_before - 20,
            "吃掉藻之后能量应当补回来：{fish_before} → {fish_after}"
        );
        // 不留尸体：世界里的尸体数不该因为"吃了一顿"而增加。
        assert_eq!(
            w.count_of(MaterialKind::Corpse),
            0,
            "藻被吞下去了，不该留下尸体"
        );
        let after = sim.audit(&w);
        assert!(after.is_balanced(), "差额 {}", after.leak());
        // 不能断言 `after <= before`：这 4 tick 里鱼的生产者细胞在晒光，
        // 注入是正常增长。要验的是"吃藻没有额外造能量"，而守恒断言
        // （`is_balanced`）已经覆盖了那一条。
        assert!(after.injected >= before.injected, "光注入不该倒退");
    }

    /// 嗅觉：闻得到水后面的食物，闻不到石头后面的。
    #[test]
    fn smell_goes_through_water_but_not_through_stone() {
        let mut w = pool();
        w.set(20, 45, MaterialKind::Remains);
        w.set_flags(20, 45, crate::life::REMAINS_ENERGY as u8);
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
        // 中性光照：趋光的权重是 ±12，全零输入等于"无光海底"，
        // 那会把 DOWN 直接按到 -1 饱和，再往上加什么都看不出来。
        inputs[IN_LIGHT] = LIGHT_SETPOINT;
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
    fn plankton_seeds_in_a_generated_sea() {
        let mut w = World::new(128, 128).unwrap();
        crate::worldgen::sea_and_islands(&mut w, &mut Rng::new(7));
        let mut sim = LifeSim::new(&mut w);
        let mut material = crate::sim::MaterialSim::new();

        for _ in 0..2000 {
            sim.step(&mut w);
            material.step(&mut w);
        }

        assert!(
            sim.stats().plankton > 10,
            "真实的海里应当长出藻，实际 {} 个",
            sim.stats().plankton
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
        // 关掉播种：这条测的是尸体腐烂，藻的尸体会混进 `count_of(Corpse)`。
        sim.set_food_growth(false);
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
        sim.set_food_growth(false);
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
        // 关掉播种：这条测的是尸体腐烂，藻的尸体会混进 `count_of(Corpse)`。
        sim.set_food_growth(false);
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
        let before = water_volume(&w, &sim);
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
                water_volume(&w, &sim),
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
        // **必须给一个中性的光照。** `IN_LIGHT` 的权重是 ±12（趋光是这套
        // 基因组里最强的信号），而全零输入意味着 light=0 —— 那在行为上
        // 等于"在无光的海底"，趋光会把垂直方向整个按死，
        // 测到的就不是"社交倾向"而是"趋光"。
        inputs[IN_LIGHT] = LIGHT_SETPOINT;
        inputs[IN_ORGANISM_DIR_Y] = 0.5; // 别人在下方

        let hunt = handcrafted_fish_genome(Tactic::Hunt).think(&inputs);
        assert!(
            hunt[OUT_DOWN] > hunt[OUT_UP],
            "有牙的应当往下（追）：down {} vs up {}",
            hunt[OUT_DOWN],
            hunt[OUT_UP]
        );

        let flee = handcrafted_fish_genome(Tactic::Flee).think(&inputs);
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
                w.set(20, 52, MaterialKind::Remains);
                w.set_flags(20, 52, crate::life::REMAINS_ENERGY as u8);
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
        // 只跑 `predate`，不跑整步：整步里猎物会晒到太阳、游走、被追丢，
        // 而这条测试要量的是**一口咬下去掉了多少**。掺进产能之后
        // `1000 - energy` 甚至会变成负数（它靠光合作用把血赚回来了）。
        let (w, mut sim, _hunter, soft) = hunter_and_prey(&crate::body::SILVERFISH);
        let soft_before = sim.organism(soft).unwrap().energy;
        for _ in 0..10 {
            sim.predate(&w);
        }
        let soft_loss = soft_before as i64 - sim.organism(soft).map_or(0, |o| o.energy) as i64;

        let (w2, mut sim2, _hunter2, hard) = hunter_and_prey(&crate::body::LEAFBACK);
        let hard_before = sim2.organism(hard).unwrap().energy;
        for _ in 0..10 {
            sim2.predate(&w2);
        }
        let hard_loss = hard_before as i64 - sim2.organism(hard).map_or(0, |o| o.energy) as i64;

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
    // 重力
    // -----------------------------------------------------------------------

    /// 放在半空的身体会**掉下来**，而不是原地冻结。
    ///
    /// 这条是玩家报的第三个 bug：把鱼放在看得见的地方（水面附近），
    /// 而水面附近往往是空气——于是它一动不动，像被钉住了。
    /// 根因是水陆判据拦住了**所有**移动，包括"被重力拉下去"。
    #[test]
    fn a_body_dropped_in_midair_falls() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // pool() 的水在 y=30..59，这条鱼放在 y=12 的空气里。
        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 12), Genome::blank(), 500))
            .unwrap();
        assert_eq!(sim.organism(id).unwrap().anchor.1, 12);

        for _ in 0..10 {
            sim.step(&mut w);
        }

        let y = sim.organism(id).unwrap().anchor.1;
        assert!(y > 15, "放在半空的身体应当往下掉，实际只到 y={y}");
        assert!(sim.fell_total() > 0, "下落要被数出来，否则看不出重力在工作");
    }

    /// 掉进水里就停，然后开始正常游。
    ///
    /// 这是上一条的另一半：只做"下落"不做"停"，鱼会一路穿过水面掉到海床。
    #[test]
    fn a_body_stops_falling_once_it_is_in_the_water() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 12), Genome::blank(), 500))
            .unwrap();

        // 掉 80 tick 足够穿过水面并沉进去。
        for _ in 0..80 {
            sim.step(&mut w);
        }

        let org = sim.organism(id).unwrap();
        let cells: Vec<(i32, i32)> = org.footprint().collect();
        assert!(
            LifeSim::touches_water(&w, org.width, &cells),
            "掉进水里之后应当算'在水里'，锚点在 y={}",
            org.anchor.1
        );
        // 而且不再继续往下掉（水面 30、海床 60，它该停在水体里）。
        assert!(
            org.anchor.1 < 58,
            "它不该一路穿过整个水体掉到海床上，实际 y={}",
            org.anchor.1
        );
    }

    /// 下落**不花能量**——重力不是鱼在使劲。
    ///
    /// 反过来说：如果下落扣能量，一条掉进深水的鱼会因为"下落太贵"
    /// 而卡在半空，而那正是这条规则要修的那个 bug 的另一个版本。
    #[test]
    fn falling_costs_no_energy() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 12), Genome::blank(), 500))
            .unwrap();
        // 把能量设成刚好 0：连一格都游不动，但重力照样该把它拉下来。
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();
        let drained = sim.organisms[i].energy;
        sim.organisms[i].energy = 0;
        sim.ledger.dissipated += drained as u64;

        // 只跑重力，不跑整步。
        //
        // 整步里这条鱼会被代谢判定饿死——它在水面之上的空气里，
        // 光合作用不启动（见 `SUBMERGED_MIN`），于是第一个 tick 就没了。
        // 而这条测试要问的是"下落花不花能量"，那是一个**纯粹的力学问题**，
        // 掺进代谢只会让它变得依赖别的规则。
        for _ in 0..5 {
            sim.apply_gravity(&mut w);
        }

        let y = sim.organism(id).unwrap().anchor.1;
        assert!(y > 12, "没有能量也该被重力拉下来，实际 y={y}");
        assert_eq!(sim.organism(id).unwrap().energy, 0, "下落不该花能量");
    }

    /// 落在陆地上的身体**搁浅**：动不了，而且会饿死。
    ///
    /// 这是"鱼离开水会死"该有的样子——比"悬在空中冻结"诚实。
    #[test]
    fn a_body_stranded_on_land_starves() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // 石头上方一格（pool 的石头从 y=60 开始，水面在 30）。
        // 先把水抽干，做成"一块露出水面的陆地"。
        w.fill_rect(0, 30, 64, 30, MaterialKind::Empty);
        // 能量给足：这条测的是"搁浅"，不是"多久饿死"。40 点撑不到
        // 它掉下去就先饿死了，而那会让断言看起来像"下落没生效"。
        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((20, 55), Genome::blank(), 300))
            .unwrap();

        // 它该停在石头上面（y≈57，身体三格），而不是穿进石头。
        for _ in 0..20 {
            sim.step(&mut w);
        }
        let y = sim.organism(id).unwrap().anchor.1;
        assert!(y < 60, "不该穿进石头，实际 y={y}");
        assert!(y >= 58, "它该一路掉到石头上面，实际 y={y}");

        // 维持成本照扣，于是它会饿死（300 点 ÷ 约 9/tick ≈ 33 tick）。
        for _ in 0..400 {
            sim.step(&mut w);
            if sim.organism(id).is_none() {
                break;
            }
        }
        assert!(sim.organism(id).is_none(), "搁浅的鱼应当饿死");
    }

    // -----------------------------------------------------------------------
    // 陆地：从"开关"改成"成本"（v0.31）
    // -----------------------------------------------------------------------

    /// 水占比 → 光合折扣的斜坡。
    #[test]
    fn photosynthesis_ramps_with_water_coverage() {
        assert_eq!(dampness(0.0), 0.0, "干透的格子不该有任何产出");
        assert_eq!(dampness(DRY_MIN), 0.0, "门槛上仍然是零");
        assert!(
            (dampness(0.2) - 0.5).abs() < 1e-6,
            "半湿应当在中间，实测 {}",
            dampness(0.2)
        );
        assert_eq!(dampness(SUBMERGED_MIN), 1.0, "到 SUBMERGED_MIN 应当满产");
        assert_eq!(dampness(0.9), 1.0, "泡在水里不该有额外加成");
    }

    /// 水占比按**列**数，不按格数。
    #[test]
    fn water_coverage_counts_columns_not_cells() {
        let w = pool(); // 水面 30
        let width = 8;

        // 整具身体在水下。
        let under: Vec<(i32, i32)> = vec![(20, 33), (21, 33), (20, 32), (21, 32)];
        assert_eq!(LifeSim::submerged_frac(&w, width, &under), 1.0);
        assert!(LifeSim::touches_water(&w, width, &under));

        // 一列露头：x=20 的头顶在 30（水面那一行被身体借走了，向上扫是空气），
        // x=21 还在水下。
        let half: Vec<(i32, i32)> = vec![(20, 30), (21, 31), (20, 31)];
        let f = LifeSim::submerged_frac(&w, width, &half);
        assert!((f - 0.5).abs() < 1e-6, "两列里一列在水下，实测 {f}");
        assert!(
            !LifeSim::touches_water(&w, width, &half),
            "有一列露头就不该算'整具都在水下'"
        );

        // 全在空气里。
        let air: Vec<(i32, i32)> = vec![(20, 10), (21, 10)];
        assert_eq!(LifeSim::submerged_frac(&w, width, &air), 0.0);
        assert!(!LifeSim::touches_water(&w, width, &air));
    }

    /// **v0.31 的核心行为变化**：半露出水面的身体**能游**，只是更贵。
    ///
    /// 旧代码在这里直接 `return false`（"身体不能整个离开水"那条硬判据），
    /// 于是"上岸"在物理上是不可能的——而陆地演化需要的是"很贵但可行"。
    #[test]
    fn a_body_with_its_head_out_can_still_swim_but_it_costs_more() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // anchor y=31：x=0 那列头顶在 30（露出来了），x=1 那列还在水下。
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((20, 31), Genome::blank(), 5_000),
            )
            .unwrap();
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();
        let base = sim.organisms[i].move_cost();
        let before = sim.organisms[i].energy;

        assert!(
            sim.try_move(&mut w, i, 1, 0, MoveKind::Swim),
            "半露出水面的身体应当还能游——旧的硬判据在这里会返回 false"
        );
        let spent = before - sim.organisms[i].energy;
        assert!(
            spent > base,
            "上岸走一步应当比在水里贵：付了 {spent}，水里是 {base}"
        );
        // 一半的列在水下 → 加价 (1 + LAND_COST × 0.5) 倍。
        let want = (base as f32 * (1.0 + LAND_COST * 0.5)) as u32;
        assert_eq!(spent, want, "加价幅度不对");
    }

    /// 站在陆地上要付**支撑成本**，按质量 × 露出比例算。
    ///
    /// 这一项和移动加价是两件事：那是"走一步多贵"，这是"站着不动也要付"。
    /// 只有前者的话，一条上岸的鱼可以一动不动地待着，搁浅于是变成一种
    /// 可行的休眠策略。
    #[test]
    fn standing_on_land_costs_extra_upkeep_per_unit_mass() {
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // 抽干水面以上的一切：石头从 y=60 开始，于是 0..60 是一整块陆地。
        w.fill_rect(0, 0, 64, 60, MaterialKind::Empty);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((20, 55), Genome::blank(), 5_000),
            )
            .unwrap();
        // 先让它掉到石头上（下落不花钱，所以能量在这一段不动）。
        for _ in 0..20 {
            sim.step(&mut w);
        }
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();
        let before = sim.organisms[i].energy;
        let upkeep = sim.organisms[i].upkeep();
        let mass = sim.organisms[i].mass();
        assert!(mass > 0, "这条测试要求身体有质量");

        sim.step(&mut w);

        let spent = before - sim.organism(id).unwrap().energy;
        let land = (mass as f32 * LAND_UPKEEP) as u32;
        assert!(land > 0, "3 格身体应当有非零的支撑成本");
        assert_eq!(
            spent,
            upkeep + land,
            "陆地支撑成本没算进去：付了 {spent}，维持成本 {upkeep}，陆地应当再加 {land}"
        );
    }

    /// 上岸能活多久：**身体越大，死得越快**。
    ///
    /// 这是 M5 的验收数字。判据不是"某条鱼死了"——那什么都说明不了，
    /// 因为搁浅的鱼本来就会饿死。判据是**大身体比小身体死得更快**：
    /// 那才说明 `mass` 项真的在起作用，而不是所有体型一视同仁地饿死。
    #[test]
    fn a_bigger_body_dies_faster_on_dry_land() {
        fn survival(cells: i16) -> u32 {
            let mut w = pool();
            // 抽干水面以上：石头从 y=60 开始，于是 0..60 是一整块干陆地。
            w.fill_rect(0, 0, 64, 60, MaterialKind::Empty);
            let mut sim = LifeSim::new(&mut w);
            sim.set_food_growth(false);
            let body: Vec<Cell> = (0..cells)
                .map(|i| Cell::new(CellType::Mover, (i, 0)))
                .collect();
            let mut org = Organism::new(0, (20, 50), body, Genome::blank());
            org.energy = 3_000;
            let id = sim.spawn_injected(&mut w, org).unwrap();
            let mut ticks = 0;
            while sim.organism(id).is_some() && ticks < 2_000 {
                sim.step(&mut w);
                ticks += 1;
            }
            ticks
        }

        let small = survival(3);
        let big = survival(12);
        assert!(
            small < 2_000 && big < 2_000,
            "两者都该饿死：小 {small} tick、大 {big} tick"
        );
        assert!(
            big < small,
            "大身体上岸该更贵：小 {small} tick、大 {big} tick"
        );
    }

    // -----------------------------------------------------------------------
    // 身体进光场（v0.33）
    // -----------------------------------------------------------------------

    /// **食腐要有利可图：尸体值的是身体，不是脂肪。**
    ///
    /// 早先 `bury` 只把 `energy` 写进尸体，于是饿死的身体留下一具 0 能量的
    /// 尸体——而饿死是主要死因。实测 61% 的尸体格能量为 0，全部尸体只占
    /// 账本注入的 0.18%，一格尸体够一个嘴活一个 tick：**食腐这个生态位
    /// 在数学上不存在**。
    ///
    /// 现在尸体的价值是 `energy + structure`，而 `structure` 是它活着的时候
    /// 从光合里攒下来的。这条测试钉的就是"把脂肪烧光之后，身体还值钱"。
    #[test]
    fn a_starved_body_still_leaves_an_edible_corpse() {
        let mut w = shallow_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((10, 12), Genome::blank(), 100),
            )
            .unwrap();
        // 晒一阵光：生物量从光合里长出来。
        for _ in 0..60 {
            sim.step(&mut w);
        }
        let org = sim.organism(id).expect("60 tick 还不该死");
        assert!(
            org.structure > 0,
            "晒了 60 tick 光，应当攒出生物量（structure = {}）",
            org.structure
        );

        // 把脂肪抽干再让它死：尸体仍然值钱。
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();
        sim.organisms[i].energy = 0;
        // 用 `MAX_AGE` 而不是 `u32::MAX`：代谢里会 `age += 1`，
        // 顶到上限会溢出（debug 下直接 panic）。
        sim.organisms[i].age = MAX_AGE;
        sim.step(&mut w);

        let mut best = 0u8;
        for y in 0..64isize {
            for x in 0..64isize {
                if w.get(x, y) == MaterialKind::Corpse {
                    best = best.max(w.flags(x, y));
                }
            }
        }
        assert!(
            best > 0,
            "饿死的身体也该留下有能量的尸体——它值的是身体，不是脂肪"
        );
    }

    // -----------------------------------------------------------------------
    // 繁殖门（v0.44 · 已回滚，见阶段目标 M16）
    // -----------------------------------------------------------------------

    /// 一片"岸"：左边是水，右边是平地。
    fn shore() -> (World, i32) {
        let mut w = World::new(120, 80).unwrap();
        w.fill_rect(0, 60, 120, 20, MaterialKind::Stone);
        w.fill_rect(0, 40, 20, 20, MaterialKind::Water);
        (w, 20)
    }

    /// **陆地繁殖还没打通，这条测试记的是"目标"，不是"现状"。**
    ///
    /// v0.44 把繁殖门从"整具身体都在水下"换成"够得着水 **或** 脚下有实体"，
    /// 单元测试全绿、这条也确实通过了（内陆植物能生）。**但生态里炸了**：
    /// 陆地尸体在原地烂成水，而身体挡着水不让它落下——水托身体、身体托水，
    /// 形成悬空水柱；水柱里能出生，于是自持。实测悬空个体 0 → 887、
    /// 动物数 482 → 1230。
    ///
    /// 所以判据已经回滚，这条先 `#[ignore]`。它同时是**下一轮的验收**：
    /// 等"尸体的水该去哪儿"有了答案，把它打开。
    #[test]
    #[ignore = "等 M16 的病根（尸体水 + 水被身体托住）修好"]
    fn a_land_plant_can_reproduce() {
        let (mut w, shore) = shore();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);

        let mut cells = Vec::new();
        for dx in 0..4i16 {
            cells.push(Cell::new(CellType::Producer, (dx, -1)));
        }
        for dx in 0..2i16 {
            cells.push(Cell::from_organelles(
                crate::organelle::Organelles(ROOT),
                (dx, 0),
            ));
        }
        let mut genome = handcrafted_fish_genome(Tactic::Ignore);
        genome.body = crate::morph::Body::from_cells(
            &cells
                .iter()
                .map(|c| (c.offset.0, c.offset.1, c.organelles.0))
                .collect::<Vec<_>>(),
        );
        let mut plant = Organism::new(0, (shore + 3, 59), cells, genome);
        plant.energy = 1_500;
        let id = sim.spawn_injected(&mut w, plant).unwrap();

        let mut born = 0;
        for _ in 0..400 {
            let before = sim.organisms().len();
            sim.step(&mut w);
            if sim.organisms().len() > before {
                born += 1;
            }
        }
        assert!(
            born > 0,
            "内陆的植物应当能繁殖（能量 {:?}、门槛 {:?}）",
            sim.organism(id).map(|o| o.energy),
            sim.organism(id).map(|o| o.reproduction_energy())
        );
    }

    // -----------------------------------------------------------------------
    // 停步（v0.44）
    // -----------------------------------------------------------------------

    /// **没有鞭毛就一步也走不了。**
    ///
    /// 原来是 `1 + movers/2`（下限 1）：任何身体每 tick 都能走一格。
    /// 而手工基因组给了 `OUT_RIGHT = 0.7` 的巡航偏置，于是工坊里拼出来的
    /// "植物"会一直往右爬——岸上每步 ×4，8 质量的身体每 tick 付 32 点，
    /// 比它光合挣的还多。症状是"能量在正常获取却一直掉"。
    #[test]
    fn a_body_without_flagella_does_not_move() {
        let (mut w, shore) = shore();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);

        // 基因组只留"想往右走"这一条：**不带繁殖**，否则它会立刻把能量
        // 花在生孩子上，而这条测试测的是"挪不挪窝"。
        let mut walker = Genome::blank();
        walker.weights[Genome::b_o(OUT_RIGHT)] = 0.7;

        // 生产者 + 根，一根鞭毛都没有。
        let cells = vec![
            Cell::new(CellType::Producer, (0, -1)),
            Cell::from_organelles(crate::organelle::Organelles(ROOT), (0, 0)),
        ];
        let mut org = Organism::new(0, (shore + 4, 59), cells.clone(), walker.clone());
        org.energy = 900;
        let id = sim.spawn_injected(&mut w, org).unwrap();
        assert_eq!(sim.organism(id).unwrap().speed(), 0, "没鞭毛速度应当是 0");

        // 对照组：同一具身体，多一格鞭毛。
        let mut with_flagellum = cells;
        with_flagellum[0] = Cell::new(CellType::Mover, (0, -1));
        let mut swimmer = Organism::new(0, (shore + 20, 59), with_flagellum, walker);
        swimmer.energy = 900;
        let sid = sim.spawn_injected(&mut w, swimmer).unwrap();
        assert_eq!(sim.organism(sid).unwrap().speed(), 1, "一格鞭毛 = 一格/tick");

        let before = sim.organism(id).unwrap().anchor;
        let s_before = sim.organism(sid).unwrap().anchor;
        for _ in 0..30 {
            sim.step(&mut w);
        }
        assert_eq!(
            sim.organism(id).map(|o| o.anchor),
            Some(before),
            "没鞭毛的身体不该挪窝"
        );
        assert_ne!(
            sim.organism(sid).map(|o| o.anchor),
            Some(s_before),
            "**有鞭毛的必须还能走**——否则这一条就把所有移动都关掉了"
        );
    }

    /// **画上去的有机物要带能量，而且要能吃到。**
    ///
    /// `world.set` 会把 `flags` 清零，而 `flags` 就是残骸/尸体的能量。
    /// 于是画笔画的碎屑是"能吃但吃了等于没吃"——用户把一具全身是嘴的
    /// 生物埋进自己画的碎屑堆里，它饿死，而画面上完全看不出原因。
    ///
    /// 两条一起断言：**能吃**（能量涨了）**且账平**（那 25 点记进了注入）。
    /// 只断言前者的话，"凭空造能量"会被漏掉。
    #[test]
    fn painted_remains_carry_energy_and_the_ledger_notices() {
        let mut w = shallow_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let before = sim.ledger().injected;

        // 模拟画笔：`world.set` 之后 flags 是 0。
        w.set(20, 20, MaterialKind::Remains);
        assert_eq!(w.flags(20, 20), 0, "`set` 应当把 flags 清零");

        let n = sim.adopt_organics(&mut w, &[(20, 20)]);
        assert_eq!(n, 1, "一格有机物应当被补上能量");
        assert_eq!(
            w.flags(20, 20),
            crate::life::REMAINS_ENERGY as u8,
            "补的应当就是标称能量"
        );
        assert_eq!(
            sim.ledger().injected,
            before + crate::life::REMAINS_ENERGY as u64,
            "凭空多出来的能量必须记账，否则 audit 会报漏"
        );

        // 已经带能量的不动（否则反复涂同一片会一直加能量）。
        let n2 = sim.adopt_organics(&mut w, &[(20, 20)]);
        assert_eq!(n2, 0, "已经有能量的不该再加一次");

        // 真的能吃。
        let cells = vec![Cell::new(CellType::Mouth, (0, 0))];
        let mut org = Organism::new(0, (20, 21), cells, Genome::blank());
        org.energy = 10;
        let id = sim.spawn_injected(&mut w, org).unwrap();
        let ok = sim.eat_remains(&mut w, id, 20, 20);
        assert!(ok, "画上去的碎屑应当能吃");
        let org = sim.organism(id).unwrap();
        assert!(
            org.energy + org.structure > 10,
            "吃到了应当有进账：{} + {}",
            org.energy,
            org.structure
        );
    }

    /// **干死的身体必须留下点东西。**
    ///
    /// 出生在空气里的身体一格水都没借过，所以它的死不该还水（`Cell::wet`）。
    /// 但早先 `bury` 把"不还水"和"不留尸体"写成了同一个 `if`，于是
    /// **往陆地上一放，鱼掉下来、挣扎两秒、凭空消失**——玩家看到的是 bug。
    ///
    /// 现在干细胞留 `Remains`：它是粉末（会掉到地上堆起来），也是
    /// `eat_remains` 的食物——陆地有机质的入口。
    #[test]
    fn a_body_that_dies_on_dry_land_leaves_remains() {
        let mut w = pool();
        // 抽干水面以上的一切，做成一块干地。
        w.fill_rect(0, 0, 64, 60, MaterialKind::Empty);
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((20, 55), Genome::blank(), 300),
            )
            .unwrap();

        for _ in 0..400 {
            sim.step(&mut w);
            if sim.organism(id).is_none() {
                break;
            }
        }
        assert!(sim.organism(id).is_none(), "陆地上没有水和光，它应当饿死");

        let mut remains = 0;
        for y in 0..64isize {
            for x in 0..64isize {
                if w.get(x, y) == MaterialKind::Remains {
                    remains += 1;
                }
            }
        }
        assert!(remains > 0, "干死的身体必须留下碎屑——早先它凭空消失了");
        assert!(remains <= 3, "最多留下身体那三格，实际 {remains}");
    }

    /// **根能让植物在水面之上继续光合，没有根就只能干等。**
    ///
    /// 这条测试钉的是"离水"这件事的机制，所以**只跑 `tick_hydration`**，
    /// 不跑 `step`——整步里重力会把身体拽回水里，而那就测不到"离水"了。
    #[test]
    fn a_root_refills_hydration_from_water_next_door() {
        // 一株竖着的生产者：锚点在水面之上 5 格，最下面一格贴在水面上。
        let plant = |root: bool| {
            let mut cells: Vec<Cell> = (0..4)
                .map(|dy| Cell::new(CellType::Producer, (0, dy)))
                .collect();
            if root {
                cells.push(Cell::from_organelles(
                    crate::organelle::Organelles::from_cell_type(CellType::Producer)
                        .with(ROOT),
                    (0, 4),
                ));
            } else {
                cells.push(Cell::new(CellType::Producer, (0, 4)));
            }
            Organism::new(0, (20, 25), cells, Genome::blank())
        };

        // 有根：根在 y=29，四邻里 (20,30) 是水 → 含水量一直满。
        let mut w = pool();
        let mut sim = LifeSim::new(&mut w);
        let id = sim.spawn_injected(&mut w, plant(true)).unwrap();
        assert_eq!(
            LifeSim::root_water(&w, sim.moisture(), sim.organism(id).unwrap()),
            Some(u8::MAX),
            "根在 y=29，正下方就是水，应当算贴水"
        );
        for _ in 0..40 {
            sim.tick_hydration(&w);
        }
        assert_eq!(
            sim.organism(id).unwrap().hydration,
            crate::life::HYDRATION_MAX,
            "根贴着水，含水量应当一直是满的"
        );

        // 没根：锚点所在的粗格（y=24..27）里没有水 → 每 tick 掉一格。
        let mut w2 = pool();
        let mut sim2 = LifeSim::new(&mut w2);
        let id2 = sim2.spawn_injected(&mut w2, plant(false)).unwrap();
        assert_eq!(
            LifeSim::root_water(&w2, sim2.moisture(), sim2.organism(id2).unwrap()),
            None,
            "没有根细胞，一滴水都够不到"
        );
        for _ in 0..40 {
            sim2.tick_hydration(&w2);
        }
        assert_eq!(
            sim2.organism(id2).unwrap().hydration,
            crate::life::HYDRATION_MAX - 40,
            "没有根，离水之后应当每 tick 掉一格"
        );
    }

    // -----------------------------------------------------------------------
    // 土壤湿度（v0.41）
    // -----------------------------------------------------------------------

    /// 受控平台：左边 20 列有水，右边一整块平地。
    ///
    /// 真实地形太吵（坡地上刚性身体放不下、内陆还有水洼），
    /// 测"多干就活不下去"必须先把地形噪声去掉。
    fn flat_shore() -> (World, i32) {
        // 800 宽而不是 400：横向是**环面**，400 宽的话"离岸 200 列"
        // 从另一边绕过去只有 180 列，测不到"超过跨度就干透"。
        let mut w = World::new(800, 100).unwrap();
        w.fill_rect(0, 60, 800, 40, MaterialKind::Stone);
        w.fill_rect(0, 40, 20, 20, MaterialKind::Water);
        (w, 20)
    }

    /// 一株"2 生产者 + 1 根"的小植物，站在平地上、根贴着土。
    fn potted_plant(anchor: (i32, i32)) -> Organism {
        let cells = vec![
            Cell::new(CellType::Producer, (0, -1)),
            Cell::new(CellType::Producer, (0, 0)),
            Cell::from_organelles(crate::organelle::Organelles(ROOT), (0, 1)),
        ];
        let mut org = Organism::new(0, anchor, cells, Genome::blank());
        org.energy = 500;
        org
    }

    /// 湿度随离水距离**线性**下降，超过跨度就干透。
    ///
    /// 线性是被实测逼出来的：平方和"产能按湿度打折"叠起来是四次方，
    /// 可活的带子窄到只剩离岸十列。
    #[test]
    fn soil_moisture_falls_off_with_distance_from_water() {
        let (mut w, shore) = flat_shore();
        let sim = LifeSim::new(&mut w);
        // 土壤在 y=60（水面之下 20 格那层石头），所以这里量的是"地表"。
        let at = |d: i32| sim.moisture_at(shore + d, 60);
        // 最靠岸的那一列**不含水**（水到 x=19 为止），而湿度是**粗格**
        // 之间的斜向距离（chamfer 4/3 格 × 4 格/粗格 ≈ 5.3 格），
        // 所以是 248 而不是 255。差这几格是"粗格"的代价，不是 bug。
        assert!(at(0) >= 245, "紧挨着水的土几乎是满的：{}", at(0));
        assert!(at(50) > at(100), "越远越干：{} vs {}", at(50), at(100));
        assert!(at(100) > at(150), "越远越干：{} vs {}", at(100), at(150));
        assert_eq!(at(210), 0, "超过跨度就干透");
        // 环绕仍然是通的：把水放在另一边时，这一列会重新变湿。
        let mut w2 = World::new(800, 100).unwrap();
        w2.fill_rect(0, 60, 800, 40, MaterialKind::Stone);
        w2.fill_rect(780, 40, 20, 20, MaterialKind::Water);
        let sim2 = LifeSim::new(&mut w2);
        assert!(
            sim2.moisture_at(20, 60) > 200,
            "水在另一头（环绕 21 列远）时，这一列应当很湿：{}",
            sim2.moisture_at(20, 60)
        );
    }

    /// **湿度是二维的：悬崖顶上比脚下干。**
    ///
    /// 这一条是 v0.42 换掉"每列一个数"的全部理由。旧模型里同一列从地表
    /// 到地底一样湿，于是"离海 50 格、但高出海面 60 格"的悬崖顶被算成
    /// 和海岸平地一样湿——而它实际上够不到任何水。
    ///
    /// 判据用同一列的两个高度，所以**横向距离完全相同**，差别只可能来自
    /// 纵向。这样它测的就只有"二维"这一件事。
    #[test]
    fn a_cliff_top_is_drier_than_its_base() {
        let mut w = World::new(200, 200).unwrap();
        // 左边一片海（水面 y=90），右边整块陆地。
        w.fill_rect(0, 90, 20, 20, MaterialKind::Water);
        w.fill_rect(0, 100, 200, 100, MaterialKind::Stone);
        // 一根柱子：x=60..64 从 y=40 一直到地底。
        w.fill_rect(60, 40, 4, 60, MaterialKind::Stone);
        let sim = LifeSim::new(&mut w);

        let base = sim.moisture_at(61, 99);
        let top = sim.moisture_at(61, 41);
        assert!(
            base > top,
            "同一列里，柱子根应当比柱顶湿：根 {base}、顶 {top}"
        );
        assert!(
            base - top > 20,
            "差得应当明显（柱顶高出海面 50 格）：根 {base}、顶 {top}"
        );
    }

    /// **画笔挖出来的湖也会变湿。**
    ///
    /// 湿度只在 `step` 末尾重算，而暂停时 `step` 不走——所以玩家用画笔
    /// 挖一个湖之后必须显式调 `refresh_moisture`。不调的话：根吸不到
    /// 那几格水、湿度视图也纹丝不动，看起来就是"湖白挖了"。
    #[test]
    fn painting_a_lake_makes_the_ground_around_it_wet() {
        // 一滴水都没有的世界：全干。
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 40, 64, 24, MaterialKind::Stone);
        let mut sim = LifeSim::new(&mut w);
        assert_eq!(sim.moisture_at(32, 39), 0, "没有水的世界应当全干");

        // 在中间挖一个湖，然后**显式刷新**（暂停时不会有 `step`）。
        w.fill_rect(28, 30, 8, 10, MaterialKind::Water);
        sim.refresh_moisture(&w);

        assert!(
            sim.moisture_at(28, 39) > 200,
            "湖边的土应当立刻变湿：{}",
            sim.moisture_at(28, 39)
        );
        assert!(
            sim.moisture_at(32, 39) > sim.moisture_at(60, 39),
            "离湖越远越干：近 {}、远 {}",
            sim.moisture_at(32, 39),
            sim.moisture_at(60, 39)
        );
    }

    /// **根能从土里吸水**——这一条让"上岸"从"贴着水"变成"能走多远"。
    ///
    /// 判据不是"活得久"，而是"含水量不往下掉"：那才是"有水源"这件事
    /// 最直接的证据。
    #[test]
    fn a_root_drinks_from_moist_soil() {
        let (mut w, shore) = flat_shore();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // 离岸 10 列：湿度 241，够回满。
        let id = sim
            .spawn_injected(&mut w, potted_plant((shore + 10, 58)))
            .unwrap();
        let before = sim.organism(id).unwrap().hydration;
        for _ in 0..60 {
            sim.tick_hydration(&w);
        }
        assert_eq!(
            sim.organism(id).unwrap().hydration,
            before,
            "根在湿土里，含水量不该往下掉"
        );
    }

    /// **干土里没有水。** 离岸 190 列（湿度 13 → 回充 0）→ 每 tick 掉一格。
    #[test]
    fn a_root_in_dry_soil_still_thirsts() {
        let (mut w, shore) = flat_shore();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let x = shore + 190;
        assert!(
            sim.moisture_at(x, 60) < 40,
            "离岸 190 列的湿度应当低到回充不了：{}",
            sim.moisture_at(x, 60)
        );
        let id = sim.spawn_injected(&mut w, potted_plant((x, 58))).unwrap();
        let before = sim.organism(id).unwrap().hydration;
        for _ in 0..60 {
            sim.tick_hydration(&w);
        }
        assert_eq!(
            sim.organism(id).unwrap().hydration,
            before - 60,
            "干土里应当每 tick 掉一格"
        );
    }

    /// 产量**整具身体一次截断**，不是逐格截断再乘格数。
    ///
    /// 这条是纯粹的算术判据：单格 4.72 截成 4 再乘 6 得 24，
    /// 而整具一次截断得 29——**15% 的差别**，在岸上是生死线。
    #[test]
    fn production_is_truncated_once_for_the_whole_body() {
        let one = producer_gain(0.77, 0.8, 0.32, 1.0, 1);
        let six = producer_gain(0.77, 0.8, 0.32, 1.0, 6);
        assert_eq!(one, 4, "单格应当是 4（4.72 截断）");
        assert_eq!(six, 28, "六格应当是一次截断的 28，不是 6×4=24");
        assert!(six > one * 6, "逐格截断会丢 {}%", 100 - one * 6 * 100 / six);
    }

    /// **根系越厚，岸上产能越高**——这是"投资根系"唯一的回报来源。
    ///
    /// v0.37 里 `LAND_DAMP` 是个常数，所以一格根和十格根产得一样多，
    /// 突变没有任何理由往"多长根"走（实测 8000 tick 一株带根植物都没出现）。
    /// 这条测试钉的就是那个梯度：**同一具身体、同一个位置，只改根格数**。
    #[test]
    fn more_roots_yield_more_energy_on_land() {
        // 水面 y=30。锚点 25，四格生产者占 y=25..28（粗格 24..27 里没有水，
        // 所以 `dampness` 是 0），根横排在 y=29——四邻 (x,30) 全是水。
        fn plant(roots: usize) -> Organism {
            let mut cells: Vec<Cell> = (0..4)
                .map(|dy| Cell::new(CellType::Producer, (0, dy)))
                .collect();
            for i in 0..roots {
                // **纯根，不带叶绿体。** 否则两组的 `producers` 数就不一样了，
                // 测到的会是"格数变多"，而不是"根系变厚"。
                cells.push(Cell::from_organelles(
                    crate::organelle::Organelles(ROOT),
                    (i as i16, 4),
                ));
            }
            Organism::new(0, (20, 25), cells, Genome::blank())
        }

        fn injected(roots: usize) -> u64 {
            let mut w = pool();
            let mut sim = LifeSim::new(&mut w);
            sim.set_food_growth(false);
            let id = sim.spawn_injected(&mut w, plant(roots)).unwrap();
            assert_eq!(
                LifeSim::root_water(&w, sim.moisture(), sim.organism(id).unwrap()),
                Some(u8::MAX),
                "{roots} 格根的版本必须都算贴水，否则两组不可比"
            );
            sim.inject_sunlight(&mut w);
            sim.ledger().injected
        }

        let one = injected(1);
        let four = injected(4);
        assert!(one > 0, "一格根也该有产出，实测 {one}");
        // 0.28 vs 0.52 → 比值 1.86。截断会吃掉几个点，留到 1.5 就够严。
        assert!(
            four as f32 > one as f32 * 1.5,
            "四格根该比一格根产得多得多：一格 {one}、四格 {four}"
        );
    }

    /// 根系买到的产能**有上限**，而且起点比 v0.37 那个常数低。
    ///
    /// 上限存在的理由不是数值好看：`0.6 × 岸上强光 ≈ 水里满产`，
    /// 再往上抬，第一株长出厚根系的植物就会铺满陆地（v0.23 海草 481 株）。
    #[test]
    fn root_dampness_is_a_capped_ramp() {
        assert!(land_damp(0) < land_damp(1), "零格根是起点，一格就该更高");
        assert!(
            land_damp(1) < land_damp(3),
            "三格根该比一格高：{} vs {}",
            land_damp(1),
            land_damp(3)
        );
        assert_eq!(land_damp(100), LAND_DAMP_MAX, "再多的根也只到上限");
        assert!(
            land_damp(0) > 0.0,
            "起点不能是 0——那样一格根等于没有"
        );
    }

    /// **食腐的判据是"吃掉的是尸体"**，不是"吃到了东西"。
    ///
    /// `eaten` 把碎屑、藻、尸体混在一起数，而"捡世界生成时撒下的底货"
    /// 和"吃别人"是两件事。这个数长期为 0 就说明食腐生态位**没有被占据**，
    /// 不管机制多完整。
    #[test]
    fn scavenging_is_counted_apart_from_eating_debris() {
        let mut w = shallow_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((10, 20), Genome::blank(), 100),
            )
            .unwrap();

        // 碎屑：算 `eaten`，不算 `scavenged`。
        w.set(20, 20, MaterialKind::Remains);
        w.set_flags(20, 20, 25);
        assert!(sim.eat_remains(&mut w, id, 20, 20), "应当吃得下");
        assert_eq!(sim.stats().scavenged, 0, "吃碎屑不算食腐");
        assert_eq!(sim.stats().eaten, 1);

        // 尸体：两样都算。
        w.set(21, 20, MaterialKind::Corpse);
        w.set_flags(21, 20, 60);
        assert!(sim.eat_remains(&mut w, id, 21, 20), "应当吃得下");
        assert_eq!(sim.stats().scavenged, 1, "吃尸体才是食腐");
        assert_eq!(sim.stats().eaten, 2);
    }

    // -----------------------------------------------------------------------
    // 尸体嗅觉（v0.39）
    // -----------------------------------------------------------------------

    /// 尸体味指向**最近的尸体**，而且只认**带能量的**。
    ///
    /// "只认带能量的"是一条有意的取舍：0 能量的尸体吃了等于没吃，
    /// 让食腐者去追一具空壳比闻不到更糟——它会把全部能量花在赶路上。
    #[test]
    fn corpse_scent_points_at_the_nearest_corpse() {
        let mut w = shallow_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((10, 20), Genome::blank(), 500),
            )
            .unwrap();
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();

        let calm = sim.sense(&w, i);
        assert_eq!(calm[IN_CORPSE_DIR_X], 0.0, "没有尸体时这一路该是 0");
        assert_eq!(calm[IN_CORPSE_DIR_Y], 0.0, "没有尸体时这一路该是 0");

        // 右边 6 格放一具尸体。
        w.set(16, 20, MaterialKind::Corpse);
        w.set_flags(16, 20, 60);
        let right = sim.sense(&w, i);
        assert!(
            right[IN_CORPSE_DIR_X] > 0.0,
            "尸体在右边，x 该是正的：{}",
            right[IN_CORPSE_DIR_X]
        );
        assert!(
            right[IN_CORPSE_DIR_Y].abs() < 1e-6,
            "同一行上不该有垂直分量：{}",
            right[IN_CORPSE_DIR_Y]
        );

        // 换到左边。
        w.set(16, 20, MaterialKind::Empty);
        w.set(4, 20, MaterialKind::Corpse);
        w.set_flags(4, 20, 60);
        let left = sim.sense(&w, i);
        assert!(
            left[IN_CORPSE_DIR_X] < 0.0,
            "尸体在左边，x 该是负的：{}",
            left[IN_CORPSE_DIR_X]
        );

        // 空壳不算。
        w.set_flags(4, 20, 0);
        let empty = sim.sense(&w, i);
        assert_eq!(
            empty[IN_CORPSE_DIR_X], 0.0,
            "0 能量的尸体不该被闻到——追空壳比闻不到更糟"
        );
    }

    /// 尸体味的遮挡规则**故意和食物射线不同**：碎屑不挡，石头挡。
    ///
    /// 这条是"尸体有自己一条射线"的全部理由。共用射线的话，海里第一个
    /// 含能量食物几乎总是藻或碎屑，射线一停尸体就再也闻不到了。
    #[test]
    fn corpse_scent_ignores_debris_but_not_stone() {
        let mut w = shallow_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((10, 20), Genome::blank(), 500),
            )
            .unwrap();
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();

        // 尸体在右边 8 格，中间 12..14 铺一堆碎屑。
        w.set(18, 20, MaterialKind::Corpse);
        w.set_flags(18, 20, 60);
        for x in 12..=14isize {
            w.set(x, 20, MaterialKind::Remains);
            w.set_flags(x, 20, 25);
        }
        let through_debris = sim.sense(&w, i);
        assert!(
            through_debris[IN_CORPSE_DIR_X] > 0.0,
            "碎屑不该挡住尸体味：{}",
            through_debris[IN_CORPSE_DIR_X]
        );

        // 同一批格子换成石头：味道该断。
        for x in 12..=14isize {
            w.set(x, 20, MaterialKind::Stone);
        }
        let through_stone = sim.sense(&w, i);
        assert_eq!(
            through_stone[IN_CORPSE_DIR_X], 0.0,
            "石头该挡住尸体味（沉积物会吸收），实测 {}",
            through_stone[IN_CORPSE_DIR_X]
        );
    }

    /// **这一路输入真的能驱动行为**：接上之后，鱼会逆着巡航偏置朝尸体游。
    ///
    /// 只断言"输入有值"是不够的——那只能说明 `sense` 写了数，说明不了
    /// 这路数进得了网络、赢得了 `pick_direction` 的阈值。
    /// 所以这里给基线一个**往右**的巡航偏置，把尸体放在**左边**：
    /// 没有这一路的话它一定往右。
    #[test]
    fn a_genome_can_steer_by_corpse_scent() {
        let mut w = shallow_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        // 尸体在左边 6 格。
        w.set(4, 20, MaterialKind::Corpse);
        w.set_flags(4, 20, 60);

        let mut g = Genome::blank();
        // 和手工鱼一样的巡航偏置：没有别的信号时它往右。
        g.weights[Genome::b_o(OUT_RIGHT)] = 0.7;
        // 尸体在左 → `IN_CORPSE_DIR_X` 是负的 → 乘 -10 变成正的"向左"。
        g.connect(IN_CORPSE_DIR_X, OUT_LEFT, -10.0);

        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((10, 20), g, 2_000))
            .unwrap();
        for _ in 0..40 {
            sim.step(&mut w);
        }
        let x = sim.organism(id).expect("40 tick 不该死").anchor.0;
        assert!(
            x < 10,
            "闻着尸体味该往左游（基线是往右），实测 x={x}"
        );
    }

    /// 含水量掉到 0 之后，光合彻底停下（不是"变慢"，是"停"）。
    #[test]
    fn a_dry_body_stops_photosynthesizing() {
        let mut w = shallow_pool();
        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((10, 12), Genome::blank(), 200),
            )
            .unwrap();
        // 直接把它拧干：这一步跳过 `tick_hydration` 的补水。
        let i = sim.organisms.iter().position(|o| o.id == id).unwrap();
        sim.organisms[i].hydration = 0;
        let before = sim.organisms[i].energy;
        // 只跑光合那一步，避开代谢扣费。
        sim.inject_sunlight(&mut w);
        assert_eq!(
            sim.organism(id).unwrap().energy,
            before,
            "干透的身体不该再产出任何能量"
        );
    }

    /// **身体遮挡按覆盖度走**：密实的身体挡得多，稀疏的挡得少。
    ///
    /// 这是"长高有收益"的唯一来源：一条鱼挡住下面一格没什么，
    /// 而一片占满粗格的植被会把整列压暗——邻居就没饭吃了。
    ///
    /// 两个比值一起断言，而不是各写一个绝对阈值：`BODY_SHADE` 是会变的
    /// （实测从 0.5 降到 0.2 才保住生态），而"密实的挡得比稀疏的多"
    /// 这条**关系**不该跟着变。
    #[test]
    fn shading_scales_with_coverage() {
        fn light_under(offsets: &[(i16, i16)]) -> (f32, f32) {
            let mut w = pool();
            let mut sim = LifeSim::new(&mut w);
            sim.set_food_growth(false);
            sim.refresh_field(&w);
            let before = sim.field().light_at(20.0, 50.0);

            let cells: Vec<Cell> = offsets
                .iter()
                .map(|&o| Cell::new(CellType::Armor, o))
                .collect();
            let mut org = Organism::new(0, (20, 36), cells, Genome::blank());
            org.energy = 500;
            sim.spawn_injected(&mut w, org).unwrap();
            sim.refresh_field(&w);
            (before, sim.field().light_at(20.0, 50.0))
        }

        // 4×4 = 16 格，正好占满一个粗格。
        let dense: Vec<(i16, i16)> = (0..4)
            .flat_map(|dy| (0..4).map(move |dx| (dx as i16, dy as i16)))
            .collect();
        // 4 格横排，只占粗格的 1/4。
        let sparse: Vec<(i16, i16)> = (0..4).map(|i| (i as i16, 0)).collect();

        let (b1, a1) = light_under(&dense);
        let (b2, a2) = light_under(&sparse);
        let dense_ratio = a1 / b1;
        let sparse_ratio = a2 / b2;

        assert!(
            dense_ratio < 0.9,
            "占满一个粗格的身体应当明显挡光：{b1:.3} → {a1:.3}"
        );
        assert!(
            sparse_ratio > 0.9,
            "4 格的身体只占 1/4，不该明显挡光：{b2:.3} → {a2:.3}"
        );
        assert!(
            dense_ratio < sparse_ratio,
            "密实的身体必须比稀疏的挡得多：{dense_ratio:.3} vs {sparse_ratio:.3}"
        );
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
        let mut w = deep_pool();

        let mut sim = LifeSim::new(&mut w);
        sim.set_food_growth(false);
        let id = sim
            .spawn_injected(
                &mut w,
                LifeSim::simple_fish((10, 200), breeder_genome(), 200),
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
