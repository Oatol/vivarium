//! 形态发生（M3 · 3b）：让身体形状由基因长出来。
//!
//! ## 一句话
//!
//! 基因不再描述"这条鱼长什么样"，而是描述**"一格细胞该往哪儿长出一个什么"**。
//! 从一个根细胞出发，反复应用这套规则，长出来的那堆格子就是身体。
//!
//! ## 为什么是"规则表"，不是神经网络
//!
//! 真正的 GRN（基因调控网络）是一张连续动力学图：节点是基因、边是激活/抑制，
//! 细胞的类型由它落在哪个吸引子上决定。听起来很美，但它有个致命问题——
//! **改一个权重，形状往往一点不变；改十个权重，形状可能天翻地覆。**
//! 那种基因型到表现型的映射对突变极其不友好，搜索会退化成随机游走。
//!
//! 规则表正好相反：一个格子被改掉，形状就变一点点。**可搜索性**比"生物学像不像"
//! 重要得多——这里要的是一个能进化的东西，不是一个能发论文的东西。
//!
//! ## 基因型
//!
//! ```text
//! rules[代][母细胞类型][生出来的方向][生长的方向] = 子细胞类型（或者"不长"）
//! ```
//!
//! 四个维度各有用处，每一条都是被具体的形状逼出来的：
//!
//! - **母细胞类型**：让"生产者旁边长生产者、移动细胞旁边长移动细胞"成为可能，
//!   这是身体分节的基础。
//! - **生出来的方向**（细胞极性）：一条蠕虫的身体两侧各挂一个生产者，
//!   中间那几格移动细胞是"从左边生出来的"还是"从右边生出来的"必须能区分，
//!   否则同一条规则会在两侧都长出东西。
//! - **生长的方向**：四个方向，顺序固定（右、下、左、上）。
//!   **顺序是形状的一部分**——同一条规则在"先试右还是先试下"下会长出完全不同的身体。
//! - **代**：从根细胞出发的步数。同一个类型的细胞在不同位置想长不同的东西，
//!   而"位置"在规则表里只能靠"第几代"来近似。
//!
//! ## 表现型
//!
//! 从根细胞开始做**广度优先**生长：每一格依次尝试四个方向，能长就长，
//! 直到长满 `max_cells` 或再没有规则可用。全程没有随机数——
//! 同一个基因组必然长出同一个身体，否则"遗传"这件事就不成立了。

use std::collections::{HashMap, HashSet, VecDeque};

use crate::body::BodyPlan;
use crate::life::{Cell, CellType};
use crate::rng::Rng;

/// 生长规则能用的最大代数。
///
/// 必须大于 [`crate::body::MAX_PLAN_CELLS`]：一条 14 格的链要 13 代才长得完，
/// 卡在 8 会让"长一条长身体"在基因层面做不到。
pub const MAX_DEPTH: usize = 16;

