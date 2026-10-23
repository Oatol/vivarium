//! 手工设计的体型（M3 · 3a）。
//!
//! ## 为什么先做手工体型，而不是形态发生
//!
//! 渲染层已经把"这一格是什么细胞"画得很清楚了：圆角、膜、按功能上色、
//! 同一条鱼的细胞连成一整块。但身体只有三格，放大之后就是**三个圆**。
//!
//! 所以"看不到结构"的原因不是画不出来，是**没有结构可画**。
//! 手工体型能在很短的时间里给出一个"能看的参照物"——先有它，
//! 讨论形态发生（3b）时才有东西可比：什么形状更好、怎么评价好坏、
//! 长出来的身体像不像这些手工的。
//!
//! 代价必须写清楚：**这是设计，不是生长**。体型写死在基因之外，
//! 子代原样继承亲代的形状，突变碰不到它。真正的"形状可遗传、可变异"
//! 要等 GRN 接上形态发生。
//!
//! ## 坐标约定
//!
//! - `+x` 是前方（头），`-x` 是尾；
//! - `+y` 向下（重力方向），`-y` 向上（朝光）；
//! - 锚点是 `(0, 0)` 那一格，**它必须是一个真实存在的细胞**——
//!   `sense` 用它取"我在多深"，`reproduce` 用它算拥挤。
//!
//! 朝向为左时整个身体沿 x 镜像（见 [`crate::life::Organism::offset_x`]），
//! 所以这里只需要按"朝右"画一遍。
//!
//! ## 每个体型都得算得过账
//!
//! 生产者的边际收益是 `PRODUCER_YIELD × 光 − 5`，浅水（光 ≈ 0.8）下约 **+14/tick**；
//! 移动细胞每 tick 要付 `4` 维持 + `2` 质量。所以一个体型能撑几个移动细胞，
//! 大致就是 `生产者数 × 2.3`。
//!
//! 这条约束决定了下面这些形状为什么长这样：**它们不是画着好看就行**，
//! 生产者太少会饿死，太多则变成赚钱机器（繁殖过快，种群瞬间铺满）。

use crate::life::{Cell, CellType};

/// 一个手工体型。
pub struct BodyPlan {
    /// 显示名。ASCII 与调试输出用它区分个体。
    pub name: &'static str,
    /// 身体：`(类型, dx, dy)`，相对锚点。
    pub cells: &'static [(CellType, i16, i16)],
}

impl BodyPlan {
    /// 展开成 [`Cell`] 列表。
    pub fn cells(&self) -> Vec<Cell> {
        self.cells
            .iter()
            .map(|&(kind, x, y)| Cell::new(kind, (x, y)))
            .collect()
    }

    /// 细胞数。
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// 是否为空（`len` 的伴生方法，clippy 要求有）。
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// 某种细胞有几个。
    pub fn count(&self, kind: CellType) -> usize {
        self.cells.iter().filter(|c| c.0 == kind).count()
    }
}

/// 银鱼：最"正常"的一条鱼。
///
/// 三排：背上与腹下是生产者（朝光的一面晒得到太阳），中间一列是移动细胞，
/// 前端两个嘴。尾鳍那三格移动细胞是**视觉上的尾**——
/// 它们对运动没有额外贡献，但少了它们身体就是一根棍子。
///
/// 账：4 生产者 + 3 移动 + 2 嘴 + 1 眼 = 10 格，
/// 维持 37、质量 13、浅水收入 76.8 → **净 +26.8/tick**。
pub const SILVERFISH: BodyPlan = BodyPlan {
    name: "银鱼",
    cells: &[
        (CellType::Mover, -2, 0),
        (CellType::Mover, -1, 0),
        (CellType::Mover, 0, 0),
        (CellType::Mouth, 1, 0),
        (CellType::Mouth, 2, 0),
        (CellType::Producer, 1, -1),
        (CellType::Producer, 2, -1),
        (CellType::Producer, 0, 1),
        (CellType::Producer, 1, 1),
        (CellType::Eye, 2, 1),
    ],
};

/// 叶背鱼：背上驮着一大片生产者，像一片会游的叶子。
///
/// 生产者多、装甲多，于是又重又慢，但晒得到的光也多。
/// 它是"体型变大"能带来什么的最直接展示：**更大的受光面积**。
///
/// 账：6 生产者 + 3 移动 + 2 嘴 + 3 装甲 = 14 格，
/// 维持 49、质量 23、浅水收入 115 → **净 +43/tick**。
pub const LEAFBACK: BodyPlan = BodyPlan {
    name: "叶背鱼",
    cells: &[
        (CellType::Mover, -2, 0),
        (CellType::Mover, -1, 0),
        (CellType::Mover, 0, 0),
        (CellType::Mouth, 1, 0),
        (CellType::Mouth, 2, 0),
        (CellType::Producer, -1, -1),
        (CellType::Producer, 0, -1),
        (CellType::Producer, 1, -1),
        (CellType::Producer, 2, -1),
        (CellType::Producer, 0, -2),
        (CellType::Producer, 1, -2),
        (CellType::Armor, -1, 1),
        (CellType::Armor, 0, 1),
        (CellType::Armor, 1, 1),
    ],
};

