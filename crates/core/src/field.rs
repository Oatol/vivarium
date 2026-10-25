//! 空气场：粗粒度的**光照**与**温度**。
//!
//! ## 为什么是粗粒度的
//!
//! 每像素一个 `f32` 的温度场在 512² 上是 1 MB，而扩散每次要扫两遍世界——
//! 那是材质层成本的量级，为了一个"看得见的梯度"不值得。4×4 一格之后是 128²，
//! 扫描成本降到 1/16，而画面上仍然是平滑的色块：**温度本来就该是模糊的**，
//! 它的真实分辨率远低于像素。
//!
//! ## 光和温度是两种完全不同的东西
//!
//! - **光是一个纯函数**：给定材质布局和光源位置，光场就唯一确定。
//!   所以它不需要积分，每次从头重算即可（低频重算，见 [`LIGHT_INTERVAL`]）。
//! - **温度是一个状态**：它要积分（扩散 + 注入 + 冷却），必须保留上一帧。
//!
//! 把两者混成一张"每 tick 都更新一下"的表，会逼出"光也要积分"这种错误的模型——
//! 而光的积分会累积误差，最后**关掉所有光源之后世界仍然亮着**。
//!
//! ## 光只被材质遮挡
//!
//! 生物**不**参与光的计算。这不是偷懒：一条鱼挡住一片光，会让整个光场
//! 变成 O(生物数 × 遮挡面积) 的活，而且它每 tick 都在动。等有了需要它的
//! 玩法（比如巨藻林里的阴影）再加，那时它会是一个**显式的**规则，
//! 而不是"顺手也算了一下"。

use crate::material::MaterialKind;
use crate::world::World;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// 一个粗格覆盖多少像素（边长）。
///
/// 4 是"够粗又不糊"的那个值：再粗一点，岛礁的轮廓就变成一团色块，
/// 滤镜子看不清哪儿是岸；再细一点，扫描成本翻倍而画面几乎没有区别。
pub const AIR_CELL: usize = 4;

/// 环境温度（°C）。世界的热平衡点。
pub const AMBIENT_TEMP: f32 = 20.0;

/// 火所在粗格的温度上限。
pub const FIRE_TEMP: f32 = 900.0;

/// 火每次更新往自己那一格注入多少度。
///
/// 必须足够大才能压过扩散：注入 320、扩散每步拉走 `0.22 × 温差`，
/// 实测火焰那一格稳定在 600–850°C，而旁边一格是 200–400°C——
/// 那正是"看得见一团火"需要的梯度。
pub const FIRE_HEAT: f32 = 320.0;

/// 阳光在**水里**走多少格衰减到 0。
///
/// 这个数就是生物眼里的"世界有多深"。180 格之后一片漆黑，
/// 而 512² 的海平均 300 格深——于是海床是**真的**没有光，
/// 不是"光很少"。
pub const SUN_ATTEN: f32 = 180.0;

/// 一个粗格被生物**完全占满**时，它挡住下面多少比例的光。
///
/// 0.2：满覆盖的粗行透光 80%，一条 4 格的鱼（25% 覆盖）只挡 5%。
///
/// ## 为什么不是 0.5——这条是量出来的
///
/// 遮挡是按粗行**连乘**的，而鱼群聚在透光层，于是十几个粗行都压着鱼。
/// 实测（三个种子 × 8000 tick，512²）：
///
/// | `BODY_SHADE` | 动物数 | 物种数 |
/// |---|---|---|
/// | 0（关） | 1617 / 1583 / 1088 | 350 / 334 / 271 |
/// | **0.2** | **645 / 320 / 340** | 137 / 126 / 115 |
/// | 0.5 | 196 / 176 / 99 | 52 / 78 / 58 |
///
/// 0.5 是**八倍**的崩塌：`0.81^10 ≈ 0.12`，整列水下直接黑掉。
/// 0.2 保住了生态（约为不遮挡时的三分之一），而"密实的身体压暗下面"
/// 这件事仍然成立。**这个数是"生态能承受的最大值"，不是"物理上最像的值"。**
///
/// ## 为什么按"覆盖度"而不是"有没有生物"
///
/// 粗格是 4×4，一条 4 格的鱼只占 25%——它不该把下面整列变黑。而一片密实的
/// 藻席或一丛海草能占满整格，那才是真的遮挡。
///
/// **这个分辨率决定了"竞争"发生在个体之间还是群落之间。** 单株植物的影子
/// 在 4×4 的粗格里几乎不存在（1/16），所以"高个子挡住矮个子"在**单株**层面
/// 不会发生；但成片的植被会。而真实生态里植物的光竞争本来也是成片发生的
/// ——所以这里刻意选了"群落级"，并且把它写清楚，免得以后有人以为是 bug。
pub const BODY_SHADE: f32 = 0.2;

/// 点光源的作用半径（像素）。
///
/// 110 格 ≈ 一次落笔半径的二十倍，足够照亮一小片海床；
/// 再大就会让"放一盏灯"变成"点亮半张图"，滤镜里看不出是点光源。
pub const POINT_RANGE: f32 = 110.0;

/// 光场重算间隔（tick）。
///
/// 8 的理由：光只被**材质**遮挡，而材质里移动最快的沙水一 tick 走一格，
/// 8 tick 的滞后在画面上是一团边缘稍微晚半拍跟上的影子——看不出来。
/// 而每次重算要扫一遍世界，1 tick 一次就是 +100% 的材质层成本。
pub const LIGHT_INTERVAL: u64 = 8;

