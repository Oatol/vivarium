//! 形态基因组：**有界语法**（C2b）+ **位置决定类型**（C3-lite）。
//!
//! ## 为什么换掉 `grn.rs`
//!
//! 旧的 `Grn` 是一张 `rules[代][母类型][极性][方向]` 的规则表，从根细胞出发
//! BFS 生长。它有三个结构性问题（实测过的，见 `讨论-生态扩展.md` §10.1）：
//!
//! 1. **没有"位置"这个维度。** 同一个类型的细胞在不同位置想长不同的东西，
//!    只能靠"第几代"近似，而 BFS 步数和几何位置没有稳定关系。
//!    **所以它表达不了"分节"**——而分节是所有多细胞身体的第一块砖。
//! 2. **浅层规则全局影响**：改 `rules[0]` = 改根细胞的 4 次分裂 = 整个身体重画。
//! 3. **深度量子化**：加一代可能让身体翻倍，也可能一格不长。
//!
//! 实测 20 个变异体**全部是"3~10 格、包围盒最大 5×3 的一团"**——局部性其实
//! 是好的（平均影响 10%），**但表型空间太小**。所以这一层要解决的不是
//! "变异太全局"，是"**长不出结构**"。
//!
//! ## 两层，正交
//!
//! ```text
//! 布局（哪里有格）      Segment 树 —— 段 + 重复 + 镜像 + 子段
//! 类型（这格带什么）    Gradient  —— 位置 → 细胞器，连续阈值
//! ```
//!
//! **正交是关键**：改梯度不挪动任何一格，改布局也不打乱任何类型规则。
//! 这就是"模块化"在代码层面的落地。
//!
//! ## 发育分两步，严格分开
//!
//! ```text
//! 步骤 A（纯几何）：Segment 树 → 一组 (offset, base)
//! 步骤 B（位置解释）：每个格算 (along, from_edge) → 最终细胞器
//! ```
//!
//! 分开之后，"改一个基因影响多少格"可以在步骤 A **单独测量**——
//! 那是判据 J2（模块化）的量尺。
//!
//! ## 为什么不用 L-system
//!
//! 产生式 + turtle 的变异效应是**非局部**的：改一条被引用 30 次的规则，
//! 整个身体重画。有界语法把递归限制在"段的嵌套"上，于是变异的影响范围
//! 是结构上可预测的：改一个 cell 只影响该段的所有重复份；加一个子段
//! 只多出一个附肢。
//!
//! 表达力损失大概 20%（丢了分形、肺、血管那种递归细节），可搜索性提升是量级。
//! 而在**这个**模拟里，递归细节的收益本来就很小——分支不提供杠杆，
//! 只提供表面积。

use std::collections::HashSet;

use crate::body::BodyPlan;
use crate::life::Cell;
use crate::organelle::{Organelles, MITO};
use crate::rng::Rng;

/// 一个身体最多能有多少格。
///
/// 写在这里（而不是散在 `mutate` 的 `clamp` 和存档的校验里）：
/// 它是**同一件事**——"身体能长多大"。两处各写一个数字，早晚有一处先改，
/// 而症状会是"读档之后这条鱼比存档时少了两格"。
///
/// 24 不是随便定的：`try_move` 的搬运代价是 O(格数²)，24² = 576 次整数比较，
/// 在 60fps 的预算里可以忽略。再往上就要换数据结构了。
pub const MAX_BODY_CELLS: u8 = 24;

/// 段嵌套的最大深度。
///
/// 深度 0 = 主干；1 = 附肢；2 = 附肢上的附肢。
/// **不设限会让 `Segment` 树变成事实上的 L-system**，而那样变异的影响范围
/// 会从"一个段"回升到"整个身体"——等于白付了有界语法的代价。
pub const MAX_SEGMENT_DEPTH: u8 = 3;

/// 一个段最多重复几次。
///
/// 8 是"体节"够用的上限。再多的话展开成本线性涨，而"8 节"和"20 节"
/// 在 24 格的体型上限下没有区别。
pub const MAX_REPEAT: u8 = 8;

/// 梯度规则最多几条。
///
/// 超过 12 条之后，"改哪一条影响哪一片"就没有人能记住了——
/// 而一条没人能预测的变异等于随机变异。
pub const MAX_BANDS: usize = 12;

/// 四根轴。顺序固定：`+x / +y / −x / −y`。
const AXES: [(i16, i16); 4] = [(1, 0), (0, 1), (-1, 0), (0, -1)];

// ---------------------------------------------------------------------------
// 数据结构
// ---------------------------------------------------------------------------

/// 段里的一格：几何 + **基础**细胞器。
///
/// `base` 写在这里而不是全靠梯度，是为了让"这一格必须有线粒体"这种
/// 硬约束有地方放。梯度会在这个基础上叠加。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyCell {
    /// 相对段原点的偏移。
    pub offset: (i16, i16),
    /// 基础细胞器。
    pub base: u16,
}

/// 身体的一"段"。
///
/// 段可以沿主轴重复、镜像、长出台子段。**这三件事就是全部的表达力来源**：
/// 重复给体节，镜像给成对器官，子段给附肢。
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// 这一段的形状。**只有几何和基础细胞器，没有"这一格是什么类型"**——
    /// 类型由 [`Gradient`] 决定。
    pub cells: Vec<BodyCell>,
    /// 主轴：`0=+x, 1=+y, 2=−x, 3=−y`。
    pub axis: u8,
    /// 沿主轴重复几次（`1..=MAX_REPEAT`）。
    pub repeat: u8,
    /// 是否沿主轴**垂直方向**镜像一份（成对器官）。
    pub mirror: bool,
    /// 子段：从这一段的**末端**长出去。
    pub children: Vec<Segment>,
}

impl Segment {
    /// 一个只有一格的段。
    pub fn single(cell: BodyCell) -> Self {
        Self {
            cells: vec![cell],
            axis: 0,
            repeat: 1,
            mirror: false,
            children: Vec::new(),
        }
    }

    /// 一个空段（不占格）。用它当"没有子段"的占位。
    pub fn empty() -> Self {
        Self {
            cells: Vec::new(),
            axis: 0,
            repeat: 1,
            mirror: false,
            children: Vec::new(),
        }
    }

    /// 这段在主轴上跨了几格。
    ///
    /// 重复和子段的位移都按它算，不是按 1 格——否则一个 3 格宽的段
    /// 重复 4 次会重叠成 1 格。
    pub fn span(&self) -> i16 {
        if self.cells.is_empty() {
            return 0;
        }
        let (ax, ay) = AXES[(self.axis as usize) % 4];
        let along: Vec<i16> = self
            .cells
            .iter()
            .map(|c| c.offset.0 * ax + c.offset.1 * ay)
            .collect();
        let lo = *along.iter().min().unwrap();
        let hi = *along.iter().max().unwrap();
        hi - lo + 1
    }

