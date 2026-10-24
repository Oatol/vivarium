//! 形态解释器：把基因组翻译成**人能读的身体**。
//!
//! ## 为什么需要它
//!
//! 三个理由，一条比一条硬：
//!
//! 1. **可解释性**。从基因组读不出任何东西——"你没法从权重看出这是只捕食者"。
//! 2. **验收**。"能长出分节吗""改一个基因影响多少格""表型变异是低维还是高维"
//!    这些问题**全部**需要一个能把基因组批量解释成形状的东西。
//! 3. **调试**。发育 bug 的典型表现是"身体偶尔长成一条线"，取决于迭代顺序和浮点，
//!    极难复现。解释器能把"这一次发育的每一步"摊开。
//!
//! ## 它是纯函数
//!
//! 不跑 tick、不碰世界、不看时间。输入一个 [`Genome`]，输出一份 [`Anatomy`]。
//! 所以它能在测试里跑几千次，也能在没有 GPU 的机器上跑。
//!
//! ## 它现在解释的是 `Grn`，将来解释的是 `Segment` 树
//!
//! 接口（[`interpret`] → [`Anatomy`]）是**按将来的样子定的**：
//! M2b 把 `Grn` 换成 `Segment` + `Gradient` 之后，只有 [`interpret`] 的实现要改，
//! 调用方一行都不用动。`segments` 那个字段现在永远只有一段（整具身体），
//! 换完之后才会真的填上"躯干 / 附肢 / 附肢上的附肢"。

use std::collections::{HashMap, HashSet, VecDeque};

use crate::life::{Cell, CellType, Genome};
use crate::rng::Rng;

/// 段嵌套的最大深度。
///
/// 深度 1 = 躯干；深度 2 = 躯干 + 附肢；深度 3 = 附肢上再长附肢。
/// **不设限会让 `Segment` 树变成事实上的 L-system**，而那样变异的影响范围
/// 会从"一个段"回升到"整个身体"——等于白付了有界语法的代价。
pub const MAX_SEGMENT_DEPTH: u8 = 3;

/// 一段在 [`Anatomy`] 里的位置。
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentSpan {
    /// 嵌套深度。0 = 主干。
    pub depth: u8,
    /// 这一段的格在 `Anatomy::cells` 里的下标区间 `[start, end)`。
    pub range: (usize, usize),
    /// 这一段的格数（含重复份与镜像份）。
    pub cells: usize,
    /// 沿主轴的位置区间 `[0, 1]`。
    pub along: (f32, f32),
}

/// 一次发育的结果，加上人能读的解读。
#[derive(Debug, Clone)]
pub struct Anatomy {
    /// 展开后的细胞，按 `(y, x)` 排序。
    pub cells: Vec<Cell>,
    /// 分节。**现在永远只有一段**（`Grn` 没有段的概念），M2b 之后才会有真段。
    pub segments: Vec<SegmentSpan>,
    /// 包围盒 `(x0, y0, x1, y1)`，闭区间。空身体返回 `(0, 0, 0, 0)`。
    pub bounds: (i16, i16, i16, i16),
    /// 4-邻接连通分量数。**> 1 就是身体散架了**。
    pub components: usize,
}

/// 一个基因位点的影响力。
#[derive(Debug, Clone)]
pub struct SlotReach {
    /// 位点的可读名字，比如 `rules[0][3][5][1]`。
    pub name: String,
    /// 微扰它之后，平均有多少比例的身体格变了。
    pub reach: f32,
}

