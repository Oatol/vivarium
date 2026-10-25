//! 土壤湿度：一张**二维**的场。
//!
//! ## 它回答的问题
//!
//! "这一格有多湿"——根从这里抽水，叶绿体按它打折。
//!
//! ## 判据：到最近**水格**的距离
//!
//! 整张场只有一条规则：
//!
//! ```text
//! moisture(p) = 255 × (1 − dist(p, 最近的水) / SPAN)
//! ```
//!
//! 于是"水满值、往陆地深处衰减"是这条规则的直接结果，不需要额外写
//! "海岸带"、"内陆"、"高地"这些概念。
//!
//! ## 为什么是二维的（v0.42 换的）
//!
//! v0.41 之前它是**每列一个数**：同一列从地表到地底一样湿。那样最省，
//! 但它有一个说不通的地方——**高处和低处一样湿**。一株长在离海 3 格、
//! 但比海面高 40 格的悬崖上的植物，按列算是"满水"，而它实际上够不到
//! 任何水。
//!
//! 换成二维之后，"湿"变成了几何：等值线是绕着水体的等距线。悬崖底下湿、
//! 顶上干；内陆湖会有一圈湿地；海底以下全是满值。**这些都不是新规则，
//! 是同一条规则的二维结果。**
//!
//! 顺带：在**平地**上它退化成 v0.41 那个一维场（平地到水的距离就是横向
//! 距离），所以 v0.41 那张"能走多远"的表仍然成立。
//!
//! ## 为什么是粗格（4×4）
//!
//! 两个理由，都来自"它要跟着世界跑"：
//!
//! 1. **每 tick 重算一次**。逐格算 512² 要几毫秒，50 tick/帧就是几百毫秒；
//!    粗格是它的十六分之一，几十微秒。
//! 2. **它就是渲染器要的那张图**。场纹理（光、温度）本来就是 4×4 的，
//!    湿度用同一套格子之后可以直接 1:1 灌进去，连平均都不用算。
//!
//! 代价是湿度按 4 格台阶走（梯度约 5 点/台阶）。而根的回充档位是 40 点
//! 一档，所以这个量化在玩法上看不出来。

use crate::{MaterialKind, World};

/// 湿度衰减到 0 的距离（**格**）。
///
/// 200：512 宽的世界里陆地约 180 列，所以"从海岸往里走 200 列"已经
/// 走到世界另一边——再往里就是沙漠。
///
/// **这个数决定了"植物能走多远"**，而它是这一整套规则里唯一一个
/// 关于"陆地有多宽"的数。改它等于改地图上能长植物的比例。
pub const SPAN: f32 = 200.0;

/// 湿度格的边长（格）。必须与 `field::AIR_CELL` 一致——
/// 湿度要直接灌进场纹理，两者的格子对不齐就会错位。
pub const CELL: usize = crate::field::AIR_CELL;

/// 一格有多湿（`0..=255`）。
///
/// 和 `Organism::hydration` 同一个量纲：`hydration` 是"身上带的水"，
/// 这是"脚下有多少水"。光合要两个都有。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Moisture {
    /// 粗格宽（列数）。
    cols: usize,
    /// 粗格高（行数）。
    rows: usize,
    /// 逐粗格的湿度，行优先，长度 `cols * rows`。
    cells: Vec<u8>,
    /// 海平面（最上面那一格水的 y）。没有水时是 0。
    ///
    /// 单独记下来是因为它是"深度"这个概念的零点：界面显示"深多少格"、
    /// 玩家判断"这块地多高"都靠它。**从湿度场本身反推不出来**——
    /// 满值的地方既可能是海面，也可能是海底。
    sea_level: i32,
    /// 每次 `rebuild` 自增。渲染器靠它决定要不要重传。
    revision: u64,
}

impl Moisture {
    /// 空场：0×0，所有查询都返回 0。
    pub fn new() -> Self {
        Self::default()
    }