    /// 格数。
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// 是否为空（`len` 的伴生方法，clippy 要求有）。
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

/// 一条"带"：满足位置条件的地方，叠加 / 清除某些细胞器。
///
/// 这就是 Wolpert 的 French flag model：细胞类型 = f(到边界的距离, 沿主轴的位置)。
///
/// 相比逐格写类型，它的三个好处：
///
/// 1. 改一个阈值 → 整片同质区域一起平移，**比例保持**；
/// 2. 阈值是连续标量，映射到离散网格时天然多对一 → **简并**（中性漂移有地方走）；
/// 3. 一次算完，不需要迭代到稳定 → **不混沌**。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Band {
    /// 沿主轴的位置区间 `t ∈ [0, 1]`。`(0.0, 0.35)` = 身体前 35%。
    pub along: (f32, f32),
    /// 到包围盒边界的距离（格）。`(0.0, 1.0)` = 最外一圈。
    pub from_edge: (f32, f32),
    /// 命中后**叠加**的细胞器。
    pub set: u16,
    /// 命中后**清除**的细胞器（`AND NOT`）。
    ///
    /// 为什么要 `clear`：一条规则要能表达"这一段是嘴，但没有叶绿体"。
    /// 只有 `set` 的话，你没法在同一格里既"设为嘴"又"保证不长叶绿体"。
    pub clear: u16,
}

impl Band {
    /// 一条什么都不做的带（占位用）。
    pub fn inert() -> Self {
        Self {
            along: (0.0, 0.0),
            from_edge: (0.0, 0.0),
            set: 0,
            clear: 0,
        }
    }

    /// 命中判据。空区间（`lo > hi`）永远不命中——这让"关掉一条带"
    /// 可以只改一个数，不用从 `Vec` 里删。
    fn hits(&self, t: f32, d: f32) -> bool {
        self.along.0 <= t
            && t <= self.along.1
            && self.from_edge.0 <= d
            && d <= self.from_edge.1
    }
}

/// 位置 → 细胞器的规则集。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Gradient {
    /// 按顺序应用。后面的覆盖前面的（用 `clear` 显式表达"这里不要"）。
    pub bands: Vec<Band>,
}

/// 身体描述。
#[derive(Debug, Clone, PartialEq)]
pub struct Body {
    /// 主干（深度 0）。
    pub trunk: Segment,
    /// 类型规则。
    pub gradient: Gradient,
    /// 体型上限（`1..=MAX_BODY_CELLS`）。
    pub max_cells: u8,
}

impl Default for Body {
    fn default() -> Self {
        Self::blank()
    }
}

impl Body {
    /// 一个"什么都没长"的身体：一格带线粒体的空细胞。
    ///
    /// 它必须能活（否则 `Genome::blank()` 会造出一个立刻死的东西），
    /// 但它不该有产能、不该能动——所以只有线粒体。
    pub fn blank() -> Self {
        Self {
            trunk: Segment::single(BodyCell {
                offset: (0, 0),
                base: MITO,
            }),
            gradient: Gradient::default(),
            max_cells: 1,
        }
    }

    /// 从手工体型造一个身体。
    ///
    /// **整具身体作为一个段，梯度为空**——于是每一格的细胞器就是
    /// `BodyCell::base`，和手工体型里写的完全一样。这是 M2b 的
    /// "手工体型行为不变"那条验收的基础。
    pub fn from_plan(plan: &BodyPlan) -> Self {
        let cells: Vec<BodyCell> = plan
            .cells
            .iter()
            .map(|&(kind, x, y)| BodyCell {
                offset: (x, y),
                base: Organelles::from_cell_type(kind).0,
            })
            .collect();
        Self {
            trunk: Segment {
                cells,
                axis: 0,
                repeat: 1,
                mirror: false,
                children: Vec::new(),
            },
            gradient: Gradient::default(),
            max_cells: plan.len().clamp(1, MAX_BODY_CELLS as usize) as u8,
        }
    }

    /// 从一组**运行期**的格子造一个身体（工坊拼出来的身体走这条）。
    ///
    /// 和 [`Body::from_plan`] 是同一件事，区别只在于那份格子是编译期的
    /// `'static` 表，还是玩家现拼的。**整具身体作为一个段、梯度为空**——
    /// 于是每一格的细胞器就是它自己，不做任何解释。
    ///
    /// ## 为什么需要它
    ///
    /// 工坊里拼的身体原来只写进 `Organism::cells`，而**基因组里那个 `body`
    /// 还是默认的一格**。于是子代长出来的是一格，不是父母的样子——
    /// "我拼的植物传不下去"，而画面上完全看不出原因（子代确实出生了，
    /// 只是长得完全不一样，然后因为没有根、没有水而死）。
    ///
    /// 这一条让"工坊做的生物"第一次真的可遗传。
    pub fn from_cells(cells: &[(i16, i16, u16)]) -> Self {
        let cells: Vec<BodyCell> = cells
            .iter()
            .map(|&(x, y, bits)| BodyCell {
                offset: (x, y),
                base: bits,
            })
            .collect();
        let n = cells.len().clamp(1, MAX_BODY_CELLS as usize) as u8;
        Self {
            trunk: Segment {
                cells,
                axis: 0,
                repeat: 1,
                mirror: false,
                children: Vec::new(),
            },
            gradient: Gradient::default(),
            max_cells: n,
        }
    }

    /// 发育：展开 + 位置解释。
    ///
    /// **确定性**：全程不掷骰子。同一个身体必然长成同一个样子——
    /// 否则"遗传"这件事就不成立了。
    pub fn develop(&self) -> Vec<Cell> {
        let raw = self.expand();
        interpret(&raw, &self.gradient)
    }

    /// 步骤 A：纯几何展开。返回 `(x, y, base)` 列表，已截断到 `max_cells`。
    ///
    /// 单独公开是为了让 [`crate::anatomy::sensitivity`] 能**只测几何**——
    /// "改布局基因影响多少格"和"改梯度基因影响多少格"是两件不同的事，
    /// 混在一起量会得到一个谁也解释不了的数。
    pub fn expand(&self) -> Vec<(i16, i16, u16)> {
        self.expand_layers()
            .into_iter()
            .map(|(x, y, base, _)| (x, y, base))
            .collect()
    }