/// 温度场更新间隔（tick）。
///
/// 和光场用同一个间隔，因为两者共用**同一次世界扫描**——
/// 分开设间隔就等于扫两遍世界。
pub const TEMP_INTERVAL: u64 = 8;

/// 显式四邻域扩散的稳定上限是 0.25，取 0.22 留余量。
const DIFFUSE: f32 = 0.22;

/// 每次更新跑几个扩散子步。
///
/// 更新间隔是 8 tick，一个子步的扩散太慢（热量爬不出几格），
/// 两个子步把"每次更新"的有效扩散率提到约 0.4——仍然稳定，
/// 因为**每个子步**的系数都还在 0.25 以下。
const DIFFUSE_STEPS: u32 = 2;

/// 向环境温度回归的速率。
///
/// 这是"热量会散掉"的唯一通道。没有它，一团火熄灭之后那一片
/// 会永远停在 800°C——一个只进不出的状态，正是封闭世界的铁律 #1。
const COOLING: f32 = 0.004;

/// 光源数量上限。
///
/// 不是性能考虑（传播是 O(格数)，和光源数无关），而是**可用性**：
/// 超过这个数之后滤镜上只剩一片白，"哪盏灯照的"已经无法回答。
pub const SOURCE_CAP: usize = 48;

/// 温度场的显示上限（°C）。滤镜把 `[0, 这个值]` 映射到整条色带。
pub const TEMP_DISPLAY_MAX: f32 = 1200.0;

// ---------------------------------------------------------------------------
// 光源
// ---------------------------------------------------------------------------

/// 一个手动放置的点光源。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LightSource {
    /// 世界坐标（格）。
    pub x: i32,
    pub y: i32,
    /// 强度，0..1。1.0 和正午的阳光一样亮。
    pub intensity: f32,
}

impl LightSource {
    /// 新建一盏满强度的灯。
    pub const fn new(x: i32, y: i32) -> Self {
        Self {
            x,
            y,
            intensity: 1.0,
        }
    }
}

// ---------------------------------------------------------------------------
// 显示滤镜
// ---------------------------------------------------------------------------

/// 世界怎么画。
///
/// 照着 TPT 的 `DisplayMode` 定：同一份世界数据，换一种"看的方式"。
/// 这不是美化——**材质视图看不见温度，温度视图看不见物种**，
/// 而调试生态时你恰恰需要在两者之间来回切。
///
/// 住在 core 而不是 render：界面要用它画那排按钮，而界面只依赖 core
/// （见 `crates/ui` 的 crate 文档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewMode {
    /// 材质 + 细胞的本来颜色。默认视图。
    #[default]
    Material,
    /// 温度：蓝 → 青 → 绿 → 黄 → 红 → 品红（照搬 TPT 的 `heatTable`）。
    Temperature,
    /// 光照：黑 → 深蓝 → 青 → 白。
    Light,
    /// 物种：每个个体一个色相，材质压成暗剪影。
    Species,
    /// 土壤湿度：水满值，往陆地深处衰减。
    ///
    /// 和温度、光照一样是"把一层不可见的数据变成可看的图"，但它**看材质**：
    /// 只画固体和水（空气不显示湿度——否则整根柱子连天空都是同一个颜色，
    /// 而且"空气有多湿"这句话本身没有意义）。
    Moisture,
}

impl ViewMode {
    /// 全部滤镜，按界面上的顺序。
    pub const ALL: [Self; 5] = [
        Self::Material,
        Self::Temperature,
        Self::Light,
        Self::Species,
        Self::Moisture,
    ];

    /// 按钮上的字。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Material => "材质",
            Self::Temperature => "温度",
            Self::Light => "光照",
            Self::Species => "物种",
            Self::Moisture => "湿度",
        }
    }

    /// 一句话说明它看得见什么。
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Material => "材质与细胞的本来颜色。",
            Self::Temperature => "热场：蓝是冷，红是热，火会烧出一团亮斑。",
            Self::Light => "光场：太阳从顶上照下来，被石头和沙挡住；点光源会照亮周围。",
            Self::Species => "物种：每个个体一个色相，材质压成暗剪影。",
            Self::Moisture => {
                "土壤湿度：水是满值，往陆地深处衰减；越干越暖，越湿越蓝。"
            }
        }
    }

    /// 下一个滤镜（循环）。
    pub const fn next(self) -> Self {
        match self {
            Self::Material => Self::Temperature,
            Self::Temperature => Self::Light,
            Self::Light => Self::Species,
            Self::Species => Self::Moisture,
            Self::Moisture => Self::Material,
        }
    }

    /// 打包成 shader 里的整数。**必须与 `blit.wgsl` 的 `VIEW_*` 常数一致。**
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Material => 0,
            Self::Temperature => 1,
            Self::Light => 2,
            Self::Species => 3,
            Self::Moisture => 4,
        }
    }
}

// ---------------------------------------------------------------------------
// 场
// ---------------------------------------------------------------------------

/// 场的统计，给界面显示。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldStats {
    /// 光源数。
    pub sources: usize,
    /// 光照平均值（0..1）。
    pub avg_light: f32,
    /// 最亮的那一格。
    pub max_light: f32,
    /// 温度范围。
    pub temp_range: (f32, f32),
    /// 平均温度。
    pub avg_temp: f32,
}

