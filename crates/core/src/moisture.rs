//! 土壤湿度：一张**二维**的场。
//!
//! ## 它回答的问题
//!
//! "这一格有多湿"——根从这里抽水，叶绿体按它打折。
//!
//! ## 两个阶段：**播种**与**演化**
//!
//! 这张场原来是"世界布局的纯函数"：`255 × (1 − 到最近水格的距离 / SPAN)`，
//! 每 tick 从零重算一遍。它有一个说不通的地方——**水一流走，湿度立刻掉回去**。
//! 于是 v0.51 加了降水之后，"雨落在岸上"这件事在场上留不下任何痕迹：
//! 雨变成水格 → 场变湿 → 水格流回海里 → 场立刻变干。实测 1479 格雨只让
//! 陆地表面湿度抬了 18 点，而且一点都记不住。
//!
//! 现在它分两段（v0.52）：
//!
//! 1. **`reseed`**：还是那条距离公式，但只在**建场时**跑（开新世界、读旧档、
//!    玩家用画笔改了材质）。它给出初值，保证开局和以前一模一样。
//! 2. **`step`**：每 tick 的**增量**演化——吸水、扩散、蒸发。
//!
//! 于是湿度变成了一个**有记忆的量**：雨落下来，那一带湿上几百 tick，
//! 慢慢被蒸发和扩散带走。**"这场雨"第一次留得住。**
//!
//! ## 三条演化规则
//!
//! ```text
//! 吸水：含水的粗格 → 255（每 8 tick 重扫一次水格）
//! 扩散：和四邻交换，各向同性，系数 DIFFUSE/256
//! 蒸发：每格每 tick 有 EVAP_PERCENT% 的概率掉 1 点
//! ```
//!
//! 稳态解是**抛物线**：`m(x) = 255 − (E / 2D)·x²`。和原来的**线性**剖面
//! 形状不同（近岸更湿、边界处掉得更快），但**能用范围几乎一样**：
//! 根的回充门槛是 40 点，线性剖面在 168 格处到 40，抛物线在 160 格——
//! 差 5%。这两个数都由 `SPAN` 和 `E/D` 决定，而 `E/D` 是照着
//! "`SPAN` 处归零"反推的（见 [`DIFFUSE`] / [`EVAP_PERCENT`] 的说明）。
//!
//! ## 为什么是二维的（v0.42 换的）
//!
//! v0.41 之前它是**每列一个数**：同一列从地表到地底一样湿。那样最省，
//! 但它有一个说不通的地方——**高处和低处一样湿**。一株长在离海 3 格、
//! 但比海面高 40 格的悬崖上的植物，按列算是"满水"，而它实际上够不到
//! 任何水。
//!
//! 换成二维之后，"湿"变成了几何：等值线是绕着水体的等距线。悬崖底下湿、
//! 顶上干；内陆湖会有一圈湿地；海底以下全是满值。
//!
//! ## 为什么是粗格（4×4）
//!
//! 两个理由：
//!
//! 1. **它每 tick 都要跑一次**。逐格算 512² 要几毫秒，50 tick/帧就是几百毫秒；
//!    粗格是它的十六分之一（128×128 = 16384 格）。
//! 2. **它就是渲染器要的那张图**。场纹理（光、温度）本来就是 4×4 的，
//!    湿度用同一套格子之后可以直接 1:1 灌进去，连平均都不用算。
//!
//! 代价是湿度按 4 格台阶走（梯度约 5 点/台阶）。而根的回充档位是 40 点
//! 一档，所以这个量化在玩法上看不出来。

use crate::{MaterialKind, Rng, World};

/// 湿度衰减到 0 的距离（**格**）。
///
/// 200：512 宽的世界里陆地约 180 列，所以"从海岸往里走 200 列"已经
/// 走到世界另一边——再往里就是沙漠。
///
/// **这个数决定了"植物能走多远"**，而它是这一整套规则里唯一一个
/// 关于"陆地有多宽"的数。改它等于改地图上能长植物的比例。
///
/// v0.52 之后它**只影响 `reseed` 的初值**：演化段的衰减尺度由
/// [`DIFFUSE`] / [`EVAP_PERCENT`] 的比值决定，而那个比值是照着
/// "`SPAN` 处归零"反推的，所以改这里要同时改那两个数。
pub const SPAN: f32 = 200.0;

/// 湿度格的边长（格）。必须与 `field::AIR_CELL` 一致——湿度要直接灌进
/// 场纹理，两者的格子对不齐就会错位。
pub const CELL: usize = crate::field::AIR_CELL;