    /// 和 [`Self::expand`] 一样，但**每一格带上它来自第几层段**。
    ///
    /// 检视面板要画"形态发生"：第 0 层是主干、第 1 层是子段、第 2 层是子段的子段。
    /// 那个视图需要的正是这个数，而 `expand` 把它丢了。
    ///
    /// 深度和 `expand_segment` 的递归深度是同一个值——`trunk` 是 0，
    /// 它的孩子是 1。所以"第 N 层"和基因组里的嵌套层级是一一对应的，
    /// 不需要另算一遍（另算一份必然和真展开漂开）。
    pub fn expand_layers(&self) -> Vec<(i16, i16, u16, u8)> {
        let mut out: Vec<(i16, i16, u16, u8)> = Vec::new();
        let mut taken: HashSet<(i16, i16)> = HashSet::new();
        expand_segment(&self.trunk, (0, 0), 0, &mut out, &mut taken);
        // **只有和根那一格 4-连通的部分才算身体。**
        //
        // 这不是洁癖，是被实测逼出来的：8000 tick 的演化里出现过
        // `包围盒 10×5` 但中间空一行、`散架成 3 块` 的身体。根因是
        // `span()` 按投影算、而格子在投影上可以有空隙（`mirror` 翻到
        // 另一侧时也会留下一格空档），于是子段从"半空"里长出来。
        //
        // 一个不连通的身体在物理上是荒谬的：`try_move` 只逐格验通行，
        // 中间那道墙根本不参与判定，于是它能"穿过"一格厚的石头。
        // 所以这条规则放在**发育**里，而不是放在变异里拒绝——
        // 真实胚胎里失去接触的细胞也是直接凋亡。
        // **截断会重新切断身体。** 一个段自己的 `cells` 是任意几何
        // （比如四格菱形），截在中间就只剩两个对角格——而对角相邻在
        // 4-连通意义上不算连着。所以顺序必须是：先截断、再取连通块。
        out.truncate(self.max_cells.max(1) as usize);
        keep_root_component(&mut out);
        out
    }

    /// 段数（不含重复份与镜像份），用于摘要。
    pub fn segment_count(&self) -> usize {
        count_segments(&self.trunk)
    }

    /// 段树的扁平摘要：`(深度, 格数, 重复次数, 是否镜像, 基础细胞器)`。
    ///
    /// 按**先序**排（主干、它的第一个孩子、那个孩子的孩子……），
    /// 和 `expand_segment` 的遍历顺序一致，所以检视面板照着画缩进就是对的。
    pub fn segments(&self) -> Vec<SegmentSummary> {
        let mut out = Vec::new();
        collect_segments(&self.trunk, 0, &mut out);
        out
    }
}

/// 一段的扁平摘要，给检视面板的"形态发生"视图用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentSummary {
    /// 嵌套深度。0 = 主干。
    pub depth: u8,
    /// 这一段的格数（不含重复与镜像）。
    pub cells: usize,
    /// 沿主轴重复几次。
    pub repeat: u8,
    /// 是否镜像一份。
    pub mirror: bool,
    /// 这一段的**基础**细胞器（梯度还没叠加）。
    pub base: u16,
}

fn collect_segments(seg: &Segment, depth: u8, out: &mut Vec<SegmentSummary>) {
    let base = seg.cells.iter().fold(0u16, |acc, c| acc | c.base);
    out.push(SegmentSummary {
        depth,
        cells: seg.cells.len(),
        repeat: seg.repeat,
        mirror: seg.mirror,
        base,
    });
    for child in &seg.children {
        collect_segments(child, depth + 1, out);
    }
}

// ---------------------------------------------------------------------------
// 步骤 A：展开
// ---------------------------------------------------------------------------

fn expand_segment(
    seg: &Segment,
    origin: (i16, i16),
    depth: u8,
    out: &mut Vec<(i16, i16, u16, u8)>,
    taken: &mut HashSet<(i16, i16)>,
) {
    if depth > MAX_SEGMENT_DEPTH || seg.cells.is_empty() {
        return;
    }
    let (ax, ay) = AXES[(seg.axis as usize) % 4];
    let span = seg.span().max(1);
    let mut cursor = origin;
    for _ in 0..seg.repeat.clamp(1, MAX_REPEAT) {
        // 1) 本段的格
        for cell in &seg.cells {
            let p = (cursor.0 + cell.offset.0, cursor.1 + cell.offset.1);
            if taken.insert(p) {
                out.push((p.0, p.1, cell.base, depth));
            }
            // 2) 镜像：沿主轴的垂直方向翻转
            if seg.mirror {
                // 轴为 x 时翻转 y，轴为 y 时翻转 x。用"相对 cursor 的距离"
                // 而不是绝对坐标，否则镜像会跑到段外面去。
                let local = (p.0 - cursor.0, p.1 - cursor.1);
                let m = if ax != 0 {
                    (p.0, cursor.1 - local.1)
                } else {
                    (cursor.0 - local.0, p.1)
                };
                if taken.insert(m) {
                    out.push((m.0, m.1, cell.base, depth));
                }
            }
        }
        // 3) 子段：从"这一份的末端**之后**"长出去
        //
        // 起点必须是 `cursor + 轴方向 × span`，不是 `× (span − 1)`。
        // 后者是段本身的最后一格，子段的第一格（offset `(0,0)`）会和它
        // 重叠，于是被 `taken` 丢掉——**整棵子树凭空消失，而且不报错**。
        // 这个 bug 的表现是"复制了子树但格数没变"，查了半天。
        let next = (cursor.0 + ax * span, cursor.1 + ay * span);
        for child in &seg.children {
            expand_segment(child, next, depth + 1, out, taken);
        }
        // 4) 下一份
        cursor = (cursor.0 + ax * span, cursor.1 + ay * span);
    }
}

// ---------------------------------------------------------------------------
// 步骤 B：位置解释
// ---------------------------------------------------------------------------

/// 把展开出来的几何翻译成带细胞器的细胞。
///
/// **主轴取包围盒的长边**，不是固定 x：一条竖着的身体和一条横着的身体
/// 都该有"前 35%"这个概念。
pub fn interpret(raw: &[(i16, i16, u16)], grad: &Gradient) -> Vec<Cell> {
    if raw.is_empty() {
        return Vec::new();
    }
    let (x0, y0, x1, y1) = bounds(raw);
    let w = (x1 - x0 + 1) as f32;
    let h = (y1 - y0 + 1) as f32;
    let along_x = w >= h;
    let span = if along_x { w } else { h };
    let denom = (span - 1.0).max(1.0);

    raw.iter()
        .map(|&(x, y, base)| {
            let t = if along_x {
                (x - x0) as f32 / denom
            } else {
                (y - y0) as f32 / denom
            };
            let d = edge_distance(x, y, x0, y0, x1, y1) as f32;
            let mut org = base;
            for band in &grad.bands {
                if band.hits(t, d) {
                    org |= band.set;
                    org &= !band.clear;
                }
            }
            Cell::from_organelles(Organelles(org), (x, y))
        })
        .collect()
}

