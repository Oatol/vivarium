//! 界面的状态与命令。
//!
//! ## 一条约束：界面不认识模拟
//!
//! 这一层只做两件事：**读一份快照，发一批命令**。
//!
//! - 读：[`Snapshot`] 是游戏主循环每帧填好递进来的只读数据；
//! - 发：[`Command`] 是界面"想让它发生"的事，由主循环去执行。
//!
//! 界面里没有 `&mut World`，也没有 `&mut LifeSim`。这不是洁癖——它让
//! "点了暂停键"这件事可以被单独测试（断言 `Command::TogglePause` 出现在
//! 输出里），而不用先造一个世界再跑一帧。
//!
//! 反过来也成立：主循环不需要知道界面长什么样，它只需要认得那几个命令。

use std::collections::VecDeque;

use vivarium_core::organelle::Organelles;
use vivarium_core::probe::Group;
use vivarium_core::{CellType, FieldStats, MaterialKind, OrganismId, TimeScale, ViewMode};

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 左栏的工具。
///
/// 这一组照着 TPT 的工具栏定：画、擦、填、吸、看。
///
/// ## 为什么橡皮有两把
///
/// 世界里同时住着两套东西：**材质**（石头、沙、水）和**生物**（细胞）。
/// 一个"擦"字说不清擦的是哪个——想清掉一片石头，结果把游过去的鱼一起
/// 抹掉了；想清掉一群藻，结果把海底挖了个洞。
///
/// 两把分开之后每条规则只有一句话：**生物橡皮只动占格表，物质橡皮只动材质**。
/// 而"一格非材质即细胞"这条地基也因此没被破坏——它们各自只碰自己那一半。
///
/// ## 为什么没有矩形和直线
///
/// 它们需要"按下 → 拖动 → 松开"三段式，而**拖拽现在是平移视野**。
/// 两个功能抢同一个手势，结果一定是一方误触。等有了修饰键
/// （Shift + 拖）再回来加，那时它们才有地方放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tool {
    /// 画：把当前材质涂成一片圆。
    #[default]
    Brush,
    /// 生物橡皮：抹掉范围内的**有机体**，材质一个格子都不动。
    EraseLife,
    /// 物质橡皮：把范围内的**材质**清成空，有机体不受影响。
    EraseMatter,
    /// 油漆桶：把连成一片的同种材质换掉。
    Fill,
    /// 取样：点哪儿就把那格的材质设为当前材质。
    Pick,
    /// 检查器：点生物看它的内部。
    Inspect,
    /// 点光源：左键放一盏灯，点在已有的灯上则把它拿走。
    Lamp,
}

impl Tool {
    /// 全部工具，按左栏从上到下的顺序。
    pub const ALL: [Self; 7] = [
        Self::Brush,
        Self::EraseLife,
        Self::EraseMatter,
        Self::Fill,
        Self::Pick,
        Self::Inspect,
        Self::Lamp,
    ];

    /// 界面标签。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Brush => "画笔",
            Self::EraseLife => "擦生物",
            Self::EraseMatter => "擦物质",
            Self::Fill => "填充",
            Self::Pick => "取样",
            Self::Inspect => "检查",
            Self::Lamp => "光源",
        }
    }

    /// 悬停时的一句话说明。
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Brush => "把当前材质涂成一片圆。",
            Self::EraseLife => "抹掉范围内的生物。材质一格不动。",
            Self::EraseMatter => "把范围内的材质清成空位。生物不受影响——它们会失去水，然后掉下去。",
            Self::Fill => "把连成一片的同种材质一次换掉。",
            Self::Pick => "点一格，把它那格的材质设为当前材质。",
            Self::Inspect => "点一个生物，在右边看它的身体、神经网络和行为。",
            Self::Lamp => "左键放一盏点光源；点在已有的灯上就把它拿走。光只被材质挡住。",
        }
    }

    /// 快捷键（单字符）。
    pub const fn hotkey(self) -> char {
        match self {
            Self::Brush => 'B',
            Self::EraseLife => 'X',
            Self::EraseMatter => 'E',
            Self::Fill => 'F',
            Self::Pick => 'I',
            Self::Inspect => 'Q',
            Self::Lamp => 'P',
        }
    }

    /// 这个工具会不会往世界里落笔。
    ///
    /// 检查与取样只读。主循环用它决定"按住左键拖动时要不要连续落笔"——
    /// 检查工具如果也落笔，用户想点开一条鱼看看，会顺手把它周围的水涂掉。
    ///
    /// 光源**不算落笔**：拖着鼠标画一道会一口气放下几十盏灯，
    /// 而灯是稀疏的、每盏都要算进光场，"拖一下就放了一排"不是任何人的本意。
    pub const fn paints(self) -> bool {
        matches!(self, Self::Brush | Self::EraseLife | Self::EraseMatter | Self::Fill)
    }
}

// ---------------------------------------------------------------------------
// 控制台
// ---------------------------------------------------------------------------