/// 变异敏感度：改一个基因位点，表型动多少格。
///
/// **这是"模块化"和"可演化性"的直接度量**：可演化性 ≈ 1 / (影响范围的方差)。
///
/// 一条改动如果影响 81% 的身体，它几乎不可能爬坡——绝大多数改动都会把身体弄坏。
/// 如果只影响 20%，那 80% 的身体是"安全的背景"，变异可以在上面小步试探。
#[derive(Debug, Clone)]
pub struct Sensitivity {
    /// 每个采样位点的数据。
    pub per_slot: Vec<SlotReach>,
    /// 全体平均。**这就是那条判据**：目标 < 30%。
    pub mean_reach: f32,
    /// 影响范围的标准差。越大越难爬坡（同一个基因组里有的位点全局、有的无效）。
    pub spread: f32,
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

/// 把一个基因组发育成身体，并解读它。
///
/// **确定性**：同一个基因组必然给出同一份 [`Anatomy`]——这里不掷任何骰子，
/// 随机数只在 [`sensitivity`] 里用，而且它不改基因组。
pub fn interpret(genome: &Genome) -> Anatomy {
    let mut cells = genome.body.develop();
    // 排序让输出稳定：`develop` 是 BFS 顺序，同一层里取决于方向枚举顺序，
    // 拿它直接画图会得到"看起来随机"的行列。
    cells.sort_by_key(|c| (c.offset.1, c.offset.0));

    let bounds = bounds_of(&cells);
    let components = count_components(&cells);
    // `Grn` 没有段的概念，整具身体算一段。M2b 换成 `Segment` 树之后，
    // 这里会按 `expand()` 时的 `depth` 切分。
    let segments = if cells.is_empty() {
        Vec::new()
    } else {
        vec![SegmentSpan {
            depth: 0,
            range: (0, cells.len()),
            cells: cells.len(),
            along: (0.0, 1.0),
        }]
    };

    Anatomy {
        cells,
        segments,
        bounds,
        components,
    }
}

impl Anatomy {
    /// 一行摘要：`14 格 · 1 段 · 包围盒 7×2 · 连通 · 叶绿体 7 · 鞭毛 6 · 消化 1`
    pub fn summary(&self) -> String {
        if self.cells.is_empty() {
            return "空身体".to_string();
        }
        let (x0, y0, x1, y1) = self.bounds;
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        let conn = if self.components > 1 {
            format!("**散架成 {} 块**", self.components)
        } else {
            "连通".to_string()
        };
        let comp = self
            .composition()
            .into_iter()
            .map(|(name, n)| format!("{name} {n}"))
            .collect::<Vec<_>>()
            .join(" · ");
        format!(
            "{} 格 · {} 段 · 包围盒 {}×{} · {} · {}",
            self.cells.len(),
            self.segments.len(),
            w,
            h,
            conn,
            comp
        )
    }