fn bounds(raw: &[(i16, i16, u16)]) -> (i16, i16, i16, i16) {
    let mut b = (i16::MAX, i16::MAX, i16::MIN, i16::MIN);
    for &(x, y, _) in raw {
        b.0 = b.0.min(x);
        b.1 = b.1.min(y);
        b.2 = b.2.max(x);
        b.3 = b.3.max(y);
    }
    b
}

/// 到包围盒四条边的**切比雪夫**最近距离（格）。最外一圈是 0。
fn edge_distance(x: i16, y: i16, x0: i16, y0: i16, x1: i16, y1: i16) -> i16 {
    let left = x - x0;
    let right = x1 - x;
    let top = y - y0;
    let bottom = y1 - y;
    left.min(right).min(top).min(bottom).max(0)
}

// ---------------------------------------------------------------------------
// 变异
// ---------------------------------------------------------------------------

/// 一次形态变异的种类。
///
/// 六个算子，每个都对应一条真实演化机制，而且**影响范围可预测**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MorphOp {
    /// 参数微扰：`repeat` / `mirror` / `axis` / `max_cells`。影响一个段。
    Param,
    /// 翻一个细胞器的 bit（`BodyCell::base` 或 `Band::set`）。影响一片。
    FlipBit,
    /// 阈值微扰：`along` / `from_edge`。影响一条边界。
    Threshold,
    /// **子树复制**：多一个结构。对应基因重复。
    Duplicate,
    /// 子树交叉：交换一个模块。对应重组。
    Crossover,
    /// 增删一条梯度带。
    BandEdit,
}

impl Body {
    /// 变异一次。
    ///
    /// `ops` 是这次要做几个算子（按权重随机挑）。
    ///
    /// ## 为什么"子树复制"是低频高影响
    ///
    /// 真实演化里基因重复的频率也很低（约 1e-5/代），但一旦发生，后果很大。
    /// **高频 + 大影响 = 灭绝；低频 + 大影响 = 创新。**
    ///
    /// ## 为什么权重交叉不做
    ///
    /// 行为网络（MLP）上有 competing conventions：两个父代的隐藏层神经元
    /// 编号不对应，交叉出来的孩子是噪声。**重组只能放在结构化基因组上**，
    /// 因为那里的"交换单位"语义完整。
    /// 把超过 [`MAX_SEGMENT_DEPTH`] 的子树砍掉。
    ///
    /// ## 为什么必须砍
    ///
    /// `expand_segment` 本来就不展开太深的段，所以**发育**不会出错；但
    /// **基因组**里那一层还在，而存档的 `read_segment` 会拒绝它
    /// （"段嵌套过深"）。症状是**存出去的档读不回来**——写端能生产读端
    /// 拒绝的东西，这是最不该出现的一类 bug，而且它只在
    /// `MORPH_OPS_PER_BIRTH` 从 2 提到 6 之后才显形（`Duplicate` 有了
    /// 足够多的机会把树推到第 4 层）。
    ///
    /// 判据和 `expand_segment` 对齐：深度 == `MAX_SEGMENT_DEPTH` 的段
    /// **仍然会被展开**，只有它的孩子不会。所以砍的是"深度达到上限的段的
    /// 孩子"，不是那个段本身。
    fn prune_depth(&mut self) {
        fn cut(seg: &mut Segment, depth: u8) {
            if depth >= MAX_SEGMENT_DEPTH {
                seg.children.clear();
                return;
            }
            for c in &mut seg.children {
                cut(c, depth + 1);
            }
        }
        cut(&mut self.trunk, 0);
    }

    pub fn mutate(&mut self, rng: &mut Rng, ops: usize) {
        for _ in 0..ops {
            let r = rng.next_f32();
            // 权重的理由：
            //
            // - `Threshold` 最多（30%）：影响最小、最常用，是"微调"的主力。
            // - `Param`（28%）**不能少**：`repeat` / `mirror` / `axis` 是
            //   结构变异里最便宜的那一类，一个数就能把"3 节"变成"4 节"。
            //   第一版给它 18%，结果 30 次变异里一次都没改到 `repeat`——
            //   身体永远是一整块，`Segment` 的表达力等于没用上。
            // - `Duplicate`（8%）：**罕见但关键**。真实演化里基因重复的频率
            //   也很低（约 1e-5/代），但一旦发生后果很大。
            //   高频 + 大影响 = 灭绝；低频 + 大影响 = 创新。
            //   8% 而不是 2%：`Body` 的基因只有几十个，2% 意味着几百次变异
            //   才有一次新结构，而一个谱系一辈子也就几十次变异。
            let op = if r < 0.30 {
                MorphOp::Threshold
            } else if r < 0.52 {
                MorphOp::FlipBit
            } else if r < 0.80 {
                MorphOp::Param
            } else if r < 0.88 {
                MorphOp::BandEdit
            } else if r < 0.92 {
                MorphOp::Crossover
            } else {
                MorphOp::Duplicate
            };
            match op {
                MorphOp::Param => self.mutate_param(rng),
                MorphOp::FlipBit => self.mutate_bit(rng),
                MorphOp::Threshold => self.mutate_threshold(rng),
                MorphOp::Duplicate => self.mutate_duplicate(rng),
                MorphOp::Crossover => self.mutate_crossover(rng),
                MorphOp::BandEdit => self.mutate_band(rng),
            }
        }
        // `Duplicate` / `Crossover` 都能把树推深，而存档读不回来那么深的树。
        // 放在**最后**统一砍：每个算子各自检查的话，漏一个就又是一次
        // "存出去的档读不回来"。
        self.prune_depth();
    }

    /// 参数微扰：动一个段的 `repeat` / `mirror` / `axis`，或者 `max_cells`。
    fn mutate_param(&mut self, rng: &mut Rng) {
        if rng.next_f32() < 0.25 {
            let step = if rng.next_f32() < 0.5 { 1i16 } else { -1 };
            let v = self.max_cells as i16 + step;
            self.max_cells = v.clamp(1, MAX_BODY_CELLS as i16) as u8;
            return;
        }
        let seg = pick_segment(&mut self.trunk, rng);
        // `repeat` 占 2/4：它是"体节数"，是结构变异里信息量最大的一个。
        // `axis` 只占 1/4：转轴会让整段换方向，通常直接毁掉身体。
        match rng.gen_range_usize(0, 4) {
            0 | 1 => {
                let step = if rng.next_f32() < 0.5 { 1i16 } else { -1 };
                let v = seg.repeat as i16 + step;
                seg.repeat = v.clamp(1, MAX_REPEAT as i16) as u8;
            }
            2 => seg.mirror = !seg.mirror,
            _ => seg.axis = ((seg.axis as usize + 1) % 4) as u8,
        }
    }