/// 四个方向，**顺序固定**。
///
/// 右、下、左、上。这个顺序不是随便定的：广度优先生长时，
/// 每一格按这个顺序尝试，第一个成功的方向决定它的"主枝"朝哪儿。
/// 换顺序 = 换形状，所以它写死在这里，不进基因组。
pub const DIRS: [(i16, i16); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

/// 根细胞没有"生出来的方向"，用这个下标占位。
pub const NO_PARENT: usize = 4;

/// 一个"代"的规则：`[母细胞类型][生出来的方向][生长的方向]`。
pub type RuleRow = [[[Option<CellType>; 4]; NO_PARENT + 1]; CellType::COUNT];

/// 完整的规则表，长度 [`MAX_DEPTH`]。
pub type RuleTable = Vec<RuleRow>;

/// 形态发生基因组。
#[derive(Debug, Clone, PartialEq)]
pub struct Grn {
    /// 根细胞的类型（身体的第一个细胞，永远在 `(0, 0)`）。
    pub root: CellType,
    /// 身体最多长到多少格。
    ///
    /// 上限是必要的：没有它，一条"每个细胞都往右长"的规则会一直长下去，
    /// 而身体大小直接决定能量上限与移动成本。
    pub max_cells: u8,
    /// 规则表，长度 [`MAX_DEPTH`]。
    pub rules: RuleTable,
}

impl Default for Grn {
    /// 空基因组：只有一格根细胞，什么都不长。
    fn default() -> Self {
        Self {
            root: CellType::Producer,
            max_cells: 1,
            rules: empty_rules(),
        }
    }
}

fn empty_rules() -> RuleTable {
    vec![[[[None; 4]; NO_PARENT + 1]; CellType::COUNT]; MAX_DEPTH]
}

impl Grn {
    /// 从一个手工体型**反推**出生长程序。
    ///
    /// 这是 3b 的热启动：不从零开始摸索（那是纯随机搜索，几乎不可能撞出一个
    /// 能活的形状），而是**从已知能活的手工体型出发**，让突变去改它。
    ///
    /// 返回 `None` 表示这个体型**用规则表表达不出来**。
    /// 判断方法是直接长一遍再逐格比对，而不是靠推理——规则表的表达力边界
    /// 不是看出来的，是撞出来的。
    pub fn from_plan(plan: &BodyPlan) -> Option<Self> {
        let set: HashMap<(i16, i16), CellType> =
            plan.cells.iter().map(|&(k, x, y)| ((x, y), k)).collect();
        let root = *set.get(&(0, 0))?;

        let mut rules = empty_rules();
        let mut seen: HashSet<(i16, i16)> = HashSet::new();
        let mut queue: VecDeque<((i16, i16), usize, usize)> = VecDeque::new();
        seen.insert((0, 0));
        queue.push_back(((0, 0), 0, NO_PARENT));

        while let Some(((x, y), depth, from)) = queue.pop_front() {
            if depth + 1 >= MAX_DEPTH {
                return None;
            }
            let kind = set[&(x, y)];
            for (d, (dx, dy)) in DIRS.iter().enumerate() {
                let next = (x + dx, y + dy);
                if seen.contains(&next) {
                    continue;
                }
                let Some(&child) = set.get(&next) else {
                    continue;
                };
                let slot = &mut rules[depth][kind.as_u8() as usize][from][d];
                match slot {
                    Some(prev) if *prev != child => return None,
                    _ => *slot = Some(child),
                }
                seen.insert(next);
                queue.push_back((next, depth + 1, (d + 2) % 4));
            }
        }
        if seen.len() != set.len() {
            return None; // 身体不连通
        }

        let grn = Self {
            root,
            max_cells: plan.len() as u8,
            rules,
        };
        // 光有规则还不够：别的细胞在同一个(代, 类型, 极性)上可能多长出一格。
        // 所以**直接长一遍再比对**，这是唯一的判据。
        let mut a: Vec<(CellType, i16, i16)> = grn
            .develop()
            .iter()
            .map(|c| (c.kind, c.offset.0, c.offset.1))
            .collect();
        let mut b: Vec<(CellType, i16, i16)> = plan.cells.to_vec();
        a.sort_by_key(|c| (c.2, c.1, c.0.as_u8()));
        b.sort_by_key(|c| (c.2, c.1, c.0.as_u8()));
        if a != b {
            return None;
        }
        Some(grn)
    }

    /// 把基因组发育成一个身体。
    ///
    /// 广度优先、无随机数、`(0, 0)` 上一定有根细胞——这三条都是硬要求：
    /// 前者让"第几代"有意义，第二条让遗传成立，最后一条是 `sense` / `reproduce`
    /// 拿锚点算深度和拥挤的前提。
    pub fn develop(&self) -> Vec<Cell> {
        let cap = (self.max_cells as usize).max(1);
        let mut cells = vec![Cell::new(self.root, (0, 0))];
        let mut depths: Vec<usize> = vec![0];
        let mut froms: Vec<usize> = vec![NO_PARENT];
        let mut taken: HashSet<(i16, i16)> = HashSet::new();
        taken.insert((0, 0));

        let mut head = 0;
        while head < cells.len() && cells.len() < cap {
            let kind = cells[head].kind.as_u8() as usize;
            let (px, py) = cells[head].offset;
            let depth = depths[head];
            let from = froms[head];
            let Some(row) = self.rules.get(depth) else {
                break;
            };
            for (d, (dx, dy)) in DIRS.iter().enumerate() {
                if cells.len() >= cap {
                    break;
                }
                let Some(child) = row[kind][from][d] else {
                    continue;
                };
                let next = (px + dx, py + dy);
                if !taken.insert(next) {
                    continue;
                }
                cells.push(Cell::new(child, next));
                depths.push(depth + 1);
                // 子细胞"是从 d 方向生出来的"，所以它自己的极性是 d 的反向。
                froms.push((d + 2) % 4);
            }
            head += 1;
        }
        cells
    }

    /// 突变：改写 `edits` 个规则槽。
    ///
    /// **按"改几个"而不是"每槽掷一次骰子"**：规则表有上千个槽，
    /// 按概率逐个掷的话，一个正常的变异率就会一次改掉几十条规则，
    /// 形状立刻被打散——那不是变异，那是重掷。
    ///
    /// 而且优先改**已经打开的**槽：随机指一个槽的话，绝大多数时候指到的是空槽，
    /// 于是变异只会"无意义地开一条新规则"，而真正在用的那十几条规则永远不动。
    pub fn mutate(&mut self, rng: &mut Rng, edits: usize) {
        for _ in 0..edits {
            // 七成改已有规则，三成试着开一条新的。
            let open = self.open_slots();
            let pick_open = !open.is_empty() && rng.next_f32() < 0.7;
            let (depth, kind, from, d) = if pick_open {
                open[(rng.next_f32() * open.len() as f32) as usize % open.len()]
            } else {
                (
                    (rng.next_f32() * MAX_DEPTH as f32) as usize % MAX_DEPTH,
                    (rng.next_f32() * CellType::COUNT as f32) as usize % CellType::COUNT,
                    (rng.next_f32() * (NO_PARENT + 1) as f32) as usize % (NO_PARENT + 1),
                    (rng.next_f32() * 4.0) as usize % 4,
                )
            };
            let slot = &mut self.rules[depth][kind][from][d];
            *slot = match *slot {
                Some(_) if rng.next_f32() < 0.3 => None,
                Some(_) => Some(random_kind(rng)),
                None => Some(random_kind(rng)),
            };
        }
        // 体型上限也会漂：它是"长到多大就不长了"的那条线。
        if rng.next_f32() < 0.05 * edits as f32 {
            let step = if rng.next_f32() < 0.5 { 1i16 } else { -1 };
            self.max_cells = (self.max_cells as i16 + step).clamp(1, 24) as u8;
        }
    }

    /// 所有已经打开的规则槽。
    fn open_slots(&self) -> Vec<(usize, usize, usize, usize)> {
        let mut out = Vec::new();
        for (depth, row) in self.rules.iter().enumerate() {
            for (kind, by_from) in row.iter().enumerate() {
                for (from, by_dir) in by_from.iter().enumerate() {
                    for (d, slot) in by_dir.iter().enumerate() {
                        if slot.is_some() {
                            out.push((depth, kind, from, d));
                        }
                    }
                }
            }
        }
        out
    }

    /// 有多少条规则是打开的（调试与统计用）。
    pub fn rule_count(&self) -> usize {
        self.open_slots().len()
    }
}

fn random_kind(rng: &mut Rng) -> CellType {
    // 只从"能干活"的类型里挑：构造器还没接上，长出来只是白付维持成本。
    const POOL: [CellType; 5] = [
        CellType::Producer,
        CellType::Mover,
        CellType::Mouth,
        CellType::Armor,
        CellType::Killer,
    ];
    POOL[(rng.next_f32() * POOL.len() as f32) as usize % POOL.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::PLANS;

    /// 生长必须是**确定的**：同一个基因组，两次发育结果一模一样。
    ///
    /// 这条看着像废话，但它是"遗传"的全部前提。发育里只要掺进一个
    /// 与时间或地址相关的随机数，父子就不像了，选择也就无从谈起。
    #[test]
    fn development_is_deterministic() {
        for plan in PLANS {
            let grn = Grn::from_plan(plan).unwrap();
            assert_eq!(grn.develop(), grn.develop());
        }
    }

    /// 手工体型能被反推成生长程序，而且长回来**一模一样**。
    ///
    /// 这是 3b 的热启动：不从零开始摸索，而是从五个已知能活的形状出发。
    /// 只要这条测试绿着，"形态发生"和"手工体型"就是同一套东西的两种写法。
    #[test]
    fn every_plan_can_be_grown_from_rules() {
        for plan in PLANS {
            let grn = Grn::from_plan(plan).unwrap_or_else(|| {
                panic!(
                    "体型「{}」用规则表表达不出来——\
                     同一个(代, 细胞类型, 极性, 方向)在不同位置需要长出不同的东西。",
                    plan.name
                )
            });
            let grown = grn.develop();
            assert_eq!(grown.len(), plan.len(), "「{}」格数不对", plan.name);
            for (k, x, y) in plan.cells {
                assert!(
                    grown.iter().any(|c| c.kind == *k && c.offset == (*x, *y)),
                    "「{}」少了 ({x}, {y}) 上的 {k:?}",
                    plan.name
                );
            }
        }
    }

    /// 身体必须连通，而且锚点上一定有细胞。
    #[test]
    fn every_developed_body_is_connected_and_anchored() {
        for plan in PLANS {
            let grn = Grn::from_plan(plan).unwrap();
            let cells = grn.develop();
            assert!(cells.iter().any(|c| c.offset == (0, 0)), "锚点空了");

            let set: HashSet<(i16, i16)> = cells.iter().map(|c| c.offset).collect();
            let mut seen = HashSet::new();
            let mut stack = vec![(0i16, 0i16)];
            seen.insert((0, 0));
            while let Some((x, y)) = stack.pop() {
                for (dx, dy) in DIRS {
                    let n = (x + dx, y + dy);
                    if set.contains(&n) && seen.insert(n) {
                        stack.push(n);
                    }
                }
            }
            assert_eq!(seen.len(), set.len(), "身体散成了几块");
        }
    }

    /// 上限真的会生效——没有它，一条"往右长"的规则会长到天上去。
    #[test]
    fn max_cells_is_respected() {
        let mut grn = Grn::from_plan(&PLANS[0]).unwrap();
        grn.max_cells = 4;
        assert_eq!(grn.develop().len(), 4);

        grn.max_cells = 1;
        assert_eq!(grn.develop().len(), 1, "上限是 1 时只该有根细胞");
    }

    /// 突变会改到东西，而且改完还是个合法的基因组（能发育、锚点在、不超上限）。
    #[test]
    fn mutation_edits_the_rules_but_keeps_them_usable() {
        let base = Grn::from_plan(&PLANS[0]).unwrap();
        let mut rng = Rng::new(99);
        let mut changed = 0;
        for _ in 0..40 {
            let mut g = base.clone();
            g.mutate(&mut rng, 2);
            if g != base {
                changed += 1;
            }
            let cells = g.develop();
            assert!(!cells.is_empty(), "发育不能长不出东西");
            assert!(cells.len() <= g.max_cells as usize);
            assert!(cells.iter().any(|c| c.offset == (0, 0)));
        }
        assert!(changed > 30, "40 次突变里只有 {changed} 次改到了东西");
    }

    /// 一次突变**不会**把形状整个打散。
    ///
    /// 这条是"改 K 个槽"而不是"每槽掷骰子"的理由：规则表有上千个槽，
    /// 按概率逐个掷的话，正常变异率会一次改掉几十条规则，身体立刻变成随机团。
    #[test]
    fn a_single_mutation_keeps_most_of_the_body() {
        let base = Grn::from_plan(&PLANS[1]).unwrap();
        let before = base.develop().len();
        let mut rng = Rng::new(7);
        for _ in 0..30 {
            let mut g = base.clone();
            g.mutate(&mut rng, 1);
            let after = g.develop().len();
            assert!(
                before.abs_diff(after) <= 4,
                "改一条规则就让身体从 {before} 格变成 {after} 格——变异太猛了"
            );
        }
    }
}