/// 蠕虫：细长、分节，靠身体两侧的生产者节段养活一长串移动细胞。
///
/// 它是这些体型里**最穷**的，而且穷的原因很具体：6 个移动细胞让它跑 2 格/tick，
/// 而"跑得快"是要付钱的（`Organism::move_cost`：速度 2 每 tick 烧 1.5 倍质量）。
///
/// 第一版只有 4 个生产者，按旧的"速度免费"成本算勉强够活；接上速度成本之后
/// 直接变成负数。所以补了一格生产者——**这就是"快"的代价第一次落在形状上**：
/// 想要两倍速度，就得有两倍以上的产出。
///
/// 账：5 生产者 + 6 移动 + 1 嘴 = 12 格，
/// 维持 51、质量 18、速度 2 → 移动 28/tick，
/// 浅水收入 96 → **净 +17/tick**。
pub const WORM: BodyPlan = BodyPlan {
    name: "蠕虫",
    cells: &[
        (CellType::Mover, -3, 0),
        (CellType::Mover, -2, 0),
        (CellType::Mover, -1, 0),
        (CellType::Mouth, 0, 0),
        (CellType::Mover, 1, 0),
        (CellType::Mover, 2, 0),
        (CellType::Mover, 3, 0),
        (CellType::Producer, -2, -1),
        (CellType::Producer, 0, -1),
        (CellType::Producer, 2, -1),
        (CellType::Producer, -2, 1),
        (CellType::Producer, 2, 1),
    ],
};

/// 水母：近似放射对称，中间一个嘴，四周一圈生产者与装甲。
///
/// 它和鱼的区别不只是形状：**它不靠"前方"活着**。嘴在身体中心，
/// 四面都能吃，所以撞墙对它的影响比鱼小得多。
///
/// 和蠕虫一样，它也得为"速度 2"付钱：4 个移动细胞买到 2 格/tick，
/// 而移动成本按速度加成（1.5 倍质量）。原来的 4 生产者 + 1 眼撑不住，
/// 所以把眼换成了一格生产者——**看得远是锦上添花，跑得动才是活着**。
/// 它因此没有眼，嗅觉半径退回基础值 20。
///
/// 账：5 生产者 + 4 移动 + 1 嘴 + 2 装甲 = 12 格，
/// 维持 45、质量 20、速度 2 → 移动 30/tick，
/// 浅水收入 96 → **净 +21/tick**。
pub const JELLY: BodyPlan = BodyPlan {
    name: "水母",
    cells: &[
        (CellType::Mover, 0, -1),
        (CellType::Mover, -1, 0),
        (CellType::Mover, 1, 0),
        (CellType::Mover, 0, 1),
        (CellType::Mouth, 0, 0),
        (CellType::Producer, -1, -1),
        (CellType::Producer, 1, -1),
        (CellType::Producer, -1, 1),
        (CellType::Producer, 1, 1),
        (CellType::Armor, -2, 0),
        (CellType::Armor, 2, 0),
        (CellType::Producer, 0, -2),
    ],
};

/// 掠食者：前端两枚杀手细胞，像一对颚。
///
/// 它是**唯一**一种"收入不只来自光"的体型：杀手细胞每 tick 从邻格的
/// 别的生物身上咬下一口能量（见 `LifeSim::predate`）。所以它的生产者比别的
/// 体型少——它靠吃别人补上那一份。
///
/// 这也是"身体即策略"的第一个例子：**要不要当捕食者，是长成什么样决定的**，
/// 不是网络输出决定的。杀手细胞自己就要付维持成本，养不起就饿死。
///
/// 账：4 生产者 + 3 移动 + 2 杀手 + 1 嘴 = 10 格，
/// 维持 40、质量 13、浅水收入 76.8 → **净 +23.8/tick**（不含捕食）。
///
/// 生产者给到 4 是**必须的**，不是保守：第一版只给 3（净 +10.6 是在光 0.8 下算的），
/// 而实际水面在 y≈155/512，光只有 0.70——净收益掉到 +3/tick，
/// 捕食者还没碰到猎物就饿死了。实测 8000 tick 时 `杀手 0 枚`：
/// 捕食者全灭，而"生物数""细胞数""能量账"三项都完全正常。
pub const HUNTER: BodyPlan = BodyPlan {
    name: "掠食者",
    cells: &[
        (CellType::Killer, 1, 0),
        (CellType::Killer, 2, 0),
        (CellType::Mouth, 0, 1),
        (CellType::Mover, 0, 0),
        (CellType::Mover, -1, 0),
        (CellType::Mover, -2, 0),
        (CellType::Producer, 0, -1),
        (CellType::Producer, 1, -1),
        (CellType::Producer, -1, -1),
        (CellType::Producer, -2, -1),
    ],
};