/// 一条日志的级别。颜色照 TPT 的 `Style.cpp`：
/// 信息是淡紫蓝、警告是黄、错误是橙红。原样搬过来——
/// 这三个颜色在深色底上已经验证了二十年。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// 普通信息。
    Info,
    /// 值得注意但不致命（比如"捕食者灭绝了"）。
    Warn,
    /// 出错。
    Error,
}

/// 控制台里的一行。
#[derive(Debug, Clone)]
pub struct LogLine {
    /// 世界 tick。比墙钟时间有用——出了问题要能回到那一 tick。
    pub tick: u64,
    /// 级别。
    pub level: Level,
    /// 正文。
    pub text: String,
}

/// 滚动日志。
///
/// 用 `VecDeque` 而不是 `Vec`：控制台是**只进不出**的，`Vec` 删头是 O(n)，
/// 而这个程序要跑几万 tick，一行一行删会变成热点。
#[derive(Debug, Default, Clone)]
pub struct Console {
    lines: VecDeque<LogLine>,
    /// 最多留多少行。超出就从头丢。
    ///
    /// 200 行 × 平均 40 字 ≈ 32 KB。留更多没有意义：没有人会往回翻 500 行，
    /// 而无限增长会让长跑进程的内存慢慢爬。
    cap: usize,
}

impl Console {
    /// 新建，默认保留 200 行。
    pub fn new() -> Self {
        Self {
            lines: VecDeque::with_capacity(64),
            cap: 200,
        }
    }

    /// 追加一行。
    pub fn push(&mut self, tick: u64, level: Level, text: impl Into<String>) {
        if self.lines.len() >= self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(LogLine {
            tick,
            level,
            text: text.into(),
        });
    }

    /// 全部行，从旧到新。
    pub fn lines(&self) -> impl DoubleEndedIterator<Item = &LogLine> {
        self.lines.iter()
    }

    /// 行数。
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

// ---------------------------------------------------------------------------
// 快照
// ---------------------------------------------------------------------------

/// 主循环每帧递进来的只读数据。
///
/// 全是值类型或借用，不持有任何模拟对象的句柄——界面不该能"顺着"这些
/// 字段改到世界。需要改世界的地方一律走 [`Command`]。
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// 世界 tick。
    pub tick: u64,
    /// 帧率。
    pub fps: f32,
    /// 单 tick 的实测成本（毫秒）。
    pub ms_per_tick: f32,
    /// 当前速度档。
    pub scale: TimeScale,
    /// 世界尺寸（格）。
    pub world_size: (u32, u32),
    /// 每格几个屏幕像素。
    pub pixels_per_cell: f32,
    /// 活跃分块 / 总分块。材质层成本的真实指标（SPEC §2.4）。
    pub active_chunks: (usize, usize),

    /// 生物数。
    pub organisms: usize,
    /// 细胞总数。
    pub cells: usize,
    /// 生物能量总和。
    pub energy: u64,
    /// 物种数（按体型判同，见 `Organism::body_fingerprint`）。
    pub species: usize,
    /// 杀手细胞数。
    pub killers: usize,
    /// 最小 / 最大体型。
    pub body_range: (usize, usize),
    /// 最大代数。
    pub generation: u32,
    /// 飘在天上的个体数。
    pub stranded: usize,
    /// 含能量的食物格数。
    pub food: usize,
    /// 天上存着多少格水汽（`Climate::vapor`）。
    ///
    /// 水循环是**唯一**能让这个数动的东西：蒸发让它涨、降水让它落。
    /// 它停在一个平衡点上，所以"这个数在抖"就是"循环在跑"。
    pub vapor: u64,

    /// 各材质的格数，下标是 `MaterialKind::as_u8`。
    pub census: [usize; MaterialKind::COUNT],

    /// 光场与温度场的统计。
    pub field: FieldStats,

    /// 环境温度（°C）。世界温度的**目标值**，冷却项往它回归。
    pub ambient_temp: f32,
    /// 当前环境温度下的光合效率（0..1）。
    ///
    /// 由主循环算好递进来，而不是界面自己按温度套一个公式：那个公式
    /// （`photosynthesis`）是模拟的一部分，界面抄一份就一定会漂移。
    pub warmth: f32,

    /// 藻席生长开不开。
    pub food_growth: bool,
    /// 水循环（蒸发 → 水汽 → 降水）开不开。
    pub water_cycle: bool,
    /// 陆地繁殖开不开（子代能不能站在实体上出生）。
    pub land_reproduction: bool,

    /// 材质层的并行工人数上限，以及上一次真正干活时用到了几个。
    pub threads: usize,
    pub workers: usize,

    /// 鼠标下面那一格的读数（分组的探针表）。指针不在世界上就是 `None`。
    ///
    /// 由主循环填：界面拿不到 `&World`，也不该拿。
    pub hover: Option<Hover>,

    /// 种群曲线（归一化到 0..1，最新在最后）。
    pub history: Vec<f32>,