impl Default for FieldStats {
    fn default() -> Self {
        Self {
            sources: 0,
            avg_light: 0.0,
            max_light: 0.0,
            temp_range: (AMBIENT_TEMP, AMBIENT_TEMP),
            avg_temp: AMBIENT_TEMP,
        }
    }
}

/// 粗粒度的空气场。
#[derive(Debug, Clone)]
pub struct AirField {
    cols: usize,
    rows: usize,
    /// 光照，0..1。
    light: Vec<f32>,
    /// 温度，°C。
    temp: Vec<f32>,
    /// 扩散的中间缓冲。
    next: Vec<f32>,
    sources: Vec<LightSource>,

    // 一次世界扫描的中间结果。留在结构体里而不是每次现分配：
    // 这是每 8 tick 一次的热路径，反复分配 4 个 16k 的 Vec 会被 allocator 记账。
    opaque: Vec<u8>,
    sun_loss: Vec<f32>,
    conduct: Vec<f32>,
    fire: Vec<u8>,
    /// 每个粗格里有多少格是水（0..16）。
    ///
    /// 存在的理由只有一个，但它很硬：**光合作用需要水**。
    /// 没有这一条，一条被扔在岸上的鱼会顶着满格阳光疯狂产能量，
    /// 于是"搁浅会死"这件事在物理上就不成立了——而那是用户明确要的行为。
    water: Vec<u8>,

    /// 每个粗格里有多少格被**生物**占着（0..16）。
    ///
    /// v0.33 之前光场只认材质，"生物本身先不遮挡"。那样做有一个后果：
    /// **长高没有任何收益**——自己挡自己只有损失，而挡住邻居才是植物长高的
    /// 唯一理由。于是植物永远长不大，"高大 > 扁平"在物理上不可能出现。
    ///
    /// 这个数组就是那条规则的数据：`sun_march` 用它按覆盖度给下面的格打折。
    body: Vec<u8>,

    /// 每次 `update_light` / `update_temp` 自增。渲染器靠它判断"要不要重传纹理"。
    revision: u64,
}

impl Default for AirField {
    /// 0×0 的空场。用在"还没有世界"的地方（比如 [`crate::LifeSim::default`]）。
    ///
    /// 空场所有查询都返回默认值（光 0、温度环境），所以它不会让任何调用点
    /// 需要额外的分支——这正是"空对象"该有的样子。
    fn default() -> Self {
        Self {
            cols: 0,
            rows: 0,
            light: Vec::new(),
            temp: Vec::new(),
            next: Vec::new(),
            sources: Vec::new(),
            opaque: Vec::new(),
            sun_loss: Vec::new(),
            conduct: Vec::new(),
            fire: Vec::new(),
            water: Vec::new(),
            body: Vec::new(),
            revision: 0,
        }
    }
}

impl AirField {
    /// 按世界尺寸新建。温度初始化为环境温度，光照全 0（第一次 `update_light` 填满）。
    pub fn new(world: &World) -> Self {
        let cols = world.width().div_ceil(AIR_CELL);
        let rows = world.height().div_ceil(AIR_CELL);
        let n = cols * rows;
        Self {
            cols,
            rows,
            light: vec![0.0; n],
            temp: vec![AMBIENT_TEMP; n],
            next: vec![AMBIENT_TEMP; n],
            sources: Vec::new(),
            opaque: vec![0; n],
            sun_loss: vec![0.0; n],
            conduct: vec![1.0; n],
            fire: vec![0; n],
            water: vec![0; n],
            body: vec![0; n],
            revision: 0,
        }
    }

    /// 粗网格列数。
    #[inline]
    pub const fn cols(&self) -> usize {
        self.cols
    }

    /// 粗网格行数。
    #[inline]
    pub const fn rows(&self) -> usize {
        self.rows
    }

    /// 版本号。每次场被重算就变。
    #[inline]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// 光照整张表（行优先，长度 `cols * rows`）。
    #[inline]
    pub fn light(&self) -> &[f32] {
        &self.light
    }

    /// 温度整张表（行优先）。
    #[inline]
    pub fn temp(&self) -> &[f32] {
        &self.temp
    }

    /// 全部光源。
    #[inline]
    pub fn sources(&self) -> &[LightSource] {
        &self.sources
    }

    /// 世界坐标 → 粗格下标。
    #[inline]
    fn coarse(&self, x: f32, y: f32) -> usize {
        let cx = (x.max(0.0) as usize / AIR_CELL).min(self.cols.saturating_sub(1));
        let cy = (y.max(0.0) as usize / AIR_CELL).min(self.rows.saturating_sub(1));
        cy * self.cols + cx
    }

    /// 某一格的光照（0..1）。越界按最近的边缘算。
    #[inline]
    pub fn light_at(&self, x: f32, y: f32) -> f32 {
        self.light.get(self.coarse(x, y)).copied().unwrap_or(0.0)
    }

    /// 某一格的温度（°C）。
    #[inline]
    pub fn temp_at(&self, x: f32, y: f32) -> f32 {
        self.temp
            .get(self.coarse(x, y))
            .copied()
            .unwrap_or(AMBIENT_TEMP)
    }