    /// 按世界重算整张场。
    ///
    /// 代价是两遍 `O(cols·rows)` 扫描——512² 的世界是 128×128 个粗格，
    /// 几十微秒。所以它**可以每 tick 调一次**，而"每 tick 重算"买到的
    /// 是：湿度永远是当前世界的函数，不存在"水流动了但场还停在旧值"。
    pub fn rebuild(&mut self, world: &World) {
        let (w, h) = (world.width(), world.height());
        self.revision = self.revision.wrapping_add(1);
        if w == 0 || h == 0 {
            self.cols = 0;
            self.rows = 0;
            self.cells.clear();
            self.sea_level = 0;
            return;
        }
        let cols = w.div_ceil(CELL);
        let rows = h.div_ceil(CELL);
        self.cols = cols;
        self.rows = rows;

        // --- 1. 种子：含水的粗格距离 0，顺带找海平面 ---
        //
        // `INF` 取一个加多少次都回不到 0 的值。
        const INF: u32 = u32::MAX / 8;
        let mut dist = vec![INF; cols * rows];
        let mut top_water = vec![usize::MAX; w];
        for y in 0..h {
            let cy = y / CELL;
            for x in 0..w {
                if world.get(x as isize, y as isize) == MaterialKind::Water {
                    dist[cy * cols + x / CELL] = 0;
                    if top_water[x] == usize::MAX {
                        top_water[x] = y;
                    }
                }
            }
        }

        // 海平面 = 各列"最上面那格水"的**众数**。
        //
        // 不能用最小值：玩家在内陆高处挖一个小水洼，最小值就成了那个水洼，
        // 于是整个世界的高度基准被一格水抬走。而海是最大的一片水面，
        // 众数一定是它。
        self.sea_level = {
            let mut hist = vec![0u32; h];
            for &t in &top_water {
                if t != usize::MAX {
                    hist[t] += 1;
                }
            }
            hist.iter()
                .enumerate()
                .max_by_key(|&(y, &n)| (n, std::cmp::Reverse(y)))
                .map(|(y, _)| y)
                .unwrap_or(0) as i32
        };

        // --- 2. 两遍 chamfer 距离变换（单位：粗格） ---
        //
        // 权重 3（正交）/ 4（斜向）是经典近似：真实斜向距离是 √2 ≈ 1.414，
        // 而 4/3 ≈ 1.333，误差 6%。对一个"决定根抽水快慢"的数完全够用，
        // 而它只用整数加法。
        //
        // 横向**环绕**（`rem_euclid`）：世界是环面，地图左边缘的陆地
        // 右边就是海。不环绕的话那一列会算成"离水最远"，画面上会出现
        // 一条假的干旱带贴在世界边上。
        fn get(dist: &[u32], cols: usize, x: usize, y: usize) -> u32 {
            dist[y * cols + x]
        }
        let left = |x: usize| (x + cols - 1) % cols;
        let right = |x: usize| (x + 1) % cols;
        for y in 0..rows {
            for x in 0..cols {
                let mut d = get(&dist, cols, x, y);
                if y > 0 {
                    d = d.min(get(&dist, cols, x, y - 1).saturating_add(3));
                    d = d.min(get(&dist, cols, left(x), y - 1).saturating_add(4));
                    d = d.min(get(&dist, cols, right(x), y - 1).saturating_add(4));
                }
                d = d.min(get(&dist, cols, left(x), y).saturating_add(3));
                dist[y * cols + x] = d;
            }
        }
        for y in (0..rows).rev() {
            for x in (0..cols).rev() {
                let mut d = get(&dist, cols, x, y);
                if y + 1 < rows {
                    d = d.min(get(&dist, cols, x, y + 1).saturating_add(3));
                    d = d.min(get(&dist, cols, left(x), y + 1).saturating_add(4));
                    d = d.min(get(&dist, cols, right(x), y + 1).saturating_add(4));
                }
                d = d.min(get(&dist, cols, right(x), y).saturating_add(3));
                dist[y * cols + x] = d;
            }
        }

        // --- 3. 距离 → 湿度 ---
        //
        // `d3` 是 3 倍的**粗格**距离，所以换成格要再乘 `CELL`。
        let span3 = SPAN / CELL as f32 * 3.0;
        self.cells = dist
            .iter()
            .map(|&d3| {
                if d3 >= INF {
                    // 一滴水都没有的世界：全干。这是合法的（空世界就是这样）。
                    return 0;
                }
                let t = 1.0 - (d3 as f32 / span3).min(1.0);
                (255.0 * t).round().clamp(0.0, 255.0) as u8
            })
            .collect();
    }

    /// 这一格有多湿。越界时返回 0，横向环绕。
    pub fn at(&self, x: i32, y: i32) -> u8 {
        if self.cols == 0 || self.rows == 0 || y < 0 || y as usize / CELL >= self.rows {
            return 0;
        }
        let cx = x.rem_euclid((self.cols * CELL) as i32) as usize / CELL;
        self.cells[(y as usize / CELL) * self.cols + cx]
    }

    /// 逐粗格湿度（行优先，长度 `cols * rows`）。给渲染器和存档用。
    pub fn cells(&self) -> &[u8] {
        &self.cells
    }

    /// 尺寸 `(列数, 行数)`（**粗格**）。
    pub fn dims(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// 海平面（最上面那一格水的 y）。
    pub fn sea_level(&self) -> i32 {
        self.sea_level
    }

    /// 版本号：每次 `rebuild` 自增。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 是不是一张空场（还没按世界算过）。
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}