    /// 选中的个体。没选中就是 `None`。
    pub selected: Option<SelectedOrganism>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            tick: 0,
            fps: 0.0,
            ms_per_tick: 0.0,
            scale: TimeScale::Paused,
            world_size: (512, 512),
            pixels_per_cell: 1.0,
            active_chunks: (0, 0),
            organisms: 0,
            cells: 0,
            energy: 0,
            species: 0,
            killers: 0,
            body_range: (0, 0),
            generation: 0,
            stranded: 0,
            vapor: 0,
            food: 0,
            census: [0; MaterialKind::COUNT],
            field: FieldStats::default(),
            ambient_temp: 20.0,
            warmth: 1.0,
            food_growth: true,
            water_cycle: false,
            land_reproduction: false,
            threads: 1,
            workers: 1,
            hover: None,
            history: Vec::new(),
            selected: None,
        }
    }
}

/// 悬停读数的内容：一张分组的探针表。
///
/// 界面**不认识任何一个具体的量**——它只把 `Value` 排版成字符串。
/// 所以"以后加了新信息，悬停里能自动看到"这件事，只需要在
/// `vivarium_core::probe::cell_probes` 里加一行，这里一个字都不用改。
pub type Hover = Vec<Group>;

/// 检查器里显示的那一个个体。
///
/// 刻意是**值**而不是 `&Organism`：界面只读一次就够，借用会把
/// 生命周期扯进 `Ui::draw` 的签名里，而那个签名会被主循环反复调用。
#[derive(Debug, Clone, Default)]
pub struct SelectedOrganism {
    /// 编号。
    pub id: OrganismId,
    /// 锚点（格）。
    pub anchor: (i32, i32),
    /// 能量。
    pub energy: u32,
    /// 能量上限。
    pub max_energy: u32,
    /// 年龄 / 寿数。
    pub age: (u32, u32),
    /// 速度（每 tick 走几格）。
    pub speed: i32,
    /// 嗅觉半径。
    pub smell: i32,
    /// 每 tick 的维持成本。
    pub upkeep: u32,
    /// 每走一格的能量成本。
    pub move_cost: u32,
    /// 身体质量。
    pub mass: u32,
    /// 细胞构成：**每种细胞器几格**（下标同 `palette::ORGANELLE_RGB`）。
    ///
    /// v0.41 从"每种类型几格"换成这个。原因很简单：一旦一格可以是
    /// "叶绿体 + 气囊"，按类型分桶就不再可加——那 13 格里到底有几格是
    /// 生产者？答案取决于你数的是哪一列。按细胞器数则永远可加。
    pub organelles: [usize; 10],
    /// 世系深度。
    pub generation: u32,
    /// 亲代编号。`None` = 投放进来的初代。
    pub parent: Option<OrganismId>,
    /// 身体形状：`(dx, dy, 细胞类型)`，**已经按朝向镜像过**。
    ///
    /// 镜像放在主循环做，不放在界面：界面要是自己按 `facing` 翻一遍，
    /// 就和渲染层各写了一份镜像规则——两份规则迟早会不一致，
    /// 而"缩略图和世界里的鱼左右相反"这种 bug 看半天也想不到。
    pub shape: Vec<(i16, i16, u16)>,
    /// 能量走势（0..1，右端是当前）。
    pub energy_trace: Vec<f32>,
    /// 缩略图上被点中的那一格：`(属于哪个个体, 第几格)`。
    /// 上一次决策的输入（`life::INPUTS` 路）。
    pub inputs: [f32; vivarium_core::life::INPUTS],
    /// 上一次决策：6 路输出。
    pub outputs: [f32; 6],
    /// 形态发生的数据（段树 / 展开层数 / 梯度）。
    pub morph: MorphView,
    /// 行为网络的**直连**权重矩阵（`INPUTS × OUTPUTS`，行优先）。
    ///
    /// 只带直连这一层，不带隐藏层：直连是"看到什么就做什么"那一层，
    /// 手工基因组里真正在干活的就是它。隐藏层有 `INPUTS×8 + 8×6` 个数，
    /// 画出来是一团噪声，而噪声不构成信息。
    pub direct: Vec<f32>,
}

/// 一段（见 `morph::SegmentSummary`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentView {
    /// 嵌套深度。0 = 主干。
    pub depth: u8,
    /// 这一段的格数（不含重复与镜像）。
    pub cells: usize,
    /// 沿主轴重复几次。
    pub repeat: u8,
    /// 是否镜像一份。
    pub mirror: bool,
    /// 这一段的基础细胞器。
    pub base: u16,
}

/// 一条梯度带（见 `morph::Band`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BandView {
    /// 沿主轴的位置区间 `0..1`。
    pub along: (f32, f32),
    /// 到包围盒边界的距离（格）。
    pub from_edge: (f32, f32),
    /// 命中后叠加的细胞器。
    pub set: u16,
    /// 命中后清除的细胞器。
    pub clear: u16,
}