    /// 字符画。每个字符是这一格的细胞类型。
    ///
    /// `P` 生产者 · `M` 移动 · `D` 嘴 · `E` 眼 · `S` 杀手 · `A` 装甲 · `B` 建造 · `.` 空
    pub fn ascii(&self) -> String {
        if self.cells.is_empty() {
            return "(空)".to_string();
        }
        let (x0, y0, x1, y1) = self.bounds;
        let (w, h) = ((x1 - x0 + 1) as usize, (y1 - y0 + 1) as usize);
        let mut grid = vec![b'.'; w * h];
        for c in &self.cells {
            let x = (c.offset.0 - x0) as usize;
            let y = (c.offset.1 - y0) as usize;
            if x < w && y < h {
                grid[y * w + x] = glyph(c.kind);
            }
        }
        (0..h)
            .map(|y| String::from_utf8_lossy(&grid[y * w..(y + 1) * w]).into_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 沿主轴的剖面：每一列有几格、主要是什么类型。
    ///
    /// 摘要和字符画看的是"形状"，剖面看的是"**沿身体的分布**"——
    /// 一条头和尾都是生产者的鱼，和一个头是嘴、尾是生产者的鱼，
    /// 字符画能看出来但一眼分不清，剖面能。
    pub fn profile(&self) -> String {
        if self.cells.is_empty() {
            return "(空)".to_string();
        }
        let (x0, _, x1, _) = self.bounds;
        let mut cols: Vec<Vec<CellType>> = vec![Vec::new(); (x1 - x0 + 1) as usize];
        for c in &self.cells {
            let i = (c.offset.0 - x0) as usize;
            if i < cols.len() {
                cols[i].push(c.kind);
            }
        }
        cols.iter()
            .enumerate()
            .map(|(i, kinds)| {
                let main = kinds
                    .iter()
                    .fold(HashMap::<u8, usize>::new(), |mut m, k| {
                        *m.entry(k.as_u8()).or_default() += 1;
                        m
                    })
                    .into_iter()
                    .max_by_key(|&(_, n)| n)
                    .map(|(k, _)| glyph(CellType::from_u8(k)) as char)
                    .unwrap_or('.');
                format!("x+{i} {}格 {main}", kinds.len())
            })
            .collect::<Vec<_>>()
            .join("   ")
    }

    /// 构成统计：每种细胞类型有几格，按格数降序。
    pub fn composition(&self) -> Vec<(String, usize)> {
        let mut m: HashMap<u8, usize> = HashMap::new();
        for c in &self.cells {
            *m.entry(c.kind.as_u8()).or_default() += 1;
        }
        let mut v: Vec<(String, usize)> = m
            .into_iter()
            .map(|(k, n)| (cell_name(CellType::from_u8(k)).to_string(), n))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    /// 完整报告：摘要 + 字符画 + 剖面。
    ///
    /// 给工坊和 CLI 用。测试里用单个方法更好——报告排版一改，断言就会红。
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("身体：{}\n", self.summary()));
        if !self.cells.is_empty() {
            out.push_str("形状：\n");
            for line in self.ascii().lines() {
                out.push_str(&format!("  {line}\n"));
            }
            out.push_str(&format!("剖面：{}\n", self.profile()));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 变异敏感度
// ---------------------------------------------------------------------------

/// 量一次"改一个基因位点会动多少格"。
///
/// ## 为什么这个数是可演化性的核心
///
/// 一个位点如果影响 100% 的身体，那它**只能**被用来做"整体缩放"——
/// 任何方向性的改动都会同时改掉所有器官，几乎必死。
/// 如果只影响 20%，剩下 80% 是安全背景，变异可以小步试探。
///
/// 所以：**可演化性 ≈ 1 / (影响范围的方差)**。方差小 = 每个位点都只动一小块。
///
/// ## 采样的理由
///
/// `Grn` 有 2240 个槽，逐个量要跑 2240 × trials 次发育。这里只采样
/// `samples` 个已打开的槽（那些才是真正在用的），加上若干空槽做对照。
pub fn sensitivity(genome: &Genome, rng: &mut Rng, trials: usize, samples: usize) -> Sensitivity {
    let base = interpret(genome);
    let base_sig = signature(&base);
    let total = base.cells.len().max(1);
    if total == 0 || samples == 0 {
        return Sensitivity {
            per_slot: Vec::new(),
            mean_reach: 0.0,
            spread: 0.0,
        };
    }

    // 对每一次"随机变异一次"，量它动了多少格。
    //
    // **这和"每一个基因位点"不是同一个东西**：`Body` 的变异算子是随机挑
    // 位置、随机挑种类的，所以这里量的是"一次典型变异的影响范围"，
    // 而 `Grn` 时代量的是"某个具体规则槽的影响范围"。
    // 前者更接近"可演化性"真正关心的东西——进化不知道自己要动哪个槽。
    let mut per_slot = Vec::new();
    for i in 0..samples {
        let mut sum = 0usize;
        for _ in 0..trials {
            let mut g = genome.clone();
            g.body.mutate(rng, 1);
            let a = interpret(&g);
            sum += diff_count(&base_sig, &signature(&a));
        }
        per_slot.push(SlotReach {
            name: format!("mutate #{i}"),
            reach: sum as f32 / (trials.max(1) as f32 * total as f32),
        });
    }

    let n = per_slot.len().max(1) as f32;
    let mean = per_slot.iter().map(|s| s.reach).sum::<f32>() / n;
    let var = per_slot
        .iter()
        .map(|s| (s.reach - mean).powi(2))
        .sum::<f32>()
        / n;
    Sensitivity {
        per_slot,
        mean_reach: mean,
        spread: var.sqrt(),
    }
}

impl Sensitivity {
    /// 一行摘要：`平均 81% 格受影响（spread 0.42）`
    pub fn summary(&self) -> String {
        format!(
            "平均 {:.0}% 格受影响（spread {:.2}）",
            self.mean_reach * 100.0,
            self.spread
        )
    }
}

// ---------------------------------------------------------------------------
// 内部
// ---------------------------------------------------------------------------

fn glyph(k: CellType) -> u8 {
    match k {
        CellType::Producer => b'P',
        CellType::Mover => b'M',
        CellType::Mouth => b'D',
        CellType::Eye => b'E',
        CellType::Killer => b'S',
        CellType::Armor => b'A',
        CellType::Builder => b'B',
        CellType::Root => b'R',
    }
}

fn cell_name(k: CellType) -> &'static str {
    match k {
        CellType::Producer => "生产者",
        CellType::Mover => "移动",
        CellType::Mouth => "嘴",
        CellType::Eye => "眼",
        CellType::Killer => "杀手",
        CellType::Armor => "装甲",
        CellType::Builder => "建造",
        CellType::Root => "根",
    }
}

fn bounds_of(cells: &[Cell]) -> (i16, i16, i16, i16) {
    if cells.is_empty() {
        return (0, 0, 0, 0);
    }
    let mut b = (i16::MAX, i16::MAX, i16::MIN, i16::MIN);
    for c in cells {
        b.0 = b.0.min(c.offset.0);
        b.1 = b.1.min(c.offset.1);
        b.2 = b.2.max(c.offset.0);
        b.3 = b.3.max(c.offset.1);
    }
    b
}

/// 4-邻接连通分量数。
///
/// 这条数"身体是不是一块"：`develop()` 理论上不会产出不连通的格子
/// （每一格都是从已存在的格子旁边长出来的），但**镜像 + 重复**那套代码
/// 一写错就会产出一个"浮在旁边的附肢"。所以这条要有。
fn count_components(cells: &[Cell]) -> usize {
    if cells.is_empty() {
        return 0;
    }
    let set: HashSet<(i16, i16)> = cells.iter().map(|c| c.offset).collect();
    let mut seen: HashSet<(i16, i16)> = HashSet::new();
    let mut n = 0;
    for &start in &set {
        if seen.contains(&start) {
            continue;
        }
        n += 1;
        let mut q = VecDeque::new();
        q.push_back(start);
        seen.insert(start);
        while let Some((x, y)) = q.pop_front() {
            for (dx, dy) in [(1i16, 0i16), (-1, 0), (0, 1), (0, -1)] {
                let p = (x + dx, y + dy);
                if set.contains(&p) && seen.insert(p) {
                    q.push_back(p);
                }
            }
        }
    }
    n
}

fn signature(a: &Anatomy) -> Vec<(i16, i16, u8)> {
    let mut v: Vec<(i16, i16, u8)> = a
        .cells
        .iter()
        .map(|c| (c.offset.0, c.offset.1, c.kind.as_u8()))
        .collect();
    v.sort_unstable();
    v
}

/// 两份指纹有多少格不同（按位置 + 类型配对比较）。
fn diff_count(a: &[(i16, i16, u8)], b: &[(i16, i16, u8)]) -> usize {
    let sa: HashSet<(i16, i16, u8)> = a.iter().copied().collect();
    let sb: HashSet<(i16, i16, u8)> = b.iter().copied().collect();
    // 对称差：删掉的格 + 新增的格 + 变了的格（变了 = 旧的一格不在新集合里）
    sa.symmetric_difference(&sb).count()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::PLANS;

    fn from_plan(plan: &crate::body::BodyPlan) -> Genome {
        let mut g = Genome::blank();
        g.body = crate::morph::Body::from_plan(plan);
        g
    }

    #[test]
    fn every_plan_interprets_back_to_itself() {
        // 解释器是"基因组 → 形状"的另一条读法。如果它读出来的形状
        // 和手工体型对不上，那它量的一切（敏感度、构成）都是错的。
        for plan in PLANS {
            let a = interpret(&from_plan(plan));
            assert_eq!(
                a.cells.len(),
                plan.len(),
                "「{}」解释出来 {} 格，实际 {} 格",
                plan.name,
                a.cells.len(),
                plan.len()
            );
            assert_eq!(a.components, 1, "「{}」的身体不连通", plan.name);
            // 构成也要对得上。
            for kind in CellType::ALL {
                let want = plan.count(kind);
                let got = a
                    .cells
                    .iter()
                    .filter(|c| c.kind == kind)
                    .count();
                assert_eq!(got, want, "「{}」的 {:?} 数量不对", plan.name, kind);
            }
        }
    }

    #[test]
    fn ascii_has_the_right_shape() {
        // 银鱼是 10 格、5 宽 3 高。画出来必须是 3 行 5 列。
        let a = interpret(&from_plan(&crate::body::SILVERFISH));
        let art = a.ascii();
        let lines: Vec<&str> = art.lines().collect();
        assert_eq!(lines.len(), 3, "字符画的行数不对：{lines:?}");
        for l in &lines {
            assert_eq!(l.chars().count(), 5, "字符画的列数不对：{l}");
        }
        // 至少有一个 P（生产者）和一个 M（移动）。
        let s = a.ascii();
        assert!(s.contains('P'), "字符画里没有生产者：\n{s}");
        assert!(s.contains('M'), "字符画里没有移动：\n{s}");
    }

    #[test]
    fn summary_mentions_what_matters() {
        let a = interpret(&from_plan(&crate::body::SILVERFISH));
        let s = a.summary();
        assert!(s.contains("10 格"), "摘要里没有格数：{s}");
        assert!(s.contains("连通"), "摘要里没有连通性：{s}");
        assert!(s.contains("生产者"), "摘要里没有构成：{s}");
    }

    #[test]
    fn a_broken_body_reports_more_than_one_component() {
        // 手工造一个"浮在旁边的附肢"：两格不相邻。
        let a = Anatomy {
            cells: vec![
                Cell::new(CellType::Producer, (0, 0)),
                Cell::new(CellType::Producer, (5, 0)),
            ],
            segments: Vec::new(),
            bounds: bounds_of(&[
                Cell::new(CellType::Producer, (0, 0)),
                Cell::new(CellType::Producer, (5, 0)),
            ]),
            components: count_components(&[
                Cell::new(CellType::Producer, (0, 0)),
                Cell::new(CellType::Producer, (5, 0)),
            ]),
        };
        assert_eq!(a.components, 2, "两格不相邻应当报 2 块");
        assert!(a.summary().contains("散架"), "散架没有报出来：{}", a.summary());
    }

    #[test]
    fn sensitivity_is_reproducible_with_the_same_rng() {
        // 敏感度是一个**统计量**，但同一个种子必须给同一个数——
        // 否则它不能进测试，也就不能当判据。
        let g = from_plan(&crate::body::SILVERFISH);
        let a = sensitivity(&g, &mut Rng::new(7), 4, 12);
        let b = sensitivity(&g, &mut Rng::new(7), 4, 12);
        assert_eq!(a.per_slot.len(), b.per_slot.len());
        for (x, y) in a.per_slot.iter().zip(&b.per_slot) {
            assert_eq!(x.name, y.name);
            assert!((x.reach - y.reach).abs() < 1e-6, "同种子给出不同的数");
        }
    }

    #[test]
    fn reach_is_a_fraction() {
        let g = from_plan(&crate::body::SILVERFISH);
        let s = sensitivity(&g, &mut Rng::new(11), 4, 16);
        assert!(!s.per_slot.is_empty(), "一个位点都没采到");
        for slot in &s.per_slot {
            assert!(
                (0.0..=1.0).contains(&slot.reach),
                "reach 越界：{} = {}",
                slot.name,
                slot.reach
            );
        }
        assert!((0.0..=1.0).contains(&s.mean_reach), "均值越界");
    }
}