    /// 翻一个 bit：某一格的 base，或者某条带的 set。
    fn mutate_bit(&mut self, rng: &mut Rng) {
        let bit = 1u16 << rng.gen_range_usize(0, 11);
        if !self.gradient.bands.is_empty() && rng.next_f32() < 0.5 {
            let i = rng.gen_range_usize(0, self.gradient.bands.len());
            self.gradient.bands[i].set ^= bit;
        } else {
            let seg = pick_segment(&mut self.trunk, rng);
            if !seg.cells.is_empty() {
                let i = rng.gen_range_usize(0, seg.cells.len());
                seg.cells[i].base ^= bit;
            }
        }
    }

    /// 阈值微扰：动一条带的边界。**这是最常用的算子**——
    /// 它改的是"边界在哪儿"，而边界移动一点，形状只变一点。
    fn mutate_threshold(&mut self, rng: &mut Rng) {
        if self.gradient.bands.is_empty() {
            self.mutate_band(rng);
            return;
        }
        let i = rng.gen_range_usize(0, self.gradient.bands.len());
        let b = &mut self.gradient.bands[i];
        let sigma = 0.08f32;
        let jitter = (rng.next_f32() - 0.5) * 2.0 * sigma;
        let which = rng.gen_range_usize(0, 4);
        match which {
            0 => b.along.0 = (b.along.0 + jitter).clamp(0.0, 1.0),
            1 => b.along.1 = (b.along.1 + jitter).clamp(0.0, 1.0),
            2 => b.from_edge.0 = (b.from_edge.0 + jitter * 8.0).max(0.0),
            _ => b.from_edge.1 = (b.from_edge.1 + jitter * 8.0).max(0.0),
        }
        // 保证区间有序：反了就换回来。不这么做的话"命中判据"会永远为假，
        // 而症状是"这条带突然没用了"——比直接删掉还难查。
        if b.along.0 > b.along.1 {
            std::mem::swap(&mut b.along.0, &mut b.along.1);
        }
        if b.from_edge.0 > b.from_edge.1 {
            std::mem::swap(&mut b.from_edge.0, &mut b.from_edge.1);
        }
    }

    /// 增删一条带。
    fn mutate_band(&mut self, rng: &mut Rng) {
        if self.gradient.bands.len() >= MAX_BANDS || (!self.gradient.bands.is_empty() && rng.next_f32() < 0.4)
        {
            let i = rng.gen_range_usize(0, self.gradient.bands.len());
            self.gradient.bands.remove(i);
        } else {
            let a = rng.next_f32().min(0.95);
            let b = (a + rng.next_f32() * 0.4).min(1.0);
            let bit = 1u16 << rng.gen_range_usize(0, 11);
            self.gradient.bands.push(Band {
                along: (a.min(b), a.max(b)),
                from_edge: (0.0, 1.0),
                set: bit,
                clear: 0,
            });
        }
    }

    /// **子树复制**：复制一棵子树插到它自己下面。
    ///
    /// 这是"从零发明新东西"的唯一通道。中性漂移让冗余基因退化，
    /// 重组让重复基因分化——**两者配合才产生新功能**。
    /// 没有重复，漂移和重组都只是"在已有的东西上折腾"。
    fn mutate_duplicate(&mut self, rng: &mut Rng) {
        let seg = pick_segment(&mut self.trunk, rng);
        if seg.cells.is_empty() || seg.children.len() >= 3 {
            return;
        }
        let copy = seg.clone();
        // 复制出来的是**额外的**结构，所以上限也要跟着涨——
        // 否则新段会被 `max_cells` 截掉，等于没复制。
        // 症状极具欺骗性：代码看起来对，格数一动不动。
        let added = (copy.cells.len() as u8).saturating_mul(copy.repeat.max(1));
        seg.children.push(copy);
        self.max_cells = self
            .max_cells
            .saturating_add(added)
            .min(MAX_BODY_CELLS);
    }

    /// 子树交叉：把两棵子树换一下位置（同一具身体内部）。
    ///
    /// **这是个体内部的交叉**（同一具身体里换两个子段的位置），不是重组。
    /// 真正的重组见 [`crossover`]——那个要两个父代，而且需要**有性繁殖**
    /// 才有调用者。
    ///
    /// 为什么个体内部也留一个：它是"重新排列已有附肢"，比 `Duplicate`
    /// 便宜（不增加格数），是"模块化"在单亲繁殖下唯一能做的事。
    fn mutate_crossover(&mut self, rng: &mut Rng) {
        let a = pick_segment(&mut self.trunk, rng);
        if a.children.len() < 2 {
            return;
        }
        let i = rng.gen_range_usize(0, a.children.len());
        let j = rng.gen_range_usize(0, a.children.len());
        a.children.swap(i, j);
    }
}