/// **形态发生**：基因组 → 身体的那一次计算。
///
/// 注意它**不是发育**。身体在出生那一刻就长完了，之后只死不长，
/// 所以这里画的是"这一次展开的过程"，不是"一个细胞分裂成一团"。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MorphView {
    /// 段树（先序）。
    pub segments: Vec<SegmentView>,
    /// 每一层段各长出多少格：`layers[d]` = 第 d 层贡献的格数。
    ///
    /// 这是"逐层展开"视图的全部数据——第 0 层 1 格、第 1 层 4 格、
    /// 第 2 层 13 格，一眼就能看出身体是靠"重复"还是靠"子段"长出来的。
    pub layers: Vec<usize>,
    /// 梯度带。
    pub bands: Vec<BandView>,
    /// 体型上限。
    pub max_cells: u8,
    /// 4-邻接连通分量数。`> 1` 就是身体散架了（不该发生）。
    pub components: usize,
    /// 展开的格数**顶到了** `max_cells`。
    ///
    /// 这个数很重要：它说明"突变想长得更大但被上限截住了"，
    /// 而截断是按 BFS 顺序砍的——砍掉的可能是身体的一半。
    pub capped: bool,
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 界面要求主循环做的事。
///
/// 一次 `draw` 可能产生多条（比如"点了暂停，同时把速度设成 1×"）。
/// 主循环按顺序执行，顺序就是它们被推入的顺序。
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// 切换暂停。
    TogglePause,
    /// 单步一个 tick（暂停时才有意义）。
    Step,
    /// 设定速度档。
    SetScale(TimeScale),
    /// 重新生成地貌（同一个种子，用来验证增量上传）。
    Reseed,
    /// 镜头回正。
    CenterView,
    /// 缩放。
    Zoom(f32),
    /// 选中某个个体（`None` = 取消选中）。
    Select(Option<OrganismId>),
    /// 切换左栏工具。
    SetTool(Tool),
    /// 设定笔刷半径（格）。
    SetBrush(u32),
    /// 设定要画的材质。
    SetMaterial(MaterialKind),
    /// 设定要投放的细胞类型。
    SetCell(CellType),
    /// 切换显示滤镜（材质 / 温度 / 光照 / 物种）。
    SetViewMode(ViewMode),
    /// 让世界跑一批 tick 再停下（"快进 N 步"）。
    RunFor(u32),
    /// 把当前画笔存成一个**自定义细胞**（进左侧的细胞库）。
    SaveCell {
        /// 名字（同时是 `cells.dat` 里的键）。
        name: String,
        /// 细胞器位掩码。
        bits: u16,
    },
    /// 从细胞库里删掉一个。
    DeleteCell(String),
    /// 把工坊里画好的身体放到世界的鼠标位置。
    ///
    /// 带的是**相对锚点**的细胞表，主循环负责挑落点和构造有机体——
    /// 界面不知道世界长什么样，也不该知道。
    PlaceBody(Vec<(i16, i16, u16)>),
    /// 把一副身体存进生物库（落盘）。
    SaveBody {
        name: String,
        cells: Vec<(i16, i16, u16)>,
    },
    /// 从生物库删掉一条。
    DeleteBody(String),
    /// 新建一个世界。
    ///
    /// `seed` 为 `None` 表示"随便挑一个"——界面不认识随机数，
    /// 挑种子是主循环的事（它有 `Rng`，也知道当前种子）。
    NewWorld {
        width: u32,
        height: u32,
        seed: Option<u64>,
        /// 要不要生成地形（海洋 + 岛屿）。不生成就是一张空世界。
        terrain: bool,
        /// 要不要随机投放初始生命。
        life: bool,
    },
    /// 把当前世界存到 `saves/<name>.vivarium`。
    SaveWorld(String),
    /// 读一个存档。路径由界面原样带回——它自己不解析路径。
    LoadWorld(String),
    /// 重新扫一遍存档目录。
    ///
    /// 打开"世界"窗口时发一次，存完档再发一次。刻意不做自动刷新：
    /// 每帧扫目录在 2000² 的档上是几十毫秒，而这个列表只在用户看的时候才有意义。
    RefreshSaves,

    // --- 全局参数 ---------------------------------------------------------
    /// 改环境温度（°C）。夹的范围由 core 决定，界面只负责递一个数。
    ///
    /// 拖滑块时**每帧都会发一条**：温度是"目标值"，连续变化正是想要的效果。
    SetAmbientTemp(f32),
    /// 把整张温度场立刻拉回环境温度。
    ///
    /// 单独一条命令而不是 `SetAmbientTemp` 的副作用：调温度不该顺手清掉
    /// 一团还在烧的火，而"清掉"本身又是需要的（暂停时改温度才看得见）。
    ResetTemperature,
    /// 藻席生长开关。
    SetFoodGrowth(bool),
    /// 水循环开关。
    SetWaterCycle(bool),
    /// 陆地繁殖开关。
    SetLandReproduction(bool),
    /// 材质层并行工人数上限。
    SetThreads(usize),

    /// 退出程序。
    ///
    /// 只由**退出确认框**发出。Esc 不发它——Esc 只负责把那个框打开，
    /// 因为"取消放置身体"和"退出程序"共用同一个键，误触代价完全不同。
    Quit,
}

