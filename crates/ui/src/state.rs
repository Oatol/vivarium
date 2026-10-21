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

use vivarium_core::{CellType, MaterialKind, OrganismId, TimeScale};

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

/// 左栏的工具。
///
/// 这一组照着 TPT 的工具栏定：画、擦、填、吸、看。
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
    /// 擦：把格子清成空。
    Eraser,
    /// 油漆桶：把连成一片的同种材质换掉。
    Fill,
    /// 取样：点哪儿就把那格的材质设为当前材质。
    Pick,
    /// 检查器：点生物看它的内部。
    Inspect,
}

impl Tool {
    /// 全部工具，按左栏从上到下的顺序。
    pub const ALL: [Self; 5] = [
        Self::Brush,
        Self::Eraser,
        Self::Fill,
        Self::Pick,
        Self::Inspect,
    ];

    /// 界面标签。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Brush => "画笔",
            Self::Eraser => "橡皮",
            Self::Fill => "填充",
            Self::Pick => "取样",
            Self::Inspect => "检查",
        }
    }

    /// 快捷键（单字符）。
    pub const fn hotkey(self) -> char {
        match self {
            Self::Brush => 'B',
            Self::Eraser => 'E',
            Self::Fill => 'F',
            Self::Pick => 'I',
            Self::Inspect => 'Q',
        }
    }

    /// 这个工具会不会往世界里落笔。
    ///
    /// 检查与取样只读。主循环用它决定"按住左键拖动时要不要连续落笔"——
    /// 检查工具如果也落笔，用户想点开一条鱼看看，会顺手把它周围的水涂掉。
    pub const fn paints(self) -> bool {
        matches!(self, Self::Brush | Self::Eraser | Self::Fill)
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

    /// 各材质的格数，下标是 `MaterialKind::as_u8`。
    pub census: [usize; MaterialKind::COUNT],

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
            killers: 0,
            body_range: (0, 0),
            generation: 0,
            stranded: 0,
            food: 0,
            census: [0; MaterialKind::COUNT],
            history: Vec::new(),
            selected: None,
        }
    }
}

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
    /// 细胞构成：每种类型几格。
    pub cells: [usize; CellType::COUNT],
    /// 世系深度。
    pub generation: u32,
    /// 亲代编号。`None` = 投放进来的初代。
    pub parent: Option<OrganismId>,
    /// 身体形状：`(dx, dy, 细胞类型)`，**已经按朝向镜像过**。
    ///
    /// 镜像放在主循环做，不放在界面：界面要是自己按 `facing` 翻一遍，
    /// 就和渲染层各写了一份镜像规则——两份规则迟早会不一致，
    /// 而"缩略图和世界里的鱼左右相反"这种 bug 看半天也想不到。
    pub shape: Vec<(i16, i16, u8)>,
    /// 能量走势（0..1，右端是当前）。
    pub energy_trace: Vec<f32>,
    /// 上一次决策：17 路输入。
    pub inputs: [f32; 17],
    /// 上一次决策：6 路输出。
    pub outputs: [f32; 6],
    /// 行为网络的**直连**权重矩阵（`17 × 6`，行优先）。
    ///
    /// 只带直连这一层，不带隐藏层：直连是"看到什么就做什么"那一层，
    /// 手工基因组里真正在干活的就是它。隐藏层有 17×8 + 8×6 个数，
    /// 画出来是一团噪声，而噪声不构成信息。
    pub direct: Vec<f32>,
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
    /// 在水面附近投放 N 条。
    Drop(usize),
    /// 在最高的岛顶点点一把火。
    Ignite,
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
    /// 让世界跑一批 tick 再停下（"快进 N 步"）。
    RunFor(u32),
    /// 退出。
    Quit,
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
    /// 选中的个体。主循环据此填 `Snapshot::selected`。
    pub selected: Option<OrganismId>,
    /// 面板可见性。
    pub show_inspector: bool,
    pub show_console: bool,
    /// 控制台。
    pub console: Console,
    /// 一帧里攒下的命令。`draw` 结束时由调用方取走。
    pub commands: Vec<Command>,
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
            selected: None,
            show_inspector: true,
            show_console: true,
            console: Console::new(),
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
            "空格 暂停 · 1/2/3/4 速度 · L 放鱼 · Esc 退出",
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

    /// 取走这一帧攒下的命令。
    pub fn take_commands(&mut self) -> Vec<Command> {
        std::mem::take(&mut self.commands)
    }

    /// 往控制台写一行。主循环在发生事件时调（出生潮、灭绝、重铺地形）。
    pub fn log(&mut self, tick: u64, level: Level, text: impl Into<String>) {
        self.console.push(tick, level, text);
    }
}