/// 扩散系数：每 tick 传给**每个邻居**的比例，分母 256。
///
/// `48/256 = 0.1875`，四邻合计 `0.75 < 1`——**必须小于 1**，否则显式扩散
/// 会震荡（相邻两格来回过冲，画面上是一片跳动的棋盘格）。
///
/// 和 [`EVAP_PERCENT`] 的比值决定衰减尺度：`E / (2D) = 0.05 / 0.375 = 0.133`，
/// 于是 `m(x) = 255 − 0.133·x²`（`x` 是**粗格**），归零点在 43.7 粗格
/// ≈ **175 格**，和 [`SPAN`] 的 200 对得上。
pub const DIFFUSE: i32 = 48;

/// 每格每 tick 蒸发掉 1 点的概率（%）。
///
/// 5% 意味着"离水无限远"的土 5100 tick 从 255 干到 0——比一秒长、
/// 比一分钟短，正好是"一场雨能用一阵子"的尺度。
///
/// 用**概率**而不是"每 N tick 掉 1"，是因为 `u8` 装不下小数：定值 1/tick
/// 太快（255 tick 就干透），而"每 20 tick 掉 1"会让整张场同步呼吸。
pub const EVAP_PERCENT: u32 = 5;

const SALT_EVAPORATE: u32 = 0x00D1_0001;

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
    /// 每次 `reseed` / `step` 自增。渲染器靠它决定要不要重传。
    revision: u64,
}

impl Moisture {
    /// 空场：0×0，所有查询都返回 0。
    pub fn new() -> Self {
        Self::default()
    }

    /// 从存档里的一份状态恢复。
    ///
    /// `cells` 的长度必须等于 `cols * rows`，否则返回 `None`——宁可让读档
    /// 走"重算一遍"的降级路径，也不要一个尺寸对不上的场（那会让
    /// `at()` 读到隔壁行的数据，而画面只是"湿度有点怪"）。
    pub fn from_parts(cols: usize, rows: usize, cells: Vec<u8>, sea_level: i32) -> Option<Self> {
        if cols == 0 || rows == 0 || cells.len() != cols * rows {
            return None;
        }
        Some(Self {
            cols,
            rows,
            cells,
            sea_level,
            revision: 0,
        })
    }

    // -----------------------------------------------------------------------
    // 阶段一：播种
    // -----------------------------------------------------------------------