// ---------------------------------------------------------------------------
// 生物工坊
// ---------------------------------------------------------------------------

/// 工坊的画布边长（格）。
///
/// 16 是**画布**尺寸，不是体型上限。最大的手工体型是 14 格，画布比它大两格
/// 是为了留边距、让人看得见自己在画什么。
///
/// **体型上限在 [`vivarium_core::grn::MAX_BODY_CELLS`]（24），不在这里。**
/// 早先这段注释写着"超过 16 格的身体在水里根本推不动"——那是把一条
/// 通行性判据（`passable` 里的 `body <= 16`）当成了体型上限。
/// v0.26 拆掉了那道墙：水对任何体型都是水，大身体只是"更累"，不是"进不去"。
pub const DRAFT_SIZE: usize = 16;

/// 工坊里正在画的那副身体。
///
/// 用 `Vec<Option<CellType>>` 而不是 `Vec<(i16,i16,CellType)>`：画布是**稠密**的，
/// 按坐标索引比查表快，而且"这一格有没有细胞"是最常问的问题。
#[derive(Debug, Clone)]
pub struct Workshop {
    /// 是否打开。打开时它盖住整个窗口（模态）。
    pub open: bool,
    /// 当前画笔的**细胞器组合**。
    ///
    /// 从"细胞类型"换成"细胞器位掩码"是 v0.36 的核心：`CellType` 是封闭枚举，
    /// 而真正的功能由 `organelles` 决定（v0.26 起）。所以工坊现在画的是
    /// "这一格带哪些细胞器"——"叶绿体 + 气囊"这种组合第一次能被玩家画出来。
    pub brush: Organelles,
    /// 画布：`DRAFT_SIZE × DRAFT_SIZE`，行优先。
    pub cells: Vec<Option<Organelles>>,
    /// 名字（保存时用）。
    pub name: String,
    /// 上一次操作的提示（"已保存"/"没有嘴"），几秒后消失。
    pub hint: Option<(String, f64)>,
}

impl Default for Workshop {
    fn default() -> Self {
        Self {
            open: false,
            brush: Organelles::from_cell_type(CellType::Producer),
            cells: vec![None; DRAFT_SIZE * DRAFT_SIZE],
            name: String::new(),
            hint: None,
        }
    }
}

impl Workshop {
    /// 取一格。
    #[inline]
    pub fn get(&self, x: usize, y: usize) -> Option<Organelles> {
        self.cells.get(y * DRAFT_SIZE + x).copied().flatten()
    }

    /// 画一格。
    #[inline]
    pub fn set(&mut self, x: usize, y: usize, v: Option<Organelles>) {
        if let Some(slot) = self.cells.get_mut(y * DRAFT_SIZE + x) {
            *slot = v;
        }
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.cells.iter_mut().for_each(|c| *c = None);
    }

    /// 有几格细胞。
    pub fn count(&self) -> usize {
        self.cells.iter().filter(|c| c.is_some()).count()
    }

