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
/// 它是这四种里**最穷**的：6 个移动细胞要付 24 维持 + 12 质量，
/// 4 个生产者只赚 76.8。净 +13.8/tick，不到银鱼的一半。
/// 留着它是因为它证明了一件事：**长身体不是免费的**。
///
/// 账：4 生产者 + 6 移动 + 1 嘴 = 11 格，净 **+13.8/tick**。
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
    ],
};

/// 水母：近似放射对称，中间一个嘴，四周一圈生产者与装甲。
///
/// 它和鱼的区别不只是形状：**它不靠"前方"活着**。嘴在身体中心，
/// 四面都能吃，所以撞墙对它的影响比鱼小得多。
///
/// 账：4 生产者 + 4 移动 + 1 嘴 + 2 装甲 + 1 眼 = 12 格，
/// 维持 41、质量 20、浅水收入 76.8 → **净 +15.8/tick**。
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
        (CellType::Eye, 0, -2),
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

/// 全部手工体型。
///
/// 顺序有意义：[`crate::life_sim::stock_surface`] 按它轮换投放，
/// 所以换顺序会改变同一个种子里"哪条鱼长什么样"。
pub const PLANS: &[BodyPlan] = &[SILVERFISH, LEAFBACK, WORM, JELLY, HUNTER];

/// 最大的手工体型有多少格。
///
/// [`crate::life_sim::SMALL_BODY_MAX`] 必须不小于它——否则"设计一个更大的身体"
/// 这件事在物理上根本做不到，大生物会直接卡在水里。
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

    #[test]
    fn every_plan_pays_for_itself_in_shallow_water() {
        // 这一条是这些形状存在的理由。生产者太少 → 饿死；这条测试是它的护栏。
        // 常数抄自 life_sim：生产者边际收益 ≈ PRODUCER_YIELD × 0.8 − upkeep。
        for plan in PLANS {
            let producers = plan.count(CellType::Producer);
            let income = 24.0 * 0.8 * producers as f32;
            let upkeep: u32 = plan.cells.iter().map(|c| c.0.upkeep()).sum();
            let mass: u32 = plan.cells.iter().map(|c| c.0.mass()).sum();
            let net = income - upkeep as f32 - mass as f32;
            assert!(
                net > 5.0,
                "体型「{}」在浅水里每 tick 只净赚 {net:.1}，撑不住（收入 {income:.1} / 维持 {upkeep} / 移动 {mass}）",
                plan.name
            );
        }
    }

    #[test]
    fn no_plan_is_a_money_printer() {
        // 净收益太高会变成"繁殖快到瞬间铺满"——那样就没有生态可言了。
        for plan in PLANS {
            let producers = plan.count(CellType::Producer);
            let income = 24.0 * 0.8 * producers as f32;
            let upkeep: u32 = plan.cells.iter().map(|c| c.0.upkeep()).sum();
            let mass: u32 = plan.cells.iter().map(|c| c.0.mass()).sum();
            let net = income - upkeep as f32 - mass as f32;
            assert!(
                net < 60.0,
                "体型「{}」每 tick 净赚 {net:.1}，太快了",
                plan.name
            );
        }
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