    /// 某个粗格里水的占比（0..1）。
    ///
    /// 判据是"这一片是不是泡在水里"，粒度是粗格——这正是它该有的粒度：
    /// 问"这一格是不是水"会得到"身体格永远是 `Empty`"这种没用的答案。
    #[inline]
    pub fn water_frac(&self, x: f32, y: f32) -> f32 {
        let i = self.coarse(x, y);
        self.water
            .get(i)
            .map(|w| f32::from(*w) / (AIR_CELL * AIR_CELL) as f32)
            .unwrap_or(0.0)
    }

    /// 加一盏灯。超出 [`SOURCE_CAP`] 返回 `false`。
    pub fn add_source(&mut self, x: i32, y: i32) -> bool {
        if self.sources.len() >= SOURCE_CAP {
            return false;
        }
        self.sources.push(LightSource::new(x, y));
        true
    }

    /// 删掉 `(x, y)` 附近 `radius` 格内最近的一盏灯。返回被删掉的那个。
    ///
    /// 用"最近"而不是"精确命中"：灯在画面上是一个点，而鼠标落点有半格误差，
    /// 要求精确命中等于要求用户把鼠标对准一个 1px 的东西。
    pub fn remove_source_near(&mut self, x: i32, y: i32, radius: i32) -> Option<LightSource> {
        let r = radius.max(1);
        let mut best: Option<(i32, usize)> = None;
        for (i, s) in self.sources.iter().enumerate() {
            let d = (s.x - x).abs().max((s.y - y).abs());
            if d <= r && best.map(|(bd, _)| d < bd).unwrap_or(true) {
                best = Some((d, i));
            }
        }
        best.map(|(_, i)| self.sources.remove(i))
    }

    /// 清空所有光源。
    pub fn clear_sources(&mut self) {
        self.sources.clear();
    }

    /// 统计。
    pub fn stats(&self) -> FieldStats {
        if self.light.is_empty() {
            return FieldStats::default();
        }
        let n = self.light.len() as f32;
        let mut sum_l = 0.0f32;
        let mut max_l = 0.0f32;
        let mut sum_t = 0.0f32;
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for i in 0..self.light.len() {
            sum_l += self.light[i];
            max_l = max_l.max(self.light[i]);
            sum_t += self.temp[i];
            lo = lo.min(self.temp[i]);
            hi = hi.max(self.temp[i]);
        }
        FieldStats {
            sources: self.sources.len(),
            avg_light: sum_l / n,
            max_light: max_l,
            temp_range: (lo, hi),
            avg_temp: sum_t / n,
        }
    }

    /// 重算光场。**纯函数**：结果只取决于当前的材质布局和光源列表。
    pub fn update_light(&mut self, world: &World) {
        self.scan(world);
        self.sun_march();
        self.point_propagate();
        self.revision = self.revision.wrapping_add(1);
    }