    /// 按世界**重算整张场**（距离公式），并定死海平面。
    ///
    /// 只在三种时刻调：开新世界、读旧档（没有湿度状态的档）、玩家用画笔
    /// 改了材质。**它会把演化出来的记忆全部清掉**——这是"重画材质"该有的
    /// 语义（你刚挖了一个湖，那就按这个湖重新算一遍），但不要每 tick 调。
    pub fn reseed(&mut self, world: &World) {
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
        // 直接扫**连续的那张表**，不逐格调 `world.get()`。
        //
        // `get` 每次都要算下标 + 查边界，而这里要问 26 万次。实测这一改动
        // 让整次重算从 0.53 ms 降到 0.2 ms 上下。
        let cells = world.cells();
        for y in 0..h {
            let cy = y / CELL;
            let row = y * w;
            for x in 0..w {
                if cells[row + x].kind == MaterialKind::Water {
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
        //
        // **只在第一次建场时定死，之后不再更新。**
        //
        // 这条是 v0.45 修的：众数会随水面被搅动而漂（生物在表层出生、
        // 死亡、挤水，都会让"最上面那格水"在 127/128/129 之间来回），
        // 于是"海平面"这个基准自己会动。而它一旦往下漂一格，
        // `water_above` 就会把整个表层（~500 列）都算成"水面以上"——
        // 实测过 0 → 663 的假增长。**基准必须是常数，否则量出来的全是噪声。**
        if self.sea_level == 0 {
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
        }

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
        // 环绕的邻居下标**预先算好**，不在内层循环里取模。
        let left: Vec<usize> = (0..cols).map(|x| (x + cols - 1) % cols).collect();
        let right: Vec<usize> = (0..cols).map(|x| (x + 1) % cols).collect();
        for y in 0..rows {
            for x in 0..cols {
                let mut d = get(&dist, cols, x, y);
                if y > 0 {
                    d = d.min(get(&dist, cols, x, y - 1).saturating_add(3));
                    d = d.min(get(&dist, cols, left[x], y - 1).saturating_add(4));
                    d = d.min(get(&dist, cols, right[x], y - 1).saturating_add(4));
                }
                d = d.min(get(&dist, cols, left[x], y).saturating_add(3));
                dist[y * cols + x] = d;
            }
        }
        for y in (0..rows).rev() {
            for x in (0..cols).rev() {
                let mut d = get(&dist, cols, x, y);
                if y + 1 < rows {
                    d = d.min(get(&dist, cols, x, y + 1).saturating_add(3));
                    d = d.min(get(&dist, cols, left[x], y + 1).saturating_add(4));
                    d = d.min(get(&dist, cols, right[x], y + 1).saturating_add(4));
                }
                d = d.min(get(&dist, cols, right[x], y).saturating_add(3));
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

    // -----------------------------------------------------------------------
    // 阶段二：演化
    // -----------------------------------------------------------------------

    /// 推进一个 tick：吸水 → 扩散 → 蒸发。
    ///
    /// 尺寸对不上世界（刚换了世界、还没 `reseed`）时退化成一次 `reseed`——
    /// 那是唯一安全的选择：拿旧尺寸的场去算新世界会直接越界。
    pub fn step(&mut self, world: &World, tick: u64) {
        let (w, h) = (world.width(), world.height());
        if w == 0 || h == 0 {
            return;
        }
        if self.cols != w.div_ceil(CELL) || self.rows != h.div_ceil(CELL) {
            self.reseed(world);
            return;
        }

        // 1) 吸水：含水的粗格强制满值。
        //
        // **每 tick 都扫一遍世界**，而不是每 N tick 扫一次、把结果缓存成
        // 一张"哪些粗格含水"的表。两个理由：
        //
        // 1. **缓存会让读档不能逐位一致。** 那张表是"上一次扫描的结果"，
        //    是一份**带时间戳的状态**：原件在 tick 16 扫过、读档件在 tick 19
        //    才第一次扫，两次扫描之间水格已经变了，于是两边从同一份存档
        //    出发却跑出不同的湿度。实测就是这条把 `a_loaded_world_keeps_
        //    running_identically` 打红的。
        // 2. 缓存也没省多少：扫一遍 512² 是 26 万次连续读，实测
        //    **0.19 ms/tick**，占整 tick 的 5% 上下。为它引入一份要存档、
        //    要解释、还要小心对齐的隐藏状态，不划算。
        //
        // 顺带一个坑：一开始"重扫"和"钉值"是混在一起的（每 8 tick 才钉
        // 一次），结果**一个水格只能把它的粗格抬到平均 81 分**——钉上之后
        // 立刻被四个干邻居抽走 75%。于是"一格雨"在场上几乎留不下痕迹。
        // 现在钉是每 tick 都做的。
        {
            let cells = world.cells();
            for y in 0..h {
                let cy = y / CELL;
                let row = y * w;
                for x in 0..w {
                    if cells[row + x].kind == MaterialKind::Water {
                        self.cells[cy * self.cols + x / CELL] = 255;
                    }
                }
            }
        }

        // 2) 扩散 + 蒸发。
        //
        // ## 扩散必须**逐边**算，不能逐格算
        //
        // 第一版是逐格的：`flow = f(Σ(邻居 − 我))`，`f` 带四舍五入。
        // 它**不守恒**——`f` 是奇函数但不是线性的，而
        // `Σ f(acc_i) = 0` 只在 `f` 线性时成立。实测：1000 tick 里蒸发
        // 烧掉 12881 点，总量只掉了 2690 点，**扩散凭空造出一万点**。
        // 症状是"土怎么越来越湿"，而每一行代码看着都对。
        //
        // 现在每条边算一次流量，**同时加到两端**（一正一负），于是守恒
        // 是构造上的：不管怎么取整，两个端点的变化量永远互相抵消。
        // 顺带还便宜一半——每格只看右边和下边两条边。
        //
        // 读 `self.cells`、写 `delta`/`next`：**不能就地改**，否则结果依赖
        // 遍历顺序（右邻居已经被本 tick 更新过），画面上是一片朝右的条纹。
        let (cols, rows) = (self.cols, self.rows);
        let mut delta = vec![0i32; cols * rows];
        for y in 0..rows {
            let row = y * cols;
            for x in 0..cols {
                let i = row + x;
                // 右边（**环绕**：世界是环面）。只有一列时别自己连自己。
                let r = row + (x + 1) % cols;
                if r != i {
                    let f = (self.cells[r] as i32 - self.cells[i] as i32) * DIFFUSE / 256;
                    delta[i] += f;
                    delta[r] -= f;
                }
                // 下边（不环绕：世界有底）。
                if y + 1 < rows {
                    let j = i + cols;
                    let f = (self.cells[j] as i32 - self.cells[i] as i32) * DIFFUSE / 256;
                    delta[i] += f;
                    delta[j] -= f;
                }
            }
        }

        for y in 0..rows {
            let row = y * cols;
            for x in 0..cols {
                let i = row + x;
                let mut v = self.cells[i] as i32 + delta[i];
                if Rng::at_percent(x, y, tick, SALT_EVAPORATE, EVAP_PERCENT) {
                    v -= 1;
                }
                self.cells[i] = v.clamp(0, 255) as u8;
            }
        }
        self.revision = self.revision.wrapping_add(1);
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

    /// 版本号：每次 `reseed` / `step` 自增。
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// 是不是一张空场（还没按世界算过）。
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 64 宽、水在 x<20，右边是陆地。地表在 y=40。
    fn shore() -> World {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 40, 64, 24, MaterialKind::Stone);
        w.fill_rect(0, 30, 20, 34, MaterialKind::Water);
        w
    }

    /// 建场之后，紧挨着水的土几乎是满的。
    #[test]
    fn the_seed_profile_is_wet_next_to_water() {
        let w = shore();
        let mut m = Moisture::new();
        m.reseed(&w);
        assert!(m.at(20, 39) > 240, "水边应当是满值：{}", m.at(20, 39));
    }

    /// **蒸发真的会让土变干**，而且离水越远干得越快（没有补给）。
    ///
    /// 这是"有记忆"这件事的另一半：如果只吸水不蒸发，那整张场迟早
    /// 全变 255，雨就变成了"永久湿地"。
    #[test]
    fn soil_dries_out_without_a_water_source() {
        let w = shore();
        let mut m = Moisture::new();
        m.reseed(&w);
        let far_before = m.at(60, 39);
        // 把水抽干：之后再没有补给。
        let mut dry = World::new(64, 64).unwrap();
        dry.fill_rect(0, 40, 64, 24, MaterialKind::Stone);

        for tick in 0..6000u64 {
            m.step(&dry, tick);
        }
        assert!(
            m.at(60, 39) < far_before / 2,
            "没有水源时应当明显变干：{} → {}",
            far_before,
            m.at(60, 39)
        );
        // 最远处应当已经干透（离水 44 格）。
        assert!(m.at(60, 39) < 40, "远处应当干到根吸不动：{}", m.at(60, 39));
    }

    /// **水一直在，就一直是湿的**——吸水 + 扩散得撑得住蒸发。
    ///
    /// 这条如果坏了，症状是"海边的土慢慢变成沙漠"，而画面上完全看不出
    /// 是谁干的（水还在、根就是吸不到）。
    #[test]
    fn a_steady_shore_stays_wet() {
        let w = shore();
        let mut m = Moisture::new();
        m.reseed(&w);
        for tick in 0..8000u64 {
            m.step(&w, tick);
        }
        assert!(m.at(20, 39) > 240, "水边的土不该变干：{}", m.at(20, 39));
        assert!(m.at(30, 39) > 150, "离岸 10 格应当仍然够湿：{}", m.at(30, 39));
        // 也不能塌掉——这是"稳态范围和播种剖面大致对得上"的判据。
        assert!(
            m.at(50, 39) > 20,
            "离岸 30 格不该干透（稳态范围要接近 SPAN）：{}",
            m.at(50, 39)
        );
    }

    /// **雨的记忆**：往岸上倒一片水，跑一阵之后水没了，但土还记得。
    ///
    /// 这就是这一整轮改动的目的。旧模型里"水格消失"和"湿度归零"是同一
    /// 时刻，所以雨留不下任何东西。
    ///
    /// 世界要**够宽**才有"干的内陆"可用：64 宽、`SPAN = 200` 的世界里
    /// 最远的一格离海也才 44 格，按距离公式已经是 235 分——在那里下雨
    /// 最多只能涨到 255，测不出"变湿"。400 宽时 x=200 是干的。
    #[test]
    fn a_rain_remembers_itself_after_the_water_is_gone() {
        let mut w = World::new(400, 64).unwrap();
        w.fill_rect(0, 40, 400, 24, MaterialKind::Stone);
        w.fill_rect(0, 30, 20, 34, MaterialKind::Water);
        let mut m = Moisture::new();
        m.reseed(&w);
        let before = m.at(200, 39);
        assert!(before < 60, "内陆本来就该是干的（否则测不出雨）：{before}");

        // 模拟"下了一场雨"：在内陆放一格水，跑 200 tick（吸水 + 扩散）。
        let mut rainy = World::new(400, 64).unwrap();
        rainy.fill_rect(0, 40, 400, 24, MaterialKind::Stone);
        rainy.fill_rect(0, 30, 20, 34, MaterialKind::Water);
        rainy.fill_rect(200, 39, 1, 1, MaterialKind::Water);
        for tick in 0..200u64 {
            m.step(&rainy, tick);
        }
        let wet = m.at(200, 39);
        assert!(
            wet as i32 > before as i32 + 100,
            "雨应当让那一格明显变湿：{before} → {wet}"
        );

        // 水没了（雨流走了 / 被蒸发了）。
        for tick in 200..400u64 {
            m.step(&w, tick);
        }
        let after = m.at(200, 39);
        assert!(
            after as i32 > before as i32 + 50,
            "雨走了之后土还该记得它：雨前 {before}、雨中 {wet}、雨后 {after}"
        );
    }
}