/// 海草：竖直的几节生产者，扎在浅水里。
///
/// **它是唯一不动的体型**——没有移动细胞，基因组里也没有方向偏置
/// （见 `life_sim::rooted_genome`）。它靠光活着，靠分裂繁殖。
///
/// 存在理由：让"生物"不只是鱼。一条 4 格的生产者链在生态里扮演的是
/// **初级生产者**——它把光变成可吃的能量，鱼吃它（或者吃它的尸体）。
/// 没有它，整个生态的能量入口只有鱼自己身上那几个生产者细胞，
/// 而"谁吃谁"就没有层次了。
///
/// 账：4 生产者 + 0 移动 = 4 格，维持 20、质量 8、不动，
/// 浅水收入 76.8 → **净 +56.8/tick**。它是全表里最赚的体型，
/// 代价是它一格也动不了：光变弱时只能等死。
pub const KELP: BodyPlan = BodyPlan {
    name: "海草",
    cells: &[
        (CellType::Producer, 0, 0),
        (CellType::Producer, 0, -1),
        (CellType::Producer, 0, -2),
        (CellType::Producer, 0, -3),
    ],
};

/// 全部手工体型。
///
/// 顺序有意义：[`crate::life_sim::stock_surface`] 按它轮换投放，
/// 所以换顺序会改变同一个种子里"哪条鱼长什么样"。
/// **海草不在这个表里。**
///
/// 它是"玩家在工坊里画出来的东西"，不是"自动投放的物种"。
/// 原因是实测的：海草不动、不付移动成本、4 个生产者净赚 56.8/tick，
/// 比任何一条鱼都富——把它放进轮换投放，它会指数增长到 481 株、
/// 把光全吃光，鱼一条不剩。**固着生物在"按格数算产能"的模型里天然占优**，
/// 所以它只能由玩家亲手种，不能自动撒。
pub const PLANS: &[BodyPlan] = &[SILVERFISH, LEAFBACK, WORM, JELLY, HUNTER];