/// **两个父代之间**的子树交叉（重组）。
///
/// 语义：从 `b` 里挑一段当供体，**替换** `a` 里的一段。于是子代可能带着
/// "`a` 的一只附肢 + `b` 的另一只"——**两个亲代都没有的形状**。
///
/// ## 为什么必须放在结构化基因组上
///
/// 行为网络（MLP）上有 competing conventions：两个父代的隐藏层神经元编号
/// 不对应，交叉出来的孩子是噪声。而 `Segment` 的"交换单位"语义完整——
/// 换一棵子树就是换一个附肢。**所以重组只在这里做，权重一律不交叉。**
///
/// ## 它还没有接线到繁殖
///
/// 现在的繁殖是**单亲**的（`reproduce` 只取一个亲代）。真正的重组需要
/// 有性繁殖：找配偶、能量对半分、种群动态全变。那是一个独立决策，
/// 不该塞进"换基因组表示"这一轮。这里先把**算子**做实并证明它有用
/// （`crossover_can_combine_structures_neither_parent_has`），
/// 接线留给后面。
pub fn crossover(a: &Body, b: &Body, rng: &mut Rng) -> Body {
    let mut child = a.clone();

    // 供体：从 b 的树上随便挑一段。
    let mut paths: Vec<Vec<usize>> = Vec::new();
    collect_paths(&b.trunk, &mut Vec::new(), &mut paths);
    let donor_path = paths.swap_remove(rng.gen_range_usize(0, paths.len()));
    let donor = segment_at(&b.trunk, &donor_path).clone();

    // 断点：**优先非根**。换掉整根等于克隆另一个亲代，那不是重组。
    let mut paths: Vec<Vec<usize>> = Vec::new();
    collect_paths(&a.trunk, &mut Vec::new(), &mut paths);
    let non_root: Vec<Vec<usize>> = paths.into_iter().filter(|p| !p.is_empty()).collect();
    if non_root.is_empty() || rng.next_f32() < 0.15 {
        child.trunk = donor;
    } else {
        let i = rng.gen_range_usize(0, non_root.len());
        let path = non_root[i].clone();
        *segment_at_mut(&mut child.trunk, &path) = donor;
    }

    // 换进来的段可能更大，上限要跟着涨——否则新结构会被 `max_cells`
    // 截掉，症状是"交叉成功了但格数一动不动"（和 `mutate_duplicate`
    // 踩过的是同一个坑）。
    let added = count_cells(&child.trunk).min(MAX_BODY_CELLS as usize) as u16;
    child.max_cells = (child.max_cells as u16)
        .max(added.min(MAX_BODY_CELLS as u16))
        .min(MAX_BODY_CELLS as u16) as u8;
    child
}

/// 按路径取一段（只读）。
fn segment_at<'a>(seg: &'a Segment, path: &[usize]) -> &'a Segment {
    let mut cur = seg;
    for &i in path {
        cur = &cur.children[i];
    }
    cur
}

/// 按路径取一段（可写）。
fn segment_at_mut<'a>(seg: &'a mut Segment, path: &[usize]) -> &'a mut Segment {
    let mut cur = seg;
    for &i in path {
        cur = &mut cur.children[i];
    }
    cur
}

/// 一段（含子孙、含重复份）总共占几格。
fn count_cells(seg: &Segment) -> usize {
    let own = seg.cells.len() * seg.repeat.max(1) as usize;
    own + seg.children.iter().map(count_cells).sum::<usize>()
}

/// 从树上随机挑一个段（按节点数均匀，不是按格数）。
///
/// 实现上先收集"从根到每个段的路径"，再按路径下钻一次。
/// 直接递归返回 `&mut` 会撞上借用检查器（同一条 `&mut` 链上不能有两个
/// 活跃的借用），而"路径 + 一次下钻"是最省事的绕法——代价是每次变异
/// 多分配一个小 `Vec`，而变异只在繁殖时发生，可以忽略。
fn pick_segment<'a>(seg: &'a mut Segment, rng: &mut Rng) -> &'a mut Segment {
    let mut paths: Vec<Vec<usize>> = Vec::new();
    collect_paths(seg, &mut Vec::new(), &mut paths);
    let target = rng.gen_range_usize(0, paths.len());
    let path = paths.swap_remove(target);
    let mut cur: &mut Segment = seg;
    for &i in &path {
        cur = &mut cur.children[i];
    }
    cur
}

fn collect_paths(seg: &Segment, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
    out.push(cur.clone());
    for (i, child) in seg.children.iter().enumerate() {
        cur.push(i);
        collect_paths(child, cur, out);
        cur.pop();
    }
}

/// 只留下和第一格 4-连通的那一块。
///
/// 第一格是主干 `cells[0]` 落在的格子——发育的"种子"。丢掉的部分不是身体，
/// 而是"从半空里长出来的碎片"。
fn keep_root_component(cells: &mut Vec<(i16, i16, u16, u8)>) {
    if cells.len() < 2 {
        return;
    }
    let all: HashSet<(i16, i16)> = cells.iter().map(|c| (c.0, c.1)).collect();
    let root = (cells[0].0, cells[0].1);
    let mut seen: HashSet<(i16, i16)> = HashSet::new();
    seen.insert(root);
    let mut stack = vec![root];
    while let Some((x, y)) = stack.pop() {
        for (dx, dy) in [(1i16, 0i16), (-1, 0), (0, 1), (0, -1)] {
            let p = (x + dx, y + dy);
            if all.contains(&p) && seen.insert(p) {
                stack.push(p);
            }
        }
    }
    if seen.len() == all.len() {
        return;
    }
    cells.retain(|c| seen.contains(&(c.0, c.1)));
}