    /// 把画布转成"相对锚点"的细胞表，锚点是**包围盒中心**。
    ///
    /// 用中心而不是左上角：画布是 16×16 的方框，人画的时候凭直觉放在中间，
    /// 而锚点在身体正中意味着"放进世界时鼠标指的就是它身体的中心"——
    /// 比"左上角对齐鼠标"符合直觉得多。
    ///
    /// 返回空表示画布上什么都没有。
    pub fn to_cells(&self) -> Vec<(i16, i16, u16)> {
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (usize::MAX, 0usize, usize::MAX, 0usize);
        for y in 0..DRAFT_SIZE {
            for x in 0..DRAFT_SIZE {
                if self.get(x, y).is_some() {
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
        }
        if min_x == usize::MAX {
            return Vec::new();
        }
        // 锚点放在包围盒中心（向下取整）。
        let cx = (min_x + max_x) / 2;
        let cy = (min_y + max_y) / 2;
        let mut out = Vec::with_capacity(self.count());
        for y in 0..DRAFT_SIZE {
            for x in 0..DRAFT_SIZE {
                if let Some(o) = self.get(x, y) {
                    out.push((x as i16 - cx as i16, y as i16 - cy as i16, o.0));
                }
            }
        }
        out
    }

    /// 从一副身体载入画布（锚点对齐到画布中心）。
    pub fn load(&mut self, cells: &[(i16, i16, u16)]) {
        self.clear();
        if cells.is_empty() {
            return;
        }
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (i16::MAX, i16::MIN, i16::MAX, i16::MIN);
        for &(x, y, _) in cells {
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
        }
        let half = DRAFT_SIZE as i16 / 2;
        let ox = (min_x + max_x) / 2 - half;
        let oy = (min_y + max_y) / 2 - half;
        for &(x, y, bits) in cells {
            let cx = (x - ox) as usize;
            let cy = (y - oy) as usize;
            if cx < DRAFT_SIZE && cy < DRAFT_SIZE {
                self.set(cx, cy, Some(Organelles(bits)));
            }
        }
    }
}

/// 细胞库里的一条：一个**自定义细胞**（细胞器组合）。
///
/// 为什么它和 [`LibraryEntry`] 是两个类型而不是一个带 `Option` 的类型：
/// 一个是"一格怎么造"，一个是"一副身体长什么样"。混在一起之后，
/// 每个用到的地方都要问"这条到底是哪一种"，而那个分支永远只有一个答案。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CellEntry {
    /// 名字（同时是 `cells.dat` 里的键）。
    pub name: String,
    /// 细胞器位掩码。
    pub bits: u16,
}

/// 生物库里的一条。
#[derive(Debug, Clone, Default)]
pub struct LibraryEntry {
    /// 名字（同时是文件名）。
    pub name: String,
    /// 细胞表，坐标相对锚点。第三个数是**细胞器位掩码**（`u16`）。
    ///
    /// v0.36 之前这里是 `CellType::as_u8`。改成位掩码之后，
    /// 生物库里的身体可以带"叶绿体 + 气囊"这种组合——而那正是
    /// 工坊里"新建一种细胞"的载体。
    pub cells: Vec<(i16, i16, u16)>,
}

// ---------------------------------------------------------------------------
// 世界窗口
// ---------------------------------------------------------------------------

/// 新建世界时宽度的下限与上限（格）。
///
/// 20 是"还能看出是个世界"的最小尺寸：再小连一列水柱都铺不下，
/// 地形生成器会退化成一片石头。上限比 [`vivarium_core::world::MAX_WORLD_SIDE`]
/// （2000）小得多，因为那个数是**内存与纹理的硬边界**，不是"好玩"的边界——
/// 2000² 的世界在 1100px 的窗口里是 0.55 px/格，什么都看不见。
pub const MIN_SIDE: u32 = 20;
/// 宽度上限。见 [`MIN_SIDE`]。
///
/// 768 = **12 个 64 宽的块**。早先是 600（9.375 块），而那个数字没有依据
/// ——它只是"比 512 大一点"。
pub const MAX_WIDTH: u32 = 768;
/// 高度上限。
///
/// **768，和宽度一样 = 12 个 64 高的块。**
///
/// 早先这里是 **450**，理由是"世界是横板侧视，高度是水深，太深就是一条窄缝"。
/// 但那条理由从来没跟实际数字对过：**默认世界就是 512×512**，
/// 也就是默认值本身越界。于是这个上限什么也没守住，只在两个地方留下副作用：
///
/// 1. 滑块拖到 450 以上会被**悄悄夹回去**，而玩家以为自己设成功了；
/// 2. 预设"大 768×728"从另一条路径绕过了 clamp，于是**点一下闪退**
///    （渲染层读到世界外面）。
///
/// 一个上限如果连默认值都容不下，它就不是上限，是笔误。
pub const MAX_HEIGHT: u32 = 768;

/// 三个预设。
///
/// ## 每一个都必须在 [`MIN_SIDE`]..=[`MAX_WIDTH`] × [`MAX_HEIGHT`] 之内
///
/// 这条不是洁癖：早先"大"是 **768×728**，而 `MAX_HEIGHT` 是 450，
/// 点下去直接绕过滑块把 728 写进 `state.world.height`——于是
/// **点一下就闪退**（渲染层读到世界外面，`index out of bounds`）。
/// 预设走的是另一条赋值路径，它不经过滑块的 `clamp`，
/// 所以"合法尺寸"这件事必须在两处都成立，而不是只在一处。
///
/// 现在由 `presets_are_within_the_limits` 钉住。
pub const PRESETS: [(&str, u32, u32); 3] = [
    ("小 256×256", 256, 256),
    ("标准 512×512", 512, 512),
    ("大 768×728", 768, 728),
];

/// 二次确认的有效秒数。
///
/// 3 秒是"够我读完那行红字"的长度。更长会让"点两次"变成默认行为，
/// 更短则会让人手忙脚乱——而这个确认存在的意义是**让人停一下**，
/// 不是考手速。
pub const CONFIRM_SECONDS: f64 = 3.0;

/// 一个会丢弃当前世界的动作。
///
/// 只有这两种：新建和读取。保存不丢东西，所以不在这张表里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldAction {
    /// 新建。
    New,
    /// 读取某个存档（带路径）。
    Load(String),
}