/// 最大的手工体型有多少格。
///
/// 它必须不大于 [`crate::grn::MAX_BODY_CELLS`]——那是**身体格数的硬上限**，
/// 手工体型和形态发生长出来的身体共用同一条线。
///
/// 早先这里写的是"必须不大于 `SMALL_BODY_MAX`"，而那个常量是"一次移动能顶开
/// 多少格水"——一条**通行性判据**被当成了体型上限，于是 17 格以上的身体
/// 在水里根本动不了。v0.26 拆掉了。
pub const MAX_PLAN_CELLS: usize = 14;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_plan_has_a_cell_on_the_anchor() {
        // 锚点不是装饰：`sense` 用它取深度，`reproduce` 用它算拥挤。
        // 锚点落在空处不会立刻崩，但会让"我在多深"和实际身体位置错开。
        for plan in PLANS {
            assert!(
                plan.cells.iter().any(|&(_, x, y)| x == 0 && y == 0),
                "体型「{}」的锚点 (0,0) 上没有细胞",
                plan.name
            );
        }
    }

    #[test]
    fn no_plan_overlaps_itself() {
        for plan in PLANS {
            let mut seen = std::collections::HashSet::new();
            for &(_, x, y) in plan.cells {
                assert!(
                    seen.insert((x, y)),
                    "体型「{}」在 ({x}, {y}) 上叠了两格",
                    plan.name
                );
            }
        }
    }

    /// 每个体型在浅水里的**每 tick 收支**。
    ///
    /// 移动那一项必须用真的公式（`life::move_cost` × 速度），不能拿质量顶替：
    /// 速度 2 的体型每 tick 烧 1.5 倍质量，用质量算会高估它们两成的收入——
    /// 而那正是"蠕虫和水母悄悄饿死"的那种错，从测试上看不出来。
    fn plan_budget(plan: &BodyPlan) -> (f32, u32, u32) {
        use crate::life::{move_cost, Organism};
        let org = Organism::new(0, (0, 0), plan.cells(), crate::life::Genome::blank());
        let producers = plan.count(CellType::Producer);
        // 生产者边际收益 ≈ PRODUCER_YIELD × 0.8 − 5（它的维持成本）。
        let income = 24.0 * 0.8 * producers as f32;
        let upkeep: u32 = plan.cells.iter().map(|c| c.0.upkeep()).sum();
        // 每 tick 走满 `speed` 格的总成本。
        let move_per_tick = org.move_cost() * org.speed() as u32;
        let _ = move_cost;
        (income, upkeep, move_per_tick)
    }

    #[test]
    fn every_plan_pays_for_itself_in_shallow_water() {
        // 这一条是这些形状存在的理由。生产者太少 → 饿死；这条测试是它的护栏。
        for plan in PLANS {
            let (income, upkeep, moving) = plan_budget(plan);
            let net = income - upkeep as f32 - moving as f32;
            assert!(
                net > 5.0,
                "体型「{}」在浅水里每 tick 只净赚 {net:.1}，撑不住（收入 {income:.1} / 维持 {upkeep} / 移动 {moving}）",
                plan.name
            );
        }
    }

    #[test]
    fn no_plan_is_a_money_printer() {
        // 净收益太高会变成"繁殖快到瞬间铺满"——那样就没有生态可言了。
        for plan in PLANS {
            let (income, upkeep, moving) = plan_budget(plan);
            let net = income - upkeep as f32 - moving as f32;
            assert!(
                net < 60.0,
                "体型「{}」每 tick 净赚 {net:.1}，太快了",
                plan.name
            );
        }
    }

    /// 建造细胞**降低整个身体的维持成本**，而且身体越大越划算。
    ///
    /// 这是它存在的理由：在此之前 `Builder` 只是一个"付 6/tick、什么都不换"
    /// 的格子——玩家画上去只会让身体更穷。现在它有了一个真实的权衡：
    /// 3 格的小鱼带它是亏的（省下的 20% 不够付它自己的 6 点），
    /// 14 格的身体带 2 个能省 36%。
    #[test]
    fn builders_cut_the_upkeep_of_the_whole_body() {
        use crate::life::{Cell, Genome, Organism};
        let build = |cells: &[(crate::life::CellType, i16, i16)]| {
            Organism::new(
                0,
                (0, 0),
                cells
                    .iter()
                    .map(|&(k, x, y)| Cell::new(k, (x, y)))
                    .collect(),
                Genome::blank(),
            )
        };
        use crate::life::CellType as C;

        // 一条 8 格的鱼（没有建造细胞）。
        let plain = build(&[
            (C::Producer, 0, 0),
            (C::Producer, 1, 0),
            (C::Producer, 2, 0),
            (C::Mover, 0, 1),
            (C::Mover, 1, 1),
            (C::Mouth, 2, 1),
            (C::Eye, 0, -1),
            (C::Armor, 1, -1),
        ]);
        let raw = plain.upkeep();
        assert_eq!(raw, 5 + 5 + 5 + 4 + 4 + 2 + 1 + 1);

        // 把一格装甲换成建造细胞：总成本应当**下降**，
        // 而且下降的幅度要大于建造细胞自己那 6 点的开销。
        let with_builder = build(&[
            (C::Producer, 0, 0),
            (C::Producer, 1, 0),
            (C::Producer, 2, 0),
            (C::Mover, 0, 1),
            (C::Mover, 1, 1),
            (C::Mouth, 2, 1),
            (C::Eye, 0, -1),
            (C::Builder, 1, -1),
        ]);
        let cut = with_builder.upkeep();
        assert!(cut < raw, "带建造细胞的身体维持成本应当更低：{raw} → {cut}");
        // 折扣作用在**含建造细胞在内**的总成本上：
        // 27 − 1（去掉装甲）+ 6（加上建造）= 32，再乘 0.8 = 25。
        // 净效果是 27 → 25：省 2 点。身体越大，这个差越大。
        assert_eq!(cut, (raw - 1 + 6) * 80 / 100);

        // 两格建造细胞是乘性叠加（0.64），不是各减 20 点。
        let two = build(&[
            (C::Producer, 0, 0),
            (C::Producer, 1, 0),
            (C::Producer, 2, 0),
            (C::Mover, 0, 1),
            (C::Mover, 1, 1),
            (C::Mouth, 2, 1),
            (C::Builder, 0, -1),
            (C::Builder, 1, -1),
        ]);
        assert_eq!(two.upkeep(), (5 + 5 + 5 + 4 + 4 + 2 + 6 + 6) * 60 / 100);

        // 折扣有下限：堆再多也最多省一半。
        let many = build(&[
            (C::Builder, 0, 0),
            (C::Builder, 1, 0),
            (C::Builder, 2, 0),
            (C::Builder, 0, 1),
            (C::Builder, 1, 1),
            (C::Builder, 2, 1),
        ]);
        assert_eq!(many.upkeep(), 36 * 50 / 100);
    }

    /// **跑得快更烧钱**：质量相同时，速度每加一档，每 tick 的移动成本就往上跳。
    ///
    /// 这是"移动细胞越多速度越快"的另一半。只做前一半的话，速度是白拿的，
    /// 体型会一路朝"堆移动细胞"跑，直到撞上维持成本的天花板——
    /// 而那和"进化在权衡快慢"是两件完全不同的事。
    #[test]
    fn speed_costs_energy() {
        use crate::life::move_cost;
        let mass = 20;
        let per_tick = |speed| move_cost(mass, speed) * speed as u32;
        assert_eq!(per_tick(1), 20, "速度 1 走一格就是一份质量");
        // 速度 2 是 25，每步向上取整（25/2 → 13 × 2 = 26）。
        assert_eq!(per_tick(2), 26, "速度 2 每 tick 1.25 倍质量");
        assert_eq!(per_tick(3), 30, "速度 3 每 tick 1.5 倍质量");
        assert!(
            per_tick(2) > per_tick(1) && per_tick(3) > per_tick(2),
            "速度必须单调更贵"
        );
        // 但**每格**成本是降的：多长移动细胞买的是"同样的钱跑得更远"。
        let per_cell = |speed| move_cost(mass, speed);
        assert!(per_cell(2) < per_cell(1) && per_cell(3) < per_cell(2));
    }

    /// 形状必须**换得到东西**：移动细胞买速度，眼买嗅觉半径。
    ///
    /// 这两条是"形态发生能进化"的全部前提。在那之前，一个 6 移动细胞的蠕虫
    /// 和一个 3 移动细胞的银鱼跑得一样快，眼则纯粹是白付维持成本的装饰——
    /// 于是"多长几格移动细胞"永远是个坏买卖，突变没有方向可走，
    /// 形态发生只能收敛到"已知最好的那个"，长不出新的。
    #[test]
    fn movers_buy_speed_and_eyes_buy_reach() {
        use crate::life::{Genome, Organism};
        let build = |p: &BodyPlan| Organism::new(0, (0, 0), p.cells(), Genome::blank());

        for plan in PLANS {
            let org = build(plan);
            let movers = plan.count(CellType::Mover) as i32;
            let eyes = plan.count(CellType::Eye) as i32;
            assert_eq!(
                org.speed(),
                (1 + movers / 4).min(3),
                "「{}」的速度不对",
                plan.name
            );
            assert_eq!(
                org.smell_radius(),
                20 + eyes * 4,
                "「{}」的嗅觉半径不对",
                plan.name
            );
        }

        // 至少得有一个体型真的靠形状换到了东西，否则这两条映射等于没接。
        assert!(
            PLANS.iter().any(|p| build(p).speed() > 1),
            "没有任何体型能从移动细胞换到速度"
        );
    }

    #[test]
    fn plans_fit_inside_the_body_limit() {
        for plan in PLANS {
            assert!(
                plan.len() <= MAX_PLAN_CELLS,
                "体型「{}」有 {} 格，超过 MAX_PLAN_CELLS = {MAX_PLAN_CELLS}",
                plan.name,
                plan.len()
            );
        }
    }

    #[test]
    fn plans_have_distinct_shapes() {
        // 体型必须真的不一样，否则"混合投放"没有意义。
        //
        // 判据是**细胞构成**，不是格数：早先比的是 `cells.len()`，而掠食者
        // 加进来之后和银鱼都是 10 格——但一个是「2 嘴 + 1 眼」，另一个是
        // 「2 杀手」，画面上（橙红 vs 紫红）一眼就能分开。
        // 格数只是个粗糙的替身，构成才是"形状"本身。
        for (i, a) in PLANS.iter().enumerate() {
            for b in &PLANS[i + 1..] {
                assert_ne!(a.name, b.name);
                let mut ca: Vec<(CellType, i16, i16)> = a.cells.to_vec();
                let mut cb: Vec<(CellType, i16, i16)> = b.cells.to_vec();
                ca.sort_by_key(|c| (c.0.as_u8(), c.1, c.2));
                cb.sort_by_key(|c| (c.0.as_u8(), c.1, c.2));
                assert_ne!(ca, cb, "「{}」与「{}」是同一个形状", a.name, b.name);
            }
        }
    }
}