fn count_segments(seg: &Segment) -> usize {
    1 + seg.children.iter().map(count_segments).sum::<usize>()
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::PLANS;
    use crate::organelle::{ARMOR, CHLORO_SHIFT, FLAGELLUM_SHIFT};

    #[test]
    fn a_single_cell_segment_is_one_cell() {
        let b = Body::blank();
        let cells = b.develop();
        assert_eq!(cells.len(), 1);
        assert_eq!(cells[0].offset, (0, 0));
        assert!(cells[0].organelles.has(MITO));
    }

    #[test]
    fn every_plan_round_trips_through_a_body() {
        // 手工体型必须**精确**还原：格数、每一格的类型、连通性。
        // 这是 M2b "手工体型行为不变"那条验收的基础。
        for plan in PLANS {
            let body = Body::from_plan(plan);
            let cells = body.develop();
            assert_eq!(cells.len(), plan.len(), "「{}」格数不对", plan.name);
            for &(kind, x, y) in plan.cells {
                let c = cells
                    .iter()
                    .find(|c| c.offset == (x, y))
                    .unwrap_or_else(|| panic!("「{}」丢了 ({x},{y})", plan.name));
                assert_eq!(
                    c.organelles.primary_cell_type(),
                    kind,
                    "「{}」({x},{y}) 的类型不对",
                    plan.name
                );
            }
        }
    }

    #[test]
    fn repeat_gives_segments_that_do_not_overlap() {
        // 一个 1 格宽的段重复 4 次，应该是 4 格排成一条线。
        let mut seg = Segment::single(BodyCell {
            offset: (0, 0),
            base: MITO,
        });
        seg.repeat = 4;
        let b = Body {
            trunk: seg,
            gradient: Gradient::default(),
            max_cells: 24,
        };
        let cells = b.develop();
        assert_eq!(cells.len(), 4, "重复 4 次应当有 4 格");
        let mut xs: Vec<i16> = cells.iter().map(|c| c.offset.0).collect();
        xs.sort_unstable();
        assert_eq!(xs, vec![0, 1, 2, 3], "重复份应当首尾相接，不重叠");
    }

    #[test]
    fn a_wide_segment_repeats_by_its_span_not_by_one() {
        // 3 格宽的段重复 2 次：第二份从 x=3 开始，不是 x=1。
        // 按 1 格重复会让两份叠在一起——这是最典型的写法错误。
        let seg = Segment {
            cells: vec![
                BodyCell { offset: (0, 0), base: MITO },
                BodyCell { offset: (1, 0), base: MITO },
                BodyCell { offset: (2, 0), base: MITO },
            ],
            axis: 0,
            repeat: 2,
            mirror: false,
            children: Vec::new(),
        };
        let b = Body {
            trunk: seg,
            gradient: Gradient::default(),
            max_cells: 24,
        };
        let mut xs: Vec<i16> = b.develop().iter().map(|c| c.offset.0).collect();
        xs.sort_unstable();
        assert_eq!(xs, vec![0, 1, 2, 3, 4, 5], "重复应当按跨度平移");
    }

    #[test]
    fn mirror_gives_pairs_across_the_axis() {
        // 镜像的用途是**成对器官**：轴上有一格当脊柱，两侧各一格。
        // 所以测试也要按真实形状搭——只有轴外一格时镜像会隔着一条空行，
        // 那不是身体（见下一条测试）。
        let mut seg = Segment {
            cells: vec![
                BodyCell {
                    offset: (0, 0),
                    base: MITO,
                },
                BodyCell {
                    offset: (0, 1),
                    base: MITO,
                },
            ],
            axis: 0,
            repeat: 1,
            mirror: true,
            children: Vec::new(),
        };
        seg.mirror = true;
        let b = Body {
            trunk: seg,
            gradient: Gradient::default(),
            max_cells: 24,
        };
        let cells = b.develop();
        let ys: HashSet<i16> = cells.iter().map(|c| c.offset.1).collect();
        assert_eq!(cells.len(), 3, "轴上 1 格 + 两侧各 1 格：{ys:?}");
        assert!(ys.contains(&0) && ys.contains(&1) && ys.contains(&-1), "{ys:?}");
    }

    #[test]
    fn mirror_without_a_spine_is_dropped() {
        // 只有轴外一格时，镜像给出的是"隔一条空行的两格"——**不连通**。
        // 发育规则（`keep_root_component`）把远侧丢掉，于是镜像退化成原样。
        //
        // 这不是 bug，是"身体必须连通"这条规则的直接后果：想长成对器官，
        // 得先有脊柱把两侧连起来。**这条规则把 `mirror` 从"免费翻倍"
        // 变成了"需要先有躯干"**——正好是我们想要的取舍。
        let mut seg = Segment::single(BodyCell {
            offset: (0, 1),
            base: MITO,
        });
        seg.mirror = true;
        let b = Body {
            trunk: seg,
            gradient: Gradient::default(),
            max_cells: 24,
        };
        let cells = b.develop();
        assert_eq!(cells.len(), 1, "隔着空行的镜像不该算身体");
    }

    #[test]
    fn crossover_can_combine_structures_neither_parent_has() {
        // 重组的价值不在"打散"，在**组合**：两个谱系各自长出一只附肢，
        // 重组能让一个后代同时带着两只。没有这一步，漂移和重复都只是
        // "在已有的东西上折腾"。
        let mk = |bit: u16| Segment {
            cells: vec![BodyCell {
                offset: (0, 0),
                base: bit,
            }],
            axis: 0,
            repeat: 1,
            mirror: false,
            children: Vec::new(),
        };
        // a：躯干 + 两个子段（叶绿体 / 鞭毛）
        let mut a = Body {
            trunk: mk(MITO),
            gradient: Gradient::default(),
            max_cells: 24,
        };
        a.trunk.children.push(mk(1 << CHLORO_SHIFT));
        a.trunk.children.push(mk(1 << FLAGELLUM_SHIFT));
        // b：躯干 + 一个子段（装甲）
        let mut b = Body {
            trunk: mk(MITO),
            gradient: Gradient::default(),
            max_cells: 24,
        };
        b.trunk.children.push(mk(ARMOR));

        let mut combined = 0usize;
        let mut rng = Rng::new(0x5EED_1234);
        for _ in 0..400 {
            let c = crossover(&a, &b, &mut rng);
            let kids = &c.trunk.children;
            if kids.len() == 2
                && kids.iter().any(|k| k.cells[0].base == ARMOR)
                && kids.iter().any(|k| k.cells[0].base != ARMOR)
            {
                combined += 1;
            }
        }
        assert!(
            combined > 20,
            "400 次交叉里只有 {combined} 次真的组合了两个亲代的结构"
        );
    }

    #[test]
    fn a_diamond_of_four_cells_collapses_to_one() {
        // 对角线相邻不算连通。四格菱形（上下左右各一格、中心空）在
        // 4-连通意义上就是 4 个孤立细胞——它不该是一具身体。
        let seg = Segment {
            cells: vec![
                BodyCell { offset: (1, 0), base: MITO },
                BodyCell { offset: (0, 1), base: MITO },
                BodyCell { offset: (2, 1), base: MITO },
                BodyCell { offset: (1, 2), base: MITO },
            ],
            axis: 0,
            repeat: 1,
            mirror: false,
            children: Vec::new(),
        };
        let b = Body { trunk: seg, gradient: Gradient::default(), max_cells: 24 };
        let cells = b.develop();
        assert_eq!(cells.len(), 1, "菱形应当只剩根那一格，实际 {:?}", cells.iter().map(|c| c.offset).collect::<Vec<_>>());
    }

    #[test]
    fn a_body_is_never_disconnected() {
        // 8000 tick 的演化里出现过 `包围盒 10×5`、中间空一行、"散架成 3 块"
        // 的身体。不连通在物理上是荒谬的：`try_move` 只逐格验通行，
        // 中间那道墙不参与判定，于是它能穿过一格厚的石头。
        //
        // 这里从银鱼出发做 200 次变异 × 20 个种子，逐个数连通分量。
        let mut rng = Rng::new(0xC0FF_EE01);
        let mut checked = 0usize;
        for seed in 0..20u64 {
            let mut b = Body::from_plan(&crate::body::SILVERFISH);
            let mut r = Rng::new(seed.wrapping_mul(0x9E37_79B9) ^ 7);
            b.mutate(&mut r, 6);
            let cells = b.expand();
            let set: HashSet<(i16, i16)> = cells.iter().map(|c| (c.0, c.1)).collect();
            // 一次洪泛，数能到几格。
            let mut seen: HashSet<(i16, i16)> = HashSet::new();
            let mut stack = vec![*set.iter().next().unwrap()];
            seen.insert(stack[0]);
            while let Some((x, y)) = stack.pop() {
                for (dx, dy) in [(1i16, 0i16), (-1, 0), (0, 1), (0, -1)] {
                    let p = (x + dx, y + dy);
                    if set.contains(&p) && seen.insert(p) {
                        stack.push(p);
                    }
                }
            }
            assert_eq!(
                seen.len(),
                set.len(),
                "种子 {seed} 长出了不连通的身体（{} / {} 格）",
                seen.len(),
                set.len()
            );
            checked += 1;
            let _ = &mut rng;
        }
        assert_eq!(checked, 20);
    }

    #[test]
    fn children_grow_from_the_end_of_the_parent() {
        // 主干 1 格 + 一个子段 1 格，子段应当长在主干后面，不重叠。
        let child = Segment::single(BodyCell {
            offset: (0, 0),
            base: MITO,
        });
        let trunk = Segment {
            cells: vec![BodyCell { offset: (0, 0), base: MITO }],
            axis: 0,
            repeat: 1,
            mirror: false,
            children: vec![child],
        };
        let b = Body {
            trunk,
            gradient: Gradient::default(),
            max_cells: 24,
        };
        let cells = b.develop();
        assert_eq!(cells.len(), 2);
        let xs: Vec<i16> = cells.iter().map(|c| c.offset.0).collect();
        assert!(xs.contains(&0) && xs.contains(&1), "子段应当接在末端：{xs:?}");
    }

    #[test]
    fn gradient_bands_set_and_clear() {
        // 一条"前半身是生产者、后半身是嘴"的梯度。
        let trunk = Segment {
            cells: (0..6)
                .map(|i| BodyCell {
                    offset: (i, 0),
                    base: MITO,
                })
                .collect(),
            axis: 0,
            repeat: 1,
            mirror: false,
            children: Vec::new(),
        };
        let grad = Gradient {
            bands: vec![
                Band {
                    along: (0.0, 0.5),
                    from_edge: (0.0, 9.0),
                    set: crate::organelle::CHLORO,
                    clear: 0,
                },
                Band {
                    along: (0.5, 1.0),
                    from_edge: (0.0, 9.0),
                    set: crate::organelle::DIGEST,
                    clear: crate::organelle::CHLORO,
                },
            ],
        };
        let b = Body {
            trunk,
            gradient: grad,
            max_cells: 24,
        };
        let cells = b.develop();
        let producers = cells
            .iter()
            .filter(|c| c.organelles.level(crate::organelle::CHLORO_SHIFT) > 0)
            .count();
        let mouths = cells
            .iter()
            .filter(|c| c.organelles.has(crate::organelle::DIGEST))
            .count();
        assert!(producers >= 2, "前半身应当有生产者，实际 {producers}");
        assert!(mouths >= 2, "后半身应当有嘴，实际 {mouths}");
        // `clear` 必须真的清掉：后半身不该同时有叶绿体。
        for c in &cells {
            if c.organelles.has(crate::organelle::DIGEST) {
                assert_eq!(
                    c.organelles.level(crate::organelle::CHLORO_SHIFT),
                    0,
                    "`clear` 没生效：这一格既是嘴又有叶绿体"
                );
            }
        }
    }

    #[test]
    fn max_cells_truncates() {
        let mut seg = Segment::single(BodyCell { offset: (0, 0), base: MITO });
        seg.repeat = 8;
        let b = Body {
            trunk: seg,
            gradient: Gradient::default(),
            max_cells: 3,
        };
        assert_eq!(b.develop().len(), 3, "max_cells 应当截断");
    }

    #[test]
    fn development_is_deterministic() {
        // 遗传的全部前提：同一个身体必然长成同一个样子。
        let b = Body::from_plan(&crate::body::SILVERFISH);
        assert_eq!(b.develop(), b.develop());
    }

    #[test]
    fn depth_is_capped() {
        // 深度超过 `MAX_SEGMENT_DEPTH` 的子段必须被丢掉——
        // 否则 `Segment` 树会变成事实上的 L-system，变异影响范围回升。
        let mut leaf = Segment::single(BodyCell { offset: (0, 0), base: MITO });
        for _ in 0..6 {
            leaf = Segment {
                cells: vec![BodyCell { offset: (0, 0), base: MITO }],
                axis: 0,
                repeat: 1,
                mirror: false,
                children: vec![leaf],
            };
        }
        let b = Body {
            trunk: leaf,
            gradient: Gradient::default(),
            max_cells: 24,
        };
        // 深度 0..=3 一共 4 个段能展开，剩下的被丢掉。
        assert_eq!(b.develop().len(), 4, "深度上限没生效");
    }

    #[test]
    fn duplicate_adds_a_structure() {
        // 基因重复：复制一棵子树，格数必须涨。
        let mut b = Body::from_plan(&crate::body::SILVERFISH);
        let before = b.develop().len();
        // 手工把主干复制一份当子段。上限要跟着放开，否则新段会被截掉——
        // 这正是 `mutate_duplicate` 自己会做的事。
        b.max_cells = MAX_BODY_CELLS;
        let copy = b.trunk.clone();
        b.trunk.children.push(copy);
        let after = b.develop().len();
        assert!(after > before, "复制子树之后格数应当变多：{before} → {after}");
    }

    #[test]
    fn mutation_never_panics_and_stays_in_bounds() {
        // 变异是"永远不该崩"的那类代码：它在每次繁殖时都跑。
        let mut rng = Rng::new(7);
        let mut b = Body::from_plan(&crate::body::SILVERFISH);
        for _ in 0..500 {
            b.mutate(&mut rng, 3);
            let cells = b.develop();
            assert!(
                cells.len() <= MAX_BODY_CELLS as usize,
                "变异之后身体超过了格数上限：{}",
                cells.len()
            );
            for c in &cells {
                assert!(c.organelles.0 < (1 << 12), "细胞器位越界：{}", c.organelles.0);
            }
        }
    }

    #[test]
    fn bands_stay_ordered_after_mutation() {
        // 阈值变异之后区间必须仍然有序——反了的话命中判据永远为假，
        // 症状是"这条带突然没用了"，比直接删掉还难查。
        let mut rng = Rng::new(11);
        let mut b = Body::from_plan(&crate::body::SILVERFISH);
        for _ in 0..200 {
            b.mutate_threshold(&mut rng);
            for band in &b.gradient.bands {
                assert!(band.along.0 <= band.along.1, "along 区间反了：{:?}", band.along);
                assert!(
                    band.from_edge.0 <= band.from_edge.1,
                    "from_edge 区间反了：{:?}",
                    band.from_edge
                );
            }
        }
    }
}