/// 存档列表里的一条。
///
/// 全部字段由主循环填好——**包括格式化过的字符串**。界面不碰文件系统、
/// 不碰 `SystemTime`，所以这一整块能在没有磁盘的机器上测。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SaveEntry {
    /// 显示名（文件名去掉扩展名）。
    pub name: String,
    /// 完整路径。界面原样带回给主循环。
    pub path: String,
    /// 世界宽（格）。
    pub width: u32,
    /// 世界高（格）。
    pub height: u32,
    /// 存档那一刻的 tick。
    pub tick: u64,
    /// 文件大小，已格式化（"1.0 MB"）。
    pub size: String,
    /// 修改时间，已格式化（"10-22 22:41"）。
    pub modified: String,
}

/// 「世界」窗口的全部界面状态。
#[derive(Debug, Clone)]
pub struct WorldPanel {
    /// 是否打开。打开时它是模态的——和生物工坊一样盖住整个世界。
    pub open: bool,

    // --- 新建 ---
    /// 宽（格）。
    pub width: u32,
    /// 高（格）。
    pub height: u32,
    /// 种子输入框。**留空 = 随机**。
    pub seed: String,
    /// 是否生成地形。
    pub terrain: bool,
    /// 是否随机投放初始生命。
    pub life: bool,

    // --- 保存 ---
    /// 文件名（不带扩展名）。
    pub save_name: String,
    /// 存档目录的显示文本。主循环填，用来回答"文件到底存哪儿了"。
    pub saves_dir: String,

    // --- 读取 ---
    /// 存档列表。主循环填。
    pub saves: Vec<SaveEntry>,

    /// 正在等待二次确认的动作，以及它的截止时刻（`egui` 的时间轴）。
    ///
    /// 存的是**截止时刻**而不是"还剩几秒"：每帧减一次秒数会在掉帧时
    /// 变慢或变快，而截止时刻与帧率无关。
    pub confirm: Option<(WorldAction, f64)>,
    /// 上一次操作的结果，显示在窗口底部。
    pub hint: Option<String>,
}

impl Default for WorldPanel {
    fn default() -> Self {
        Self {
            open: false,
            // 默认就是"标准世界"：第一次打开窗口的人多半只是想看看，
            // 而 512×512 是这个项目的默认世界。
            width: 512,
            height: 512,
            seed: String::new(),
            terrain: true,
            life: true,
            // 文件名由主循环在启动时填（它才知道今天是几号）。
            save_name: String::new(),
            saves_dir: String::new(),
            saves: Vec::new(),
            confirm: None,
            hint: None,
        }
    }
}

impl WorldPanel {
    /// 记下一个"要丢世界"的意图。返回 `true` 表示**这次点击是确认**。
    ///
    /// 三段语义：
    /// - 第一次点：进入待确认，返回 `false`；
    /// - 3 秒内再点**同一个动作**：确认，返回 `true`，状态清空；
    /// - 超时或点了**别的**动作：重新开始计时，返回 `false`。
    ///
    /// 最后一条是刻意的：先点"新建"再点"读取"，不该被当成"确认新建"。
    /// 两个不同的动作共用一个确认位，是这类交互里最容易写错的地方。
    pub fn confirm_click(&mut self, want: WorldAction, now: f64) -> bool {
        match &self.confirm {
            Some((pending, deadline)) if *pending == want && now <= *deadline => {
                self.confirm = None;
                true
            }
            _ => {
                self.confirm = Some((want, now + CONFIRM_SECONDS));
                false
            }
        }
    }

    /// 这个动作现在是不是在等确认（且没超时）。
    pub fn is_confirming(&self, want: &WorldAction, now: f64) -> bool {
        matches!(&self.confirm, Some((p, deadline)) if p == want && now <= *deadline)
    }

    /// 超时的确认要清掉。每帧开头调一次。
    pub fn expire_confirm(&mut self, now: f64) {
        if let Some((_, deadline)) = &self.confirm {
            if now > *deadline {
                self.confirm = None;
            }
        }
    }

    /// 把种子输入框解析成一个数。空串 = `None`（随机）。
    ///
    /// 非空但解析不了时也返回 `None`：一个打错的种子不该让"新建"失败，
    /// 而"留空=随机"已经是这个输入框的语义，多一种退路不会让人困惑。
    pub fn parsed_seed(&self) -> Option<u64> {
        let text = self.seed.trim();
        if text.is_empty() {
            return None;
        }
        text.parse().ok()
    }
}

/// 「全局参数」窗口的状态。
///
/// ## 这里**故意**不存任何一份"当前值"
///
/// 温度、三个开关、工人数全部从 [`Snapshot`] 读，点击只发命令。理由不是
/// 洁癖：界面自己留一份副本，就一定会和世界漂移——读档、新建世界、命令行
/// 参数、甚至另一个面板都能改它们，而副本不会跟着变。漂移的症状是
/// "面板上写着开、世界其实关着"，这种 bug 没有报错，只有玩家觉得怪。
///
/// 所以这个结构体里只有"窗口开没开"和"上一次操作说了什么"。
#[derive(Debug, Clone, Default)]
pub struct ParamsPanel {
    /// 是否打开。打开时它是模态的。
    pub open: bool,
}