    /// 推进温度场：注入 → 扩散 → 冷却。
    pub fn update_temp(&mut self, world: &World) {
        self.scan(world);
        let n = self.temp.len();

        // 1) 扩散。Jacobi 迭代：读旧表、写新表，再交换。
        //    原地写（Gauss-Seidel）会引入方向偏置——热量朝扫描方向跑得更快，
        //    而那个偏置在温度滤镜上是**看得见的斜向拖尾**。
        for _ in 0..DIFFUSE_STEPS {
            for cy in 0..self.rows {
                for cx in 0..self.cols {
                    let i = cy * self.cols + cx;
                    let t = self.temp[i];
                    // 越界按自己算 = 绝热边界。世界边缘不该凭空散热，
                    // 否则靠边的火会明显比中间的弱。
                    let l = if cx > 0 { self.temp[i - 1] } else { t };
                    let r = if cx + 1 < self.cols {
                        self.temp[i + 1]
                    } else {
                        t
                    };
                    let u = if cy > 0 { self.temp[i - self.cols] } else { t };
                    let d = if cy + 1 < self.rows {
                        self.temp[i + self.cols]
                    } else {
                        t
                    };
                    let avg = (l + r + u + d) * 0.25;
                    self.next[i] = t + (avg - t) * DIFFUSE * self.conduct[i];
                }
            }
            std::mem::swap(&mut self.temp, &mut self.next);
        }

        // 2) 冷却。向环境温度回归，**永远**有一条出路——
        //    这是"封闭世界不能有吸收态"（SPEC §2.2）在热场上的版本。
        for t in self.temp.iter_mut() {
            *t += (AMBIENT_TEMP - *t) * COOLING;
        }

        // 3) 注入。火把这一格加热到「火焰温度 × 覆盖度」，**用 `max` 而不是累加**。
        //
        // 放在扩散和冷却**之后**，是这一版的三处修正之一。三件事都是
        // 玩家一句"火的温度只有 22 °C"逼出来的：
        //
        // - **摊薄是错的**。旧实现按覆盖度线性加（`320 × 1/16 = 20`），
        //   一格火只把粗格加热 20 度，读数 40 °C。而"这个粗格里有火"和
        //   "这个粗格平均多少度"是两件事：1/16 是 900 度的火焰、
        //   15/16 是 20 度的水，平均值没有意义。下限取 `FIRE_HEAT`。
        // - **累加追不上扩散**。每 8 tick 加一次、同时被扩散拿走一半，
        //   永远到不了火焰温度。改成 `max` 之后火灭之前温度是稳的。
        // - **顺序**。注入放在扩散之前，刚设好的 900 度会被本 tick 的
        //   扩散立刻摊掉一半（实测满格火只剩 623）。放到最后，
        //   读到的就是火焰温度本身；热量从下一 tick 开始往外流。
        for i in 0..n {
            if self.fire[i] > 0 {
                let coverage = f32::from(self.fire[i]) / (AIR_CELL * AIR_CELL) as f32;
                let target = FIRE_HEAT + (FIRE_TEMP - FIRE_HEAT) * coverage;
                if self.temp[i] < target {
                    self.temp[i] = target;
                }
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }

    /// 立刻重算光场和温度（不按间隔）。
    ///
    /// 用在"暂停时放了一盏灯"这种地方：时钟不走，靠 `% INTERVAL` 的判据
    /// 就永远不会触发，而用户会盯着一个没反应的世界。
    pub fn refresh(&mut self, world: &World) {
        self.update_light(world);
        self.update_temp(world);
    }

    /// 读档：把光源与温度表整体装回去，然后**只**重算光照。
    ///
    /// 为什么重算光照却不重算温度：光照是布局的**纯函数**（关灯即暗），
    /// 读回来的世界布局已经变了，那份光照必然作废；而温度是**积分状态**，
    /// 存的就是它此刻的值，再松弛一步等于把存档时刻的温度抹掉一点点——
    /// 读档之后"和存档时不一样"这件事，恰恰是玩家最容易察觉的那种不对。
    ///
    /// `temp` 长度不符时返回 `false` 并且**不改变任何东西**：一个坏档
    /// 应该整个被拒绝，而不是装进去一半。
    pub fn restore(&mut self, world: &World, sources: &[LightSource], temp: &[f32]) -> bool {
        if temp.len() != self.temp.len() {
            return false;
        }
        self.temp.copy_from_slice(temp);
        self.next.copy_from_slice(temp);
        self.sources.clear();
        // 超上限的灯直接丢掉，而不是让 `sources` 无界增长：
        // 上限是渲染器和界面都假定的（`SOURCE_CAP` 个圆环），
        // 读档路径没有理由成为唯一能突破它的入口。
        self.sources
            .extend(sources.iter().copied().take(SOURCE_CAP));
        self.update_light(world);
        true
    }

    // -- 内部 ---------------------------------------------------------------

    /// 扫一遍世界，把每个粗格的材质摘要算出来。
    ///
    /// 光和温度共用这一份摘要——它们是同一次世界扫描的两个消费者，
    /// 各扫一遍等于把最贵的那一步做两次。
    fn scan(&mut self, world: &World) {
        let (w, h) = (world.width(), world.height());
        let area = (AIR_CELL * AIR_CELL) as f32;
        for cy in 0..self.rows {
            for cx in 0..self.cols {
                let i = cy * self.cols + cx;
                let mut solid = 0u32;
                let mut loss = 0.0f32;
                let mut cond = 0.0f32;
                let mut flame = 0u32;
                let mut wet = 0u32;
                let mut occupied = 0u32;
                let mut counted = 0u32;
                for py in 0..AIR_CELL {
                    let y = cy * AIR_CELL + py;
                    if y >= h {
                        break;
                    }
                    for px in 0..AIR_CELL {
                        let x = cx * AIR_CELL + px;
                        if x >= w {
                            break;
                        }
                        let k = world.get_unchecked(x, y);
                        counted += 1;
                        loss += sun_loss(k);
                        cond += conductivity(k);
                        // 生物占着的格：不算固体（不该让整列变黑），
                        // 但会计进 `body`，由 `sun_march` 按覆盖度打折。
                        if world.owners()[world.index_of(x, y)].is_some() {
                            occupied += 1;
                        }
                        match k {
                            MaterialKind::Stone | MaterialKind::Sand => solid += 1,
                            MaterialKind::Fire => flame += 1,
                            MaterialKind::Water => wet += 1,
                            _ => {}
                        }
                    }
                }
                let n = counted.max(1) as f32;
                // 超过一半是固体就算"挡住"。用一半而不是"有一格就算"：
                // 一粒沙不该在整张图上投下一条 4px 宽的水久阴影。
                self.opaque[i] = u8::from(solid as f32 >= area * 0.5);
                self.sun_loss[i] = loss / n;
                self.conduct[i] = (cond / n).clamp(0.0, 1.0);
                self.fire[i] = flame.min(255) as u8;
                self.water[i] = wet.min(255) as u8;
                self.body[i] = occupied.min(255) as u8;
            }
        }
    }

    /// 太阳：逐列自上而下，边走边扣。
    ///
    /// 为什么不用"从顶行往下的通用传播"：那样光会从侧面绕到悬崖下面，
    /// 而太阳是**平行光**——它照不到的地方就该是黑的。逐列扫描是
    /// 唯一能把这件事说清楚的写法，而且它 O(格数)、只扫一遍。
    fn sun_march(&mut self) {
        let step = AIR_CELL as f32 / SUN_ATTEN;
        for cx in 0..self.cols {
            let mut acc = 1.0f32;
            let mut y = 0;
            while y < self.rows {
                let i = y * self.cols + cx;
                if self.opaque[i] != 0 {
                    // 从这里往下全黑。整段填 0 再跳出——
                    // 一列到底通常只有几格石头，多填的代价可以忽略。
                    for yy in y..self.rows {
                        self.light[yy * self.cols + cx] = 0.0;
                    }
                    break;
                }
                acc = (acc - self.sun_loss[i] * step).max(0.0);
                self.light[i] = acc;
                // **身体遮挡，放在存完 `light[i]` 之后。**
                //
                // 顺序是有意的：生物拿到的仍然是"头顶来的光"，它挡的是
                // **下面**的格。反过来（先挡再存）会让每一具身体都把自己
                // 压暗，于是"多长一格"变成自我惩罚——那不是竞争，是自杀。
                let cover = self.body[i] as f32 / (AIR_CELL * AIR_CELL) as f32;
                if cover > 0.0 {
                    acc *= 1.0 - BODY_SHADE * cover;
                }
                y += 1;
            }
        }
    }

    /// 点光源：四方向扫描传播。
    ///
    /// 这是"到光源的最短路径"的近似：每走一格扣 `step`，取四邻里的最大值。
    /// 四个方向扫两轮，误差只出现在"绕过一个 L 形拐角"的地方，
    /// 而那个误差在滤镜上表现为拐角内侧稍暗——比精确的 Dijkstra 更好看，
    /// 因为真实的光本来就在拐角处衰减得更厉害。
    fn point_propagate(&mut self) {
        if self.sources.is_empty() {
            return;
        }
        let step = AIR_CELL as f32 / POINT_RANGE;

        // 种子：光源所在的粗格。
        for s in &self.sources {
            if s.x < 0 || s.y < 0 {
                continue;
            }
            let cx = s.x as usize / AIR_CELL;
            let cy = s.y as usize / AIR_CELL;
            if cx < self.cols && cy < self.rows {
                let i = cy * self.cols + cx;
                if self.light[i] < s.intensity {
                    self.light[i] = s.intensity;
                }
            }
        }

        let (cols, rows) = (self.cols, self.rows);
        for _ in 0..2 {
            // →
            for cy in 0..rows {
                for cx in 1..cols {
                    let i = cy * cols + cx;
                    relax(&mut self.light, &self.opaque, i, i - 1, step);
                }
            }
            // ←
            for cy in 0..rows {
                for cx in (0..cols.saturating_sub(1)).rev() {
                    let i = cy * cols + cx;
                    relax(&mut self.light, &self.opaque, i, i + 1, step);
                }
            }
            // ↓
            for cy in 1..rows {
                for cx in 0..cols {
                    let i = cy * cols + cx;
                    relax(&mut self.light, &self.opaque, i, i - cols, step);
                }
            }
            // ↑
            for cy in (0..rows.saturating_sub(1)).rev() {
                for cx in 0..cols {
                    let i = cy * cols + cx;
                    relax(&mut self.light, &self.opaque, i, i + cols, step);
                }
            }
        }
    }
}

/// 从邻居 `src` 往 `dst` 传一份光。固体格既不接收也不传出。
#[inline]
fn relax(light: &mut [f32], opaque: &[u8], dst: usize, src: usize, step: f32) {
    if opaque[dst] != 0 || opaque[src] != 0 {
        return;
    }
    let v = light[src] - step;
    if v > light[dst] {
        light[dst] = v;
    }
}

/// 每像素的阳光损耗，单位是"走一格水"。石头和沙由 `opaque` 单独处理。
#[inline]
fn sun_loss(kind: MaterialKind) -> f32 {
    match kind {
        MaterialKind::Empty => 0.05,
        MaterialKind::Water => 1.0,
        MaterialKind::Gas => 0.12,
        MaterialKind::Fire => 0.5,
        // 碎屑和尸体挡光挡得比水厉害得多：海底沉积层本来就该是暗的。
        MaterialKind::Remains | MaterialKind::Corpse => 1.5,
        // 固体走 `opaque`，这里给的值只影响"刚好不到一半"的格子。
        MaterialKind::Stone | MaterialKind::Sand => 4.0,
    }
}

/// 每像素的导热系数（0..1）。
///
/// 水比石头高：水会**对流**，而石头只能靠传导。这是两条不同的物理，
/// 但在这张粗网格上合成一个数字就够了——目的是让"海底比岸上凉、
/// 火边上一圈暖"这种结构出现，不是做热力学仿真。
#[inline]
fn conductivity(kind: MaterialKind) -> f32 {
    match kind {
        MaterialKind::Empty => 0.30,
        MaterialKind::Water => 0.95,
        MaterialKind::Stone => 0.55,
        MaterialKind::Sand => 0.45,
        MaterialKind::Gas => 0.35,
        MaterialKind::Fire => 0.80,
        MaterialKind::Remains => 0.40,
        MaterialKind::Corpse => 0.50,
    }
}

/// 光合作用对温度的响应：25°C 附近最旺，两侧都掉。
///
/// 形状是 `1 - ((T-25)/25)²`，在 0°C 还有 0.36、到 50°C 归零。
/// 这不是拟合出来的曲线，是**给温度一个说得清的作用**：
/// 没有它，温度场就只是一张好看的图，而"好看但无用"的场
/// 迟早会因为没人维护而悄悄烂掉。
#[inline]
pub fn photosynthesis(temp: f32) -> f32 {
    let x = (temp - 25.0) / 25.0;
    (1.0 - x * x).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(w: usize, h: usize, water_y: usize) -> World {
        let mut world = World::new(w, h).unwrap();
        for y in water_y..h {
            for x in 0..w {
                world.set(x as isize, y as isize, MaterialKind::Water);
            }
        }
        world
    }

    #[test]
    fn sun_fades_with_depth_and_dies_at_the_attenuation_length() {
        let world = pool(64, 256, 0);
        let mut f = AirField::new(&world);
        f.update_light(&world);
        let top = f.light_at(32.0, 0.0);
        let mid = f.light_at(32.0, 90.0);
        let deep = f.light_at(32.0, 200.0);
        assert!(top > 0.9, "水面应当是满光，实测 {top}");
        assert!(mid < top * 0.75, "90 格深处应当明显变暗，实测 {mid}");
        assert!(deep < 0.02, "超过衰减长度之后应当基本无光，实测 {deep}");
    }

    #[test]
    fn stone_casts_a_shadow_all_the_way_down() {
        // 石头必须占满整个粗格（4×4）才算"挡住"——这是粗粒度的代价，
        // 也是它的意义：**一粒沙不该投下一条永久阴影**。
        // 所以这里画的是 8 格宽的石板，正好两个粗格。
        let mut world = pool(64, 128, 0);
        for y in 20..24 {
            for x in 30..38 {
                world.set(x, y, MaterialKind::Stone);
            }
        }
        let mut f = AirField::new(&world);
        f.update_light(&world);
        assert!(
            f.light_at(34.0, 100.0) < 0.001,
            "石头下面应当全黑，实测 {}",
            f.light_at(34.0, 100.0)
        );
        // 石板旁边是 100 格深的水，光本来就只剩 `1 - 100/180 ≈ 0.44`——
        // 所以判据只能是"比阴影里亮得多"，不是"很亮"。
        assert!(
            f.light_at(10.0, 100.0) > 0.3,
            "石板旁边应当照常亮，实测 {}",
            f.light_at(10.0, 100.0)
        );
    }

    #[test]
    fn a_lamp_lights_its_surroundings_and_nothing_far_away() {
        // 世界是**满水**的，所以到处都有阳光。要测点光源就得先把它关掉：
        // 顶上加一层厚石头当盖子，于是水下唯一的光就是那盏灯。
        let mut world = pool(128, 128, 0);
        for x in 0..128 {
            for y in 0..8 {
                world.set(x, y, MaterialKind::Stone);
            }
        }
        let mut f = AirField::new(&world);
        f.add_source(64, 64);
        f.update_light(&world);
        assert!(f.light_at(64.0, 64.0) > 0.9, "灯口应当是满的");
        assert!(
            f.light_at(64.0, 100.0) > 0.3,
            "灯下 36 格应当还亮，实测 {}",
            f.light_at(64.0, 100.0)
        );
        assert!(
            f.light_at(4.0, 4.0) < 0.01,
            "角落不该被照亮，实测 {}",
            f.light_at(4.0, 4.0)
        );
    }

    #[test]
    fn removing_a_lamp_puts_the_dark_back() {
        let mut world = pool(64, 64, 0);
        for x in 0..64 {
            for y in 0..8 {
                world.set(x, y, MaterialKind::Stone);
            }
        }
        let mut f = AirField::new(&world);
        f.add_source(32, 40);
        f.update_light(&world);
        assert!(f.light_at(32.0, 40.0) > 0.5);
        assert!(f.remove_source_near(33, 41, 3).is_some());
        f.update_light(&world);
        assert!(
            f.light_at(32.0, 50.0) < 0.2,
            "灯拿走之后水下应当回到黑暗，实测 {}",
            f.light_at(32.0, 50.0)
        );
    }

    /// 一格火也必须烫——玩家报过"火的温度只有 22 °C"。
    ///
    /// 旧实现按覆盖度**线性摊薄**：一格 4×4 里只有 1 格在烧，
    /// 加热量是 `FIRE_HEAT × 1/16 = 20`，读数 40 °C。
    /// 而"这个粗格里有火"和"这个粗格平均多少度"是两件事——
    /// 1/16 是 900 度的火焰、15/16 是 20 度的水，平均值没有意义。
    #[test]
    fn even_a_single_fire_pixel_is_hot() {
        let mut world = pool(64, 64, 0);
        world.set(31, 31, MaterialKind::Fire);
        let mut f = AirField::new(&world);
        f.update_temp(&world);
        let t = f.temp_at(31.0, 31.0);
        assert!(t > 300.0, "一格火应当把粗格烧到几百度，实测 {t}");
        assert!(t <= FIRE_TEMP, "不能超过火焰温度，实测 {t}");
    }

    /// 一整格都在烧，就该到火焰温度。
    #[test]
    fn a_full_block_of_fire_reaches_the_flame_temperature() {
        let mut world = pool(64, 64, 0);
        for y in 32..36 {
            for x in 32..36 {
                world.set(x, y, MaterialKind::Fire);
            }
        }
        let mut f = AirField::new(&world);
        f.update_temp(&world);
        let t = f.temp_at(33.0, 33.0);
        assert!((t - FIRE_TEMP).abs() < 1.0, "满格火应当到 {FIRE_TEMP}，实测 {t}");
    }

    #[test]
    fn fire_heats_its_cell_and_the_heat_leaks_sideways() {
        let mut world = pool(64, 64, 0);
        for y in 30..34 {
            for x in 30..34 {
                world.set(x, y, MaterialKind::Fire);
            }
        }
        let mut f = AirField::new(&world);
        for _ in 0..40 {
            f.update_temp(&world);
        }
        let hot = f.temp_at(31.0, 31.0);
        let near = f.temp_at(31.0, 44.0);
        let far = f.temp_at(4.0, 4.0);
        assert!(hot > 300.0, "火那一格应当很烫，实测 {hot}");
        assert!(near > AMBIENT_TEMP + 5.0, "旁边应当被烤热，实测 {near}");
        assert!(
            (far - AMBIENT_TEMP).abs() < 3.0,
            "远处应当还是环境温度，实测 {far}"
        );
    }

    #[test]
    fn heat_always_finds_its_way_back_to_ambient() {
        // 封闭世界的铁律 #1：不能有吸收态。火烧完之后那一片必须凉回来。
        let mut world = pool(64, 64, 0);
        for y in 30..34 {
            for x in 30..34 {
                world.set(x, y, MaterialKind::Fire);
            }
        }
        let mut f = AirField::new(&world);
        for _ in 0..30 {
            f.update_temp(&world);
        }
        assert!(f.temp_at(31.0, 31.0) > 100.0);
        for y in 30..34 {
            for x in 30..34 {
                world.set(x, y, MaterialKind::Empty);
            }
        }
        for _ in 0..4000 {
            f.update_temp(&world);
        }
        let after = f.temp_at(31.0, 31.0);
        assert!(
            (after - AMBIENT_TEMP).abs() < 2.0,
            "火灭了 4000 次更新之后应当回到环境温度，实测 {after}"
        );
    }

    #[test]
    fn photosynthesis_peaks_near_room_temperature() {
        assert!(photosynthesis(25.0) > 0.99);
        assert!(photosynthesis(0.0) < 0.4);
        assert!(photosynthesis(50.0) < 0.01);
        assert!(photosynthesis(900.0) == 0.0);
        // 环境温度下几乎不受影响——否则"给温度一个作用"就会变成
        // "顺手把整个生态削弱 4%"，而那种改动在指标上和一次成功的实验无法区分。
        assert!(photosynthesis(AMBIENT_TEMP) > 0.95);
    }

    #[test]
    fn light_is_a_pure_function_of_the_layout() {
        // 同一种子同一次扫描，跑两遍结果必须逐格相同。
        // 光一旦开始"积分"，这条就不再成立——而它的症状是
        // "关掉所有光源之后世界仍然亮着"。
        let mut world = pool(64, 64, 30);
        world.set(20, 30, MaterialKind::Stone);
        let mut f = AirField::new(&world);
        f.update_light(&world);
        let a: Vec<f32> = f.light().to_vec();
        for _ in 0..5 {
            f.update_light(&world);
        }
        let b: Vec<f32> = f.light().to_vec();
        assert_eq!(a, b, "光场重算之间不该有任何漂移");
    }

    #[test]
    fn field_matches_world_size_rounded_up() {
        let world = pool(64, 64, 0);
        let f = AirField::new(&world);
        assert_eq!(f.cols(), 64 / AIR_CELL);
        assert_eq!(f.rows(), 64 / AIR_CELL);
        assert_eq!(f.light().len(), f.cols() * f.rows());
    }

    #[test]
    fn view_modes_cycle_back_to_the_start() {
        let mut m = ViewMode::Material;
        for _ in 0..ViewMode::ALL.len() {
            m = m.next();
        }
        assert_eq!(m, ViewMode::Material);
        // 编号是 shader 的约定，改它就要改 `blit.wgsl`。
        for (i, v) in ViewMode::ALL.iter().enumerate() {
            assert_eq!(v.as_u32() as usize, i);
        }
    }

    /// 512² 世界上一次「光重算 + 温度积分」花多久。
    ///
    /// 这条不进常规测试（`cargo test -- --ignored --nocapture` 才跑），
    /// 因为它是**测量**不是断言。留着它的理由：光场每 8 tick 全量重算一次，
    /// 这个数字决定了 `LIGHT_INTERVAL` 还能不能再调小，
    /// 而"凭感觉觉得够快"在换了机器之后会失效。
    #[test]
    #[ignore]
    fn field_update_cost_on_a_512_world() {
        use std::time::Instant;

        let mut world = pool(512, 512, 100);
        // 一点起伏，免得整列同高、`sun_march` 退化成同一个循环跑 128 遍。
        for x in 0..512 {
            let top = 96 + ((x as f32 * 0.07).sin() * 18.0) as usize;
            for y in top..100 {
                world.set(x as isize, y as isize, MaterialKind::Stone);
            }
        }
        let mut f = AirField::new(&world);
        for _ in 0..4 {
            f.update_light(&world);
            f.update_temp(&world);
        }
        for i in 0..8 {
            f.add_source(40 + i * 55, 60);
        }

        let n = 20;
        let t0 = Instant::now();
        for _ in 0..n {
            f.update_light(&world);
        }
        let light = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        let t0 = Instant::now();
        for _ in 0..n {
            f.update_temp(&world);
        }
        let temp = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;

        // 每 8 tick 各一次 → 摊到每 tick 的额外成本。
        println!(
            "光重算 {light:.3} ms · 温度积分 {temp:.3} ms · 摊到每 tick {:.3} ms",
            (light + temp) / 8.0
        );
        assert!(light + temp < 40.0, "光场太贵了，会吃掉整个 tick 预算");
    }
}