// ---------------------------------------------------------------------------
// 界面状态
// ---------------------------------------------------------------------------
/// 界面的全部可变状态。
///
/// 这里只放**界面自己的**东西：选了哪个工具、笔刷多大、控制台滚到哪。
/// 世界的状态一律在 `Snapshot` 里，每帧重新递进来。
#[derive(Debug)]
pub struct UiState {
    /// 当前工具。
    pub tool: Tool,
    /// 笔刷半径（格）。
    pub brush: u32,
    /// 当前材质。
    pub material: MaterialKind,
    /// 当前细胞类型。
    pub cell: CellType,
    /// 当前显示滤镜。
    pub view: ViewMode,
    /// 选中的个体。主循环据此填 `Snapshot::selected`。
    pub selected: Option<OrganismId>,
    /// 退出确认框是不是开着。
    ///
    /// 它住在 `UiState` 而不是主循环里，因为**画它的是界面**，而主循环
    /// 只回答一个布尔问题："Esc 现在该干什么"。
    pub confirm_quit: bool,
    /// 检视面板里**被点中的那一格**：`(属于哪个个体, 第几格)`。
    ///
    /// 带上编号是为了"换了选中对象就自动失效"——不需要任何事件通知，
    /// 画的时候比一下编号即可。这种"把失效条件写进数据里"的做法，
    /// 比在每个可能改变选中的地方手动清一遍可靠得多。
    pub picked_cell: Option<(OrganismId, usize)>,
    /// 面板可见性。
    pub show_inspector: bool,
    pub show_console: bool,
    /// 控制台。
    pub console: Console,
    /// 一帧里攒下的命令。`draw` 结束时由调用方取走。
    pub commands: Vec<Command>,
    /// 生物工坊。
    pub workshop: Workshop,
    /// 生物库（名字 + 细胞表）。主循环每次载入/保存后回填。
    pub library: Vec<LibraryEntry>,
    /// **自定义细胞库**：工坊左侧那一列可以直接取用的细胞。
    ///
    /// 和 `library`（整具身体）分开：一个是"零件"，一个是"成品"。
    pub cells: Vec<CellEntry>,
    /// 「世界」窗口（新建 / 保存 / 读取）。
    pub world: WorldPanel,
    /// 「全局参数」窗口（世界温度 / 生态开关 / 性能）。
    pub params: ParamsPanel,
    /// 顶部那条"上一次操作"的提示，两秒后自己消失。
    ///
    /// 存在的理由：很多操作**没有可见反馈**（设速度、切工具），
    /// 而人需要一个"我点到了"的确认。做完就淡出，不占地方。
    pub toast: Option<(String, f64)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            tool: Tool::Brush,
            brush: 2,
            material: MaterialKind::Water,
            cell: CellType::Producer,
            view: ViewMode::Material,
            selected: None,
            confirm_quit: false,
            picked_cell: None,
            show_inspector: true,
            show_console: true,
            console: Console::new(),
            workshop: Workshop::default(),
            library: Vec::new(),
            cells: Vec::new(),
            world: WorldPanel::default(),
            params: ParamsPanel::default(),
            commands: Vec::new(),
            toast: None,
        }
    }
}

impl UiState {
    /// 新建一份状态，控制台里先写两行"怎么用"。
    pub fn new() -> Self {
        let mut s = Self::default();
        s.console.push(0, Level::Info, "Vivarium 已启动");
        s.console.push(
            0,
            Level::Info,
            "空格 暂停 · 1/2/3/4 速度 · W 工坊 · G 世界 · K 参数 · Esc 退出",
        );
        s
    }

    /// 记下一条命令。
    pub fn emit(&mut self, cmd: Command) {
        // 同一个速度档连点两次没有意义，而且会让"点到了吗"更难判断。
        if let Command::SetScale(scale) = cmd {
            if self
                .commands
                .iter()
                .any(|c| matches!(c, Command::SetScale(s) if *s == scale))
            {
                return;
            }
        }
        self.commands.push(cmd);
    }

    /// 顶部提示。
    pub fn toast(&mut self, text: impl Into<String>, now: f64) {
        self.toast = Some((text.into(), now));
    }

    /// 打开/关闭「全局参数」。
    ///
    /// 收在这里而不是在按钮和快捷键里各写一遍：**模态互斥**是三条规则
    /// （开它要关掉另外两个），写两遍就一定会有一遍忘记改。
    pub fn toggle_params(&mut self) {
        self.params.open = !self.params.open;
        if self.params.open {
            self.world.open = false;
            self.workshop.open = false;
        }
    }

    /// 取走这一帧攒下的命令。
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }

    /// 往控制台写一行。主循环在发生事件时调（出生潮、灭绝、重铺地形）。
    pub fn log(&mut self, tick: u64, level: Level, text: impl Into<String>) {
        self.console.push(tick, level, text);
    }
}
