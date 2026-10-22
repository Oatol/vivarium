//! Vivarium 图形前端。
//!
//! ## 这个 bin 负责什么
//!
//! 只负责**窗口与事件循环**：开窗、收输入、按帧调用渲染器。
//! 世界怎么演化是 `vivarium-core` 的事，像素怎么画是 `vivarium-render` 的事。
//! 这个分工不是洁癖——它让"能不能开窗"和"画得对不对"可以分别测试，
//! 也让 `vivarium-sim` 能在没有显示器的机器上把一万代跑完。
//!
//! ## 降级路径
//!
//! 加 `--ascii` 就在终端里跑同一套模拟内核。这不是开发期的临时脚手架：
//! 在一台没有 GPU、没有显示器的机器上（比如 CI）复现一个 bug 时，
//! 它是唯一能用的观察手段。
//!
//! ## 操作
//!
//! | 键 / 鼠标 | 作用 |
//! |---|---|
//! | 空格 | 暂停 / 继续 |
//! | 1 / 2 / 3 / 4 | 速度 1× / 5× / 50× / 最大 |
//! | 左键 | 落笔（工具决定画 / 擦 / 填 / 吸 / 看） |
//! | 右键 / 中键拖拽 | 平移视野 |
//! | 滚轮 | 以指针为中心缩放 |
//! | B / X / E / F / I / Q / P | 画笔 / 擦生物 / 擦物质 / 填充 / 取样 / 检查 / 光源 |
//! | W | 生物工坊 |
//! | G | 世界（新建 / 保存 / 读取） |
//! | V | 换显示滤镜 |
//! | R | 重新铺一遍地形（用来验证增量上传确实在工作） |
//! | Esc | 退出 |
//!
//! ## 盯什么
//!
//! 标题栏里的**活跃块**是本项目最该盯的一个数字。SPEC 第 2.4 节说材质层的成本
//! 取决于活跃格子比例，而不是网格尺寸——世界塌稳之后它应当掉到 0。
//! 如果它长期停在满格，说明有东西在永远抖动，跳块优化等于没写。

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vivarium_app::{FrameBudget, TimeScale};
use vivarium_core::life::{Cell, CellType, Organism, BIRTH_ENERGY_PER_CELL};
use vivarium_core::life::{Genome, INPUTS, OUTPUTS};
use vivarium_core::life_sim::{handcrafted_fish_genome, stock_surface, LifeSim, Tactic, MAX_AGE};
use vivarium_core::{save, worldgen, MaterialKind, MaterialSim, Rng, World, WorldLayout};
use vivarium_ui::state::{SelectedOrganism, Snapshot};
mod clock;
mod ui_glue;

use vivarium_render::ascii::{AsciiRenderer, Camera};
use vivarium_render::pixel::PixelRenderer;
use vivarium_render::pixel::Viewport;
use vivarium_render::Renderer;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

// ---------------------------------------------------------------------------
// 命令行参数
// ---------------------------------------------------------------------------

/// 默认世界尺寸。SPEC 第 1 节的开发期默认值。
const DEFAULT_SIZE: usize = 512;
const DEFAULT_SEED: u64 = 2026_1016;

#[derive(Debug, Clone, Copy)]
struct Options {
    ascii: bool,
    width: usize,
    height: usize,
    seed: u64,
    /// ASCII 模式下跑多少 tick 再打印。
    ticks: u64,
    /// 启动时在水面附近投放多少条鱼。0 表示不放。
    fish: usize,
    morph: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ascii: false,
            width: DEFAULT_SIZE,
            height: DEFAULT_SIZE,
            seed: DEFAULT_SEED,
            ticks: 60,
            fish: 0,
            morph: false,
        }
    }
}

fn print_help() {
    println!(
        "Vivarium 图形前端

用法：
  vivarium-game [选项]

选项：
  --ascii            不开窗口，在终端里渲染（没有 GPU 时的降级路径）
  --width <N>        世界宽度，必须是 64 的倍数（默认 {DEFAULT_SIZE}）
  --height <N>       世界高度，必须是 64 的倍数（默认 {DEFAULT_SIZE}）
  --seed <N>         随机种子（默认 {DEFAULT_SEED}）
  --ticks <N>        仅 --ascii：先跑多少 tick 再打印（默认 60）
  --fish <N>         启动时在水面附近投放 N 条鱼（默认 0，按 L 也可以随时放）
  --morph            身体由基因组**长出来**（形态发生），而不是用手工体型表
  -h, --help         显示这份帮助"
    );
}

fn parse_args() -> Result<Options, String> {
    let mut opts = Options::default();
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ascii" => opts.ascii = true,
            "--width" => opts.width = take_number(&mut args, "--width")? as usize,
            "--height" => opts.height = take_number(&mut args, "--height")? as usize,
            "--seed" => opts.seed = take_number(&mut args, "--seed")?,
            "--ticks" => opts.ticks = take_number(&mut args, "--ticks")?,
            "--fish" => opts.fish = take_number(&mut args, "--fish")? as usize,
            "--morph" => opts.morph = true,
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            other => return Err(format!("无法识别的参数：{other}")),
        }
    }

    Ok(opts)
}

fn take_number(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    let raw = args
        .next()
        .ok_or_else(|| format!("{flag} 后面缺一个数字"))?;
    raw.parse::<u64>()
        .map_err(|_| format!("{flag} 的值 `{raw}` 不是数字"))
}

// ---------------------------------------------------------------------------
// 入口
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("参数错误：{e}");
            eprintln!("用 `vivarium-game --help` 看看怎么用。");
            return ExitCode::FAILURE;
        }
    };

    if opts.ascii {
        return run_ascii(&opts);
    }

    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("创建事件循环失败：{e}");
            eprintln!("如果这台机器没有图形环境，试试 `vivarium-game --ascii`。");
            return ExitCode::FAILURE;
        }
    };

    let mut app = App {
        options: opts,
        game: None,
        exit_code: ExitCode::SUCCESS,
    };

    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("事件循环异常退出：{e}");
        return ExitCode::FAILURE;
    }

    app.exit_code
}

// ---------------------------------------------------------------------------
// 应用状态
// ---------------------------------------------------------------------------

struct App {
    options: Options,
    game: Option<Game>,
    exit_code: ExitCode,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // Resumed 可能连着来好几次（不同平台的挂起/恢复语义不一样），
        // 已经建好了就直接返回。
        if self.game.is_some() {
            return;
        }

        match Game::create(event_loop, &self.options) {
            Ok(game) => {
                println!("{}", game.banner());
                game.window.request_redraw();
                self.game = Some(game);
            }
            Err(e) => {
                eprintln!("无法启动图形前端：{e}");
                eprintln!("试试 `vivarium-game --ascii`，它跑的是同一套模拟内核。");
                self.exit_code = ExitCode::FAILURE;
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(game) = self.game.as_mut() else {
            return;
        };
        if window_id != game.window.id() {
            return;
        }

        // **界面先收事件。** 鼠标在面板上时，世界不该同时收到这次点击——
        // 否则点一次"暂停"会顺手在画布上画一笔。
        //
        // 顺序不能反过来：egui 的命中测试依赖"这一帧之前"的布局，
        // 而布局是上一帧算好的，所以事件必须先经过它。
        let window = game.window.clone();
        let consumed = game.ui.on_window_event(&window, &event);

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                game.renderer.resize(size.width, size.height);
                game.window.request_redraw();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                // 忽略松键与自动重复，否则按住 R 会以每秒 30 次的速度重铺地形。
                // `consumed` 为真说明焦点在界面的输入框里（将来会有），
                // 那时不该再触发世界快捷键。
                if !consumed && event.state == ElementState::Pressed && !event.repeat {
                    game.on_key(&event.logical_key, event_loop);
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                use winit::event::MouseButton;
                match (state, button) {
                    // 左键：落笔（或者检查 / 取样，取决于工具）。
                    (ElementState::Pressed, MouseButton::Left) => {
                        if !consumed && !game.ui.pointer_over_ui && !game.ui.modal {
                            game.on_press();
                        }
                    }
                    (ElementState::Released, MouseButton::Left) => game.on_release(),
                    // 右键 / 中键：拖拽平移。**不给左键**，因为左键要画画。
                    (ElementState::Pressed, MouseButton::Right | MouseButton::Middle) => {
                        if !consumed && !game.ui.pointer_over_ui && !game.ui.modal {
                            game.pan_anchor = game.cursor;
                        }
                    }
                    (ElementState::Released, MouseButton::Right | MouseButton::Middle) => {
                        game.pan_anchor = None;
                    }
                    _ => {}
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                if consumed {
                    return;
                }
                // 每格滚轮的放大倍率。1.15 是试出来的：再小要滚很久才看得见变化，
                // 再大一次误触就从全景跳到细胞内部，找不到自己在哪。
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // 触控板给的是像素。除以 40 换算成"行"，
                    // 否则一次轻扫就是十几倍，镜头直接飞出去。
                    MouseScrollDelta::PixelDelta(pos) => pos.y as f32 / 40.0,
                };
                if lines != 0.0 {
                    // 以指针为锚点缩放：想放大看的那一格会一直待在指针下面。
                    // 绕视口中心缩的话，放大三次目标就跑到画面外去了。
                    let target = game.renderer.surface_size();
                    match game.cursor {
                        Some(pos) => game.renderer.zoom_at(
                            1.15f32.powf(lines),
                            (pos.0 as f32, pos.1 as f32),
                            target,
                        ),
                        None => game.renderer.zoom_by(1.15f32.powf(lines)),
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                let prev = game.cursor;
                game.cursor = Some((position.x, position.y));
                game.on_cursor_moved(prev);
            }

            WindowEvent::RedrawRequested => game.frame(event_loop),

            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// 游戏循环
// ---------------------------------------------------------------------------

struct Game {
    window: Arc<Window>,
    /// 留一份 Instance 是刻意的：surface 与 device 内部都持有它的引用，
    /// 显式留着能让生命周期关系一眼看清。
    #[allow(dead_code)]
    instance: wgpu::Instance,
    renderer: PixelRenderer,
    world: World,
    /// 材质流动引擎。跨 tick 的状态（已移动掩码、活跃分块快照）都在它里面，
    /// 世界的全部内容仍然在 `world` 里。
    sim: MaterialSim,
    /// 生命层。**独立于材质层**，单线程顺序执行（SPEC §2.4）：
    /// 生物的身体横跨好几个分块，塞进材质线程池就得给占格表加锁。
    life: LifeSim,
    /// 初始地貌的两条基准线。横幅要用，将来相机取景也要用。
    layout: WorldLayout,
    rng: Rng,
    seed: u64,
    /// 投放初始生命时用形态发生还是手工体型表。跟着命令行开关走，中途不变。
    ///
    /// 现在只有「世界」窗口新建世界时用它（`L` 放鱼和 `G` 点火已经删掉了）。
    morph: bool,

    /// 界面。它的 GPU 资源在 `create` 里建，因为它要拿到渲染器的 device。
    ui: ui_glue::Egui,
    /// 种群曲线的历史（归一化到 0..1）。每 8 帧采一次点。
    ///
    /// 采样频率而不是每帧：曲线只有几百个像素宽，每帧一个点会把
    /// 窗口压缩到"只有最近一秒"，看不出趋势。
    history: Vec<f32>,
    /// 上次采样时的 tick。用来做"每 N tick 采一点"。
    history_tick: u64,
    /// 工坊里"放置"之后等待落点的那副身体。
    ///
    /// 放置是**两段式**的：工坊里点"放置到世界"只记下要放什么，
    /// 然后等用户在世界里点一下决定放哪儿。一段式（直接放在屏幕中心）
    /// 看起来更省事，但"我想把它放在那片浅滩上"是最常见的诉求。
    pending_body: Option<Vec<(i16, i16, u8)>>,
    /// 光标在窗口里的位置（物理像素）。点击时用它换算成世界格。
    cursor: Option<(f64, f64)>,
    /// 左键按着，而且当前工具会落笔。
    painting: bool,
    /// 这一帧改过世界，所以空气场要在画之前重算一遍。
    ///
    /// 场是按 `tick % INTERVAL` 刷的，而**暂停时 tick 不走**。玩家画了一把火、
    /// 把指针放上去，读数是 22 °C——不是温度算错了，是场还停在画之前。
    /// 同理"放了灯却没亮"。所以任何改动世界的操作都置这个标记，
    /// 在渲染前统一刷一次（1.6 ms，只在真的画了东西的那一帧付）。
    field_dirty: bool,
    /// 本次落笔一共抹掉了几个生物。**松开时才写进控制台**——
    /// 拖着一路擦过鱼群会在一次拖动里触发几十次，逐次打印会把控制台刷满。
    erased: usize,
    /// 上一次落笔的格子。用来把"这一帧的鼠标位置"和"上一帧的"连成一条线——
    /// 鼠标快的时候两帧之间能跨十几格，只在当前位置盖一个圆会画出虚线。
    last_paint: Option<(i32, i32)>,
    /// 右键 / 中键拖拽的锚点（上一个光标位置）。`None` = 没在拖。
    pan_anchor: Option<(f64, f64)>,

    scale: TimeScale,
    budget: FrameBudget,
    /// 上一帧测得的单 tick 成本，喂给"最大速度"档位做预算裁剪。
    last_tick_cost: Duration,

    // FPS 统计
    fps_window_start: Instant,
    fps_frames: u32,
    fps: f32,
}

impl Game {
    fn create(event_loop: &ActiveEventLoop, opts: &Options) -> Result<Self, String> {
        let attrs = Window::default_attributes()
            .with_title("Vivarium")
            .with_inner_size(winit::dpi::LogicalSize::new(1100.0, 840.0));

        let window = event_loop
            .create_window(attrs)
            .map_err(|e| format!("创建窗口失败：{e}"))?;
        let window = Arc::new(window);

        let instance = wgpu::Instance::default();
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("创建绘制表面失败：{e}"))?;

        let size = window.inner_size();
        let renderer = PixelRenderer::new(&instance, surface, size.width, size.height)
            .map_err(|e| e.to_string())?;

        let mut world = World::new(opts.width, opts.height).map_err(|e| e.to_string())?;
        let mut rng = Rng::new(opts.seed);
        let layout = worldgen::sea_and_islands(&mut world, &mut rng);
        // 建生命层时会扫一遍地形，把天然碎屑折成"初始能量"记进账。
        let mut life = LifeSim::new(&mut world);
        if opts.fish > 0 {
            let ids = stock_surface(&mut world, &mut life, opts.fish, 300, opts.morph);
            println!("启动时投放 {} 条鱼", ids.len());
        }

        let mut ui = ui_glue::Egui::new(renderer.device(), renderer.surface_format(), &window);
        // 生物库在启动时载入一次。之后每次保存 / 删除都重新扫一遍目录——
        // 目录里只有几个小文本文件，扫一遍比维护增量状态简单得多，
        // 而且用户可能在外面手改了文件。
        ui.state.library = load_library();

        let mut game = Self {
            window,
            instance,
            renderer,
            world,
            sim: MaterialSim::new(),
            life,
            layout,
            rng,
            seed: opts.seed,
            morph: opts.morph,
            ui,
            history: Vec::with_capacity(160),
            history_tick: 0,
            pending_body: None,
            cursor: None,
            painting: false,
            field_dirty: false,
            erased: 0,
            last_paint: None,
            pan_anchor: None,
            scale: TimeScale::X1,
            budget: FrameBudget::default(),
            last_tick_cost: Duration::ZERO,
            fps_window_start: Instant::now(),
            fps_frames: 0,
            fps: 0.0,
        };
        // 默认存档名带上今天的日期（**本地**日期，见 `clock`）：
        // 一天存好几个档时，"world" 会一路覆盖成 world(1) world(2)，
        // 而带日期至少能一眼看出这是哪天的世界。
        game.ui.state.world.save_name = format!("world-{}", clock::today().compact());
        game.refresh_saves();
        Ok(game)
    }

    fn banner(&self) -> String {
        let info = self.renderer.adapter_info();
        let (sw, sh) = self.renderer.surface_size();
        // 世界纹理是首帧才建的，所以这里按世界尺寸算，别去问渲染器——
        // 问它只会得到 0，然后横幅上写着"世界纹理 0 KiB"。
        let texture_kib = (self.world.width() * self.world.height() * 4) / 1024;
        format!(
            "Vivarium — 图形前端（横板侧视 · 重力向下）

  适配器    : {}  ({:?})
  交换链    : {:?} @ {sw}×{sh}
  世界      : {}×{} 格，{} 个分块
  地貌      : 海洋 + 岛屿 · 水面线 y={} · 海床线 y={} · 陆地 {} 列
  世界纹理  : {} KiB（脏块增量上传）
  生物      : {} 条 · {} 个细胞 · 能量 {}
  界面      : egui · 中文字体 {}

  空格 暂停/继续 · 1/2/3/4 速度 · R 重铺地形 · Esc 退出

  左键 落笔 · 右键/中键拖拽 平移 · 滚轮 缩放（以指针为中心）· 方向键 平移 · 0 回到全景
  B 画笔 · X 擦生物 · E 擦物质 · F 填充 · I 取样 · Q 检查 · P 光源
  W 生物工坊 · G 世界（新建 / 保存 / 读取）· V 换滤镜",
            info.name,
            info.backend,
            self.renderer.surface_format(),
            self.world.width(),
            self.world.height(),
            self.world.chunk_count(),
            self.layout.waterline,
            self.layout.seabed,
            self.layout.land_columns(),
            texture_kib,
            self.life.organisms().len(),
            self.life
                .organisms()
                .iter()
                .map(|o| o.cell_count())
                .sum::<usize>(),
            self.life.stats().energy,
            // 字体装不上时界面会满屏方块。这句话是唯一能让人立刻知道
            // "为什么界面上全是口口口"的地方。
            match self.ui.font() {
                Some(path) => format!("已装载（{path}）"),
                None => "**未找到**（中文会显示成方块）".to_owned(),
            },
        )
    }

    fn on_key(&mut self, key: &Key, event_loop: &ActiveEventLoop) {
        match key {
            // Esc 有两件事要做，**顺序不能反**。
            //
            // 正在连续放置身体时，Esc 的语义是"停手"——这是用户唯一会
            // 主动按 Esc 的时刻。此时直接退出程序等于"我想停手，结果
            // 把程序关了"，而那是最让人火大的一种交互。
            // 没在放置时，Esc 仍然是退出。
            Key::Named(NamedKey::Escape) => {
                if self.pending_body.take().is_some() {
                    let tick = self.world.tick();
                    self.ui.log(
                        tick,
                        vivarium_ui::Level::Info,
                        "停止放置（Esc）",
                    );
                } else {
                    event_loop.exit();
                }
            }
            Key::Named(NamedKey::Space) => {
                self.scale = if self.scale.is_paused() {
                    TimeScale::X1
                } else {
                    TimeScale::Paused
                };
            }
            Key::Named(NamedKey::ArrowLeft) => self.pan(-1.0, 0.0),
            Key::Named(NamedKey::ArrowRight) => self.pan(1.0, 0.0),
            Key::Named(NamedKey::ArrowUp) => self.pan(0.0, -1.0),
            Key::Named(NamedKey::ArrowDown) => self.pan(0.0, 1.0),
            Key::Character(text) => match text.as_str() {
                "1" => self.scale = TimeScale::X1,
                "2" => self.scale = TimeScale::X5,
                "3" => self.scale = TimeScale::X50,
                "4" => self.scale = TimeScale::Max,
                // 工具：和 `Tool::hotkey` 一一对应。走界面那条命令，
                // 免得"快捷键改了工具、左栏却没高亮"——那种不一致很难注意到。
                "b" | "B" => self.pick_tool(vivarium_ui::state::Tool::Brush),
                "x" | "X" => self.pick_tool(vivarium_ui::state::Tool::EraseLife),
                "e" | "E" => self.pick_tool(vivarium_ui::state::Tool::EraseMatter),
                "f" | "F" => self.pick_tool(vivarium_ui::state::Tool::Fill),
                "i" | "I" => self.pick_tool(vivarium_ui::state::Tool::Pick),
                "q" | "Q" => self.pick_tool(vivarium_ui::state::Tool::Inspect),
                "p" | "P" => self.pick_tool(vivarium_ui::state::Tool::Lamp),
                // W = 生物工坊，G = 世界。两个都是"打开一块面板"，
                // 所以挨在一起；G 在玩家的肌肉记忆里是"grid / 生成"，
                // 而新建世界正好是这件事。
                "w" | "W" => self.ui.state.workshop.open = !self.ui.state.workshop.open,
                "g" | "G" => self.toggle_world_panel(),
                "r" | "R" => self.reseed(),
                // V = 换滤镜。Tab 会被 egui 拿去走焦点，F1..F4 在很多
                // 键盘上是媒体键，V 是这里唯一没被占的顺手的键。
                "v" | "V" => self.cycle_view(),
                // 缩放：`+` 和 `=` 都收，因为大多数键盘上 `+` 要按 shift。
                "+" | "=" => self.renderer.zoom_by(1.5),
                "-" | "_" => self.renderer.zoom_by(1.0 / 1.5),
                "0" => {
                    let size = (self.world.width() as u32, self.world.height() as u32);
                    self.renderer.set_viewport(Viewport::centered_on(size));
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// 换下一个显示滤镜。快捷键和顶栏那排按钮走同一条路。
    fn cycle_view(&mut self) {
        let next = self.ui.state.view.next();
        self.ui.state.view = next;
        self.renderer.set_view_mode(next);
        // 不写控制台：切滤镜是高频动作，一行日志既没用又会把真正重要的
        // 那几行（能量漏账、灭绝）挤出去。按钮上已经写着名字了。
    }

    /// 左键点在世界上。
    ///
    /// 现在只做一件事：**用检查工具选中点到的那个生物**。
    /// 画笔落笔还没接（那是下一步），但"能选中、能在右边看到它的内部"
    /// 已经让这个界面有用了——它把"看"和"改"分开了，
    /// 而"看"是不需要写世界的，风险最低。
    /// 切工具。快捷键和左栏按钮走同一条路。
    fn pick_tool(&mut self, tool: vivarium_ui::state::Tool) {
        self.ui.state.tool = tool;
        self.ui.state.emit(vivarium_ui::Command::SetTool(tool));
        let tick = self.world.tick();
        self.ui.log(
            tick,
            vivarium_ui::Level::Info,
            format!("工具：{}", tool.label()),
        );
    }

    /// 左键按下。
    ///
    /// 分三种：会落笔的工具开始画；检查工具选中生物；取样工具吸一口材质。
    /// 把工坊画好的身体放在光标处。
    ///
    /// 落点合法性交给 `spawn`：它会检查边界和占格，失败就什么都不做。
    /// 界面那边已经把"没有嘴 / 超过 16 格 / 不连通"拦住了，但**能不能放下**
    /// 只有世界知道（可能压在石头上），所以这里仍然要处理失败。
    fn place_body(&mut self, cells: Vec<(i16, i16, u8)>) {
        let tick = self.world.tick();
        let Some((cx, cy)) = self.cell_under_cursor() else {
            return;
        };
        if cells.is_empty() {
            return;
        }
        let body: Vec<Cell> = cells
            .iter()
            .map(|&(dx, dy, kind)| Cell::new(CellType::from_u8(kind), (dx, dy)))
            .collect();
        // 有没有牙决定社交倾向——和 `stock_surface` 里同一条规则。
        let tactic = if body.iter().any(|c| c.kind == CellType::Killer) {
            Tactic::Hunt
        } else {
            Tactic::Ignore
        };
        let mut org = Organism::new(0, (cx, cy), body, handcrafted_fish_genome(tactic));
        // 启动能量按格数算：刚好够它活到能繁殖。给多了等于凭空注能。
        let n = org.cells.len().max(1) as u32;
        org.energy = BIRTH_ENERGY_PER_CELL * n * 6 / 5;
        match self.life.spawn_injected(&mut self.world, org) {
            Ok(id) => {
                self.ui.state.selected = Some(id);
                self.ui.log(
                    tick,
                    vivarium_ui::Level::Info,
                    format!("放下了一副 {n} 格的身体 #{}", id),
                );
            }
            Err(e) => {
                self.ui
                    .log(tick, vivarium_ui::Level::Warn, format!("这里放不下：{e}"));
            }
        }
    }

    fn on_press(&mut self) {
        use vivarium_ui::state::Tool;

        // 工坊的"放置"优先于一切：它是一次性的、明确的动作，
        // 而且工坊关掉之后工具还停在原来的位置——先处理它才不会
        // "点了放置、结果用画笔在海上涂了一笔"。
        //
        // **不 `take`**：放置是**连续**的。用户点"放置到世界"之后，
        // 每点一下世界就再放一个，直到按 Esc 为止。这在布置一片海草床
        // 或者一群同类鱼的时候是唯一说得通的交互——否则每放一条都要
        // 回到工坊点一次"放置"。
        if let Some(cells) = self.pending_body.clone() {
            self.place_body(cells);
            return;
        }

        let tool = self.ui.state.tool;
        match tool {
            Tool::Brush | Tool::EraseLife | Tool::EraseMatter => {
                self.painting = true;
                self.last_paint = None;
                self.stroke_to_cursor();
            }
            Tool::Fill => {
                if let Some((x, y)) = self.cell_under_cursor() {
                    self.flood_fill(x, y);
                }
            }
            Tool::Pick => {
                if let Some((x, y)) = self.cell_under_cursor() {
                    let kind = self.world.get(x as isize, y as isize);
                    self.ui.state.material = kind;
                    let tick = self.world.tick();
                    self.ui.log(
                        tick,
                        vivarium_ui::Level::Info,
                        format!("取样：({x}, {y}) = {}", kind.name()),
                    );
                }
            }
            Tool::Inspect => self.inspect_at_cursor(),
            Tool::Lamp => self.toggle_lamp_at_cursor(),
        }
    }

    /// 光源工具：点在已有的灯上就把它拿走，否则放一盏新的。
    ///
    /// 用"同一个键既放又删"而不是分成两个工具：灯是一个**点**，
    /// 而工具条上每多一个图标，别的图标就小一分。放和删针对的是同一个
    /// 对象、同一个位置，合成一个动作（"切换这里有没有灯"）在语义上
    /// 也比"两个工具各自只管一半"更准确。
    fn toggle_lamp_at_cursor(&mut self) {
        let Some((x, y)) = self.cell_under_cursor() else {
            return;
        };
        let tick = self.world.tick();
        // 搜索半径跟着笔刷走：灯在画面上是一个点，鼠标落点有半格误差，
        // 而放大到 20 px/格 时"差半格"就是 10 个屏幕像素。
        let r = self.ui.state.brush.max(2) as i32;
        let removed = self.life.field_mut().remove_source_near(x, y, r);
        let message = match removed {
            Some(src) => format!("拿走了 ({}, {}) 的光源", src.x, src.y),
            None => {
                if self.life.field_mut().add_source(x, y) {
                    format!("放下光源 ({x}, {y})")
                } else {
                    format!(
                        "光源已达上限 {}，先拿走几盏",
                        vivarium_core::field::SOURCE_CAP
                    )
                }
            }
        };
        // 暂停时时钟不走，靠 `% INTERVAL` 的判据永远不会触发——
        // 不手动刷一遍的话，用户会对着一个"放了灯却没亮"的世界发呆。
        self.life.refresh_field(&self.world);
        self.ui.log(tick, vivarium_ui::Level::Info, message);
    }

    /// 左键松开：结束连续落笔。
    fn on_release(&mut self) {
        self.painting = false;
        self.last_paint = None;
        if self.erased > 0 {
            let (tick, n) = (self.world.tick(), self.erased);
            self.ui
                .log(tick, vivarium_ui::Level::Info, format!("抹掉了 {n} 个生物"));
            self.erased = 0;
        }
    }

    /// 光标移动：拖拽平移，或者连续落笔。
    ///
    /// `prev` 是移动之前的位置。落笔要用它来"补线"——鼠标一帧能跨十几格，
    /// 只在当前位置盖一个圆会画成一串断点。
    fn on_cursor_moved(&mut self, prev: Option<(f64, f64)>) {
        if let Some(anchor) = self.pan_anchor {
            // 拖拽平移：把像素位移换算成格。**取负**——往右拖，
            // 看到的东西应该跟着往右走，也就是相机往左移。
            let (Some(a), Some(b)) = (prev, self.cursor) else {
                self.pan_anchor = self.cursor;
                return;
            };
            let ppc = self
                .renderer
                .pixels_per_cell(self.renderer.surface_size())
                .max(0.01);
            self.renderer
                .pan_cells(-((b.0 - a.0) as f32) / ppc, -((b.1 - a.1) as f32) / ppc);
            self.pan_anchor = self.cursor;
            let _ = anchor;
            return;
        }
        if self.painting {
            self.stroke_to_cursor();
        }
    }

    /// 光标下面是哪一格（整数格坐标）。落在世界外就是 `None`。
    fn cell_under_cursor(&self) -> Option<(i32, i32)> {
        let pos = self.cursor?;
        let size = self.renderer.surface_size();
        let (cx, cy) = self.renderer.cell_at((pos.0 as f32, pos.1 as f32), size)?;
        let (x, y) = (cx.floor() as i32, cy.floor() as i32);
        self.world
            .in_bounds(x as isize, y as isize)
            .then_some((x, y))
    }

    /// 从上一笔连到光标当前位置，一路盖章。
    fn stroke_to_cursor(&mut self) {
        let Some((x, y)) = self.cell_under_cursor() else {
            return;
        };
        let from = self.last_paint.unwrap_or((x, y));
        use vivarium_ui::state::Tool;
        let tool = self.ui.state.tool;

        // 两点之间按格插值。步长取 1 格，保证不会漏格——
        // 笔刷半径大于 1 时其实可以稀疏些，但"可能漏格"是那种
        // 只在快速拖动时才出现的 bug，不值得为省几个循环去换。
        let steps = (x - from.0).abs().max((y - from.1).abs()).max(1);
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let px = from.0 as f32 + (x - from.0) as f32 * t;
            let py = from.1 as f32 + (y - from.1) as f32 * t;
            let (px, py) = (px.round() as i32, py.round() as i32);
            match tool {
                Tool::EraseLife => self.stamp_erase_life(px, py),
                Tool::EraseMatter => {
                    self.stamp(px, py, MaterialKind::Empty);
                }
                _ => self.stamp(px, py, self.ui.state.material),
            }
        }
        self.last_paint = Some((x, y));
    }

    /// 在 `(x, y)` 盖一个圆形的笔刷。
    fn stamp(&mut self, x: i32, y: i32, kind: MaterialKind) {
        let r = self.ui.state.brush as i32;
        stamp_circle(&mut self.world, x, y, r, kind);
        self.field_dirty = true;
    }

    /// 生物橡皮：把圆里的**有机体**整个抹掉。
    ///
    /// 抹掉一个个体是**整体**的，不是"抹掉圆里的那几格细胞"——
    /// 半截身体不是生物，它既不能动也不能繁殖，只会占着格子烂不掉。
    /// 所以判据是"这个个体的身体有没有任何一格落在圆里"。
    ///
    /// 水要还：身体占住的每一格都是从水里借的（`Cell::wet`）。
    /// 不还的话，抹掉一条鱼就等于从世界里挖掉三格水——而玩家会一直抹。
    fn stamp_erase_life(&mut self, x: i32, y: i32) {
        let r = self.ui.state.brush as i32;
        let r2 = r * r;
        let mut victims: Vec<vivarium_core::OrganismId> = Vec::new();
        for org in self.life.organisms() {
            let hit = org.cells.iter().any(|c| {
                let (cx, cy) = org.cell_at(c);
                let (dx, dy) = (cx - x, cy - y);
                dx * dx + dy * dy <= r2
            });
            if hit {
                victims.push(org.id);
            }
        }
        if victims.is_empty() {
            return;
        }
        let removed = self.life.remove_organisms(&mut self.world, &victims);
        self.erased += removed;
        // 身体借走的水还回来了，材质变了——场要重算。
        self.field_dirty |= removed > 0;
    }

    /// 油漆桶：把和 `(x, y)` 连成一片的同种材质换成当前材质。
    fn flood_fill(&mut self, x: i32, y: i32) {
        let kind = match self.ui.state.tool {
            // 油漆桶只有"擦物质"这一种擦除语义：填充一整片生物没有意义，
            // 而"把这片石头换成水"是它最常用的用法。
            vivarium_ui::state::Tool::EraseMatter => MaterialKind::Empty,
            _ => self.ui.state.material,
        };
        let (filled, capped) = flood_fill_region(&mut self.world, x, y, kind, FILL_CAP);
        self.field_dirty |= filled > 0;
        let tick = self.world.tick();
        if capped {
            self.ui.log(
                tick,
                vivarium_ui::Level::Warn,
                format!("填充达到上限 {FILL_CAP} 格，剩下的没涂"),
            );
        }
        self.ui.log(
            tick,
            vivarium_ui::Level::Info,
            format!("填充 {filled} 格 → {}", kind.name()),
        );
    }

    /// 检查工具：点生物看它的内部。
    fn inspect_at_cursor(&mut self) {
        let Some((x, y)) = self.cell_under_cursor() else {
            return;
        };
        let owner = self.world.owner(x as isize, y as isize);
        let picked = if owner.organism != 0 {
            Some(owner.organism)
        } else {
            None
        };
        self.ui.state.selected = picked;
        let tick = self.world.tick();
        match picked {
            Some(id) => self
                .ui
                .log(tick, vivarium_ui::Level::Info, format!("选中 #{id}")),
            None => self.ui.log(
                tick,
                vivarium_ui::Level::Info,
                format!("({x}, {y}) 没有生物"),
            ),
        }
    }

    /// 按视口宽度的比例平移相机。
    ///
    /// 用"视口的一成"而不是固定的格数：放大之后固定格数会显得太慢
    /// （一按只挪动一个细胞的距离），缩到全景时又会一步跨过大半个世界。
    fn pan(&mut self, dx: f32, dy: f32) {
        let size = (self.world.width() as u32, self.world.height() as u32);
        let vp = self.renderer.viewport();
        let (vw, vh) = vp.view_size(size);
        self.renderer.pan_cells(dx * vw * 0.1, dy * vh * 0.1);
    }

    /// 打开 / 关掉「世界」窗口。
    ///
    /// 打开时顺手要一份存档列表：那个目录可能刚被用户在外面改过，
    /// 而"打开面板看到的是上一次的列表"会让人以为存档丢了。
    fn toggle_world_panel(&mut self) {
        self.ui.state.world.open = !self.ui.state.world.open;
        if self.ui.state.world.open {
            self.ui.state.emit(vivarium_ui::Command::RefreshSaves);
        }
    }

    /// 换一个种子重铺地形。
    ///
    /// 这不只是个玩具：它是**验证增量上传是否真的生效**的手段。
    /// 按一下 R，标题栏里的"本帧上传"应当瞬间跳到满格，然后立刻掉回 0。
    /// 如果它一直停在满格，说明影子副本没更新，增量逻辑坏了。
    fn reseed(&mut self) {
        self.seed = self.seed.wrapping_add(1);
        self.rng = Rng::new(self.seed);
        self.world.fill_all(MaterialKind::Empty);
        self.layout = worldgen::sea_and_islands(&mut self.world, &mut self.rng);
        // 掩码里留着上一个世界的 tick 戳。虽然戳不匹配本身是安全的，
        // 但重铺之后世界的"活跃区域"完全变了，重建一份更省心也更好推理。
        self.sim = MaterialSim::new();
        // 生命层也要重建：旧世界里的有机体都站在已经不存在的格子上，
        // 占格表还指着它们——不重建的话，新地形里会凭空多出一堆幽灵细胞。
        self.life = LifeSim::new(&mut self.world);
        // 地形换了，光场也得跟着重算——`LifeSim::new` 已经刷过一次，
        // 但那只覆盖了光；温度场是新初始化的，这里不用再动。
        self.renderer.set_view_mode(self.ui.state.view);
    }

    // -- 世界窗口的三件事：新建 / 保存 / 读取 ---------------------------------

    /// 新建一个世界。
    ///
    /// 界面已经拦住了尺寸范围，这里仍然走 `World::new` 的校验——
    /// **一个入口一道闸**比"上游保证过了"可靠，而且报错能进控制台。
    fn new_world(&mut self, width: u32, height: u32, seed: Option<u64>, terrain: bool, life: bool) {
        let seed = seed.unwrap_or_else(|| {
            // "留空 = 随机"。
            //
            // 用系统时钟取种子，而不是模拟里的 `Rng`：这是**用户的一次操作**，
            // 不是模拟的一步，所以它不受"固定种子"的约束。取完之后种子会
            // 写进控制台，想复现同一个世界照着填就行。
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x5EED_1234)
        });

        let mut world = match World::new(width as usize, height as usize) {
            Ok(w) => w,
            Err(e) => {
                let tick = self.world.tick();
                self.ui
                    .log(tick, vivarium_ui::Level::Error, format!("新建失败：{e}"));
                return;
            }
        };

        self.rng = Rng::new(seed);
        self.seed = seed;
        // 不生成地形时，布局只能从空网格里扫出来——`scan` 会给出
        // "一滴水都没有"的退化解（水面线 = 最高地表），取景仍然成立。
        self.layout = if terrain {
            worldgen::sea_and_islands(&mut world, &mut self.rng)
        } else {
            worldgen::scan(&world)
        };
        let mut next_life = LifeSim::new(&mut world);
        let spawned = if life {
            stock_surface(&mut world, &mut next_life, 8, 300, self.morph).len()
        } else {
            0
        };

        self.world = world;
        self.life = next_life;
        // 掩码、历史曲线、选中、待落点的身体全都属于**上一个世界**。
        // 不清的话，新世界里会有一段别人的种群曲线，和一条指向已死编号的选中。
        self.sim = MaterialSim::new();
        self.history.clear();
        self.history_tick = 0;
        self.ui.state.selected = None;
        self.pending_body = None;
        self.reset_view();

        let tick = self.world.tick();
        self.ui.log(
            tick,
            vivarium_ui::Level::Info,
            format!(
                "新建世界 {width}×{height} · 种子 {seed} · 地形 {} · 初始生命 {spawned} 只",
                if terrain { "有" } else { "无" }
            ),
        );
        self.ui.state.world.hint = Some(format!("已新建：{width}×{height}，种子 {seed}"));
    }

    /// 把当前世界存进 `saves/<name>.vivarium`。
    fn save_world(&mut self, name: &str) {
        let tick = self.world.tick();
        let dir = save::saves_dir();
        let path = dir.join(format!("{}.{}", safe_file_name(name), save::EXTENSION));
        match save::save_to_file(&path, &self.world, &self.life) {
            Ok(()) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let text = format!("已保存：{}（{}）", path.display(), human_size(size));
                self.ui.log(tick, vivarium_ui::Level::Info, text.clone());
                self.ui.state.world.hint = Some(text);
                self.refresh_saves();
            }
            Err(e) => {
                self.ui.log(
                    tick,
                    vivarium_ui::Level::Error,
                    format!("保存失败：{e}"),
                );
                self.ui.state.world.hint = Some(format!("保存失败：{e}"));
            }
        }
    }

    /// 读一个存档。
    ///
    /// 失败时**什么都不改**：`load_from_file` 要么给出一个完整的世界，
    /// 要么给一个错误。半个世界比没有世界更糟——它会让玩家以为存档
    /// 只是"有点问题"，然后接着玩下去。
    fn load_world(&mut self, path: &str) {
        let tick = self.world.tick();
        match save::load_from_file(std::path::Path::new(path)) {
            Ok((world, next_life)) => {
                self.world = world;
                self.life = next_life;
                self.sim = MaterialSim::new();
                // 存档里没有"当初用的生成种子"，只能从材质反推地表形状。
                self.layout = worldgen::scan(&self.world);
                self.history.clear();
                self.history_tick = 0;
                self.ui.state.selected = None;
                self.pending_body = None;
                self.reset_view();

                let t = self.world.tick();
                let text = format!(
                    "已读取：{} · {}×{} · tick {} · {} 只",
                    path,
                    self.world.width(),
                    self.world.height(),
                    t,
                    self.life.organisms().len()
                );
                self.ui.log(t, vivarium_ui::Level::Info, text.clone());
                self.ui.state.world.hint = Some(text);
            }
            Err(e) => {
                self.ui.log(
                    tick,
                    vivarium_ui::Level::Error,
                    format!("读取失败：{e}"),
                );
                self.ui.state.world.hint = Some(format!("读取失败：{e}"));
            }
        }
    }

    /// 重新扫一遍存档目录，把结果填进「世界」窗口。
    fn refresh_saves(&mut self) {
        let dir = save::saves_dir();
        self.ui.state.world.saves_dir = dir.display().to_string();
        self.ui.state.world.saves = save::scan_dir(&dir)
            .into_iter()
            .map(|info| vivarium_ui::SaveEntry {
                name: info.name,
                path: info.path.display().to_string(),
                width: info.header.width as u32,
                height: info.header.height as u32,
                tick: info.header.tick,
                size: human_size(info.bytes),
                // 绝对时刻要时区，而"多久以前"不要——见 `clock` 的模块文档。
                modified: info
                    .modified
                    .map(clock::ago)
                    .unwrap_or_else(|| "?".to_owned()),
            })
            .collect();
    }

    /// 换了一个世界之后：视野回正、光场重算。
    ///
    /// 视野**必须**回正：新世界的大小可能完全不同，留着旧世界的视口
    /// 会让相机停在一片界外——画面上是纯黑，而"黑屏"永远像崩溃。
    fn reset_view(&mut self) {
        let size = (self.world.width() as u32, self.world.height() as u32);
        self.renderer.set_viewport(Viewport::centered_on(size));
        self.life.refresh_field(&self.world);
        self.field_dirty = false;
    }

    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        let tick_start = Instant::now();
        let ticks = self.budget.plan_ticks(self.scale, self.last_tick_cost);
        for _ in 0..ticks {
            // 生命层先跑：鱼挤开的水要在同一个 tick 里流走。
            self.life.step(&mut self.world);
            self.sim.step(&mut self.world);
        }
        if ticks > 0 {
            self.last_tick_cost = tick_start.elapsed() / ticks;
        }

        self.sample_history();

        // --- 界面 ---------------------------------------------------------
        //
        // 顺序是：先跑界面逻辑（拿到命令），执行命令，再一次性渲染。
        // 反过来（先渲染再处理命令）会让"点了暂停"慢一帧才生效——
        // 而人眼对"点下去没反应"极其敏感，一帧也看得出来。
        let snap = self.snapshot();
        let mut frame = self.ui.run_frame(&self.window, &snap);

        // 世界只画在界面留出来的那块中央区域里。
        //
        // 界面给的是**逻辑像素**，交换链要的是**物理像素**，所以乘缩放比。
        // 漏了这一步在高 DPI 屏上会整体偏一半——而"点偏了"看起来像拾取写错了。
        let ppp = self.window.scale_factor() as f32;
        let c = self.ui.central();
        self.renderer.set_content_rect(Some((
            c.left() * ppp,
            c.top() * ppp,
            c.right() * ppp,
            c.bottom() * ppp,
        )));
        // 把命令**取出来**而不是移出去：`frame` 后面还要交给 `paint`，
        // 只移走一个字段会让它变成部分移动，借不出去。
        for cmd in std::mem::take(&mut frame.commands) {
            self.handle_command(cmd, event_loop);
        }

        // --- 画 -----------------------------------------------------------
        //
        // 画笔改过世界就先刷一次场。**不能等 `% INTERVAL`**：暂停时 tick 不走，
        // 玩家画了一把火、把指针放上去，读数是 22 °C——看起来完全像温度算错了。
        if self.field_dirty {
            self.life.refresh_field(&self.world);
            self.field_dirty = false;
        }
        //
        // 场要**先**传上去：着色器在材质 / 温度 / 光照 / 物种四种滤镜里
        // 都可能读它，而上传是按版本号跳过的，没变就什么都不做。
        self.renderer.upload_field(self.life.field());
        self.renderer.set_view_mode(self.ui.state.view);

        // device / queue 先克隆出来：`render_with` 要独占 `renderer`，
        // 而闭包里又要用 device/queue。wgpu 的这两样都是引用计数的句柄，
        // 克隆是廉价的。
        let device = self.renderer.device().clone();
        let queue = self.renderer.queue().clone();
        let size = self.renderer.surface_size();
        let Self {
            renderer,
            ui,
            world,
            ..
        } = self;
        if let Err(e) = renderer.render_with(world, |encoder, target| {
            ui.paint(frame, &device, &queue, encoder, target, size);
        }) {
            eprintln!("渲染失败：{e}");
        }

        self.refresh_title();
        // 一直重画：模拟速度与帧率是解耦的，暂停时也应当保持窗口响应。
        self.window.request_redraw();
    }

    /// 每 8 帧采一个种群点，压进 `history`。
    fn sample_history(&mut self) {
        if self.world.tick() == self.history_tick {
            return;
        }
        self.history_tick = self.world.tick();
        // 归一化的分母：能量上限的粗估。用"当前最大"归一化的话，
        // 曲线会随种群涨落整体缩放，看不出真正的趋势。
        let n = self.life.stats().organisms as f32;
        self.history.push((n / 200.0).clamp(0.0, 1.0));
        if self.history.len() > 200 {
            self.history.remove(0);
        }
    }

    /// 把当前状态打包成界面要的那份只读快照。
    fn snapshot(&self) -> Snapshot {
        let life = self.life.stats();
        let sim = self.sim.stats();
        let size = self.window.inner_size();
        let px = self.renderer.pixels_per_cell((size.width, size.height));

        Snapshot {
            tick: self.world.tick(),
            fps: self.fps,
            ms_per_tick: self.last_tick_cost.as_secs_f64() as f32 * 1000.0,
            scale: self.scale,
            world_size: (self.world.width() as u32, self.world.height() as u32),
            pixels_per_cell: px,
            active_chunks: (sim.active_chunks, self.world.chunk_count()),
            organisms: life.organisms,
            cells: life.cells,
            energy: life.energy,
            species: life.species,
            killers: life.killers,
            body_range: (life.min_cells, life.max_cells),
            generation: life.max_generation,
            stranded: life.stranded,
            food: self.life.food_cells(&self.world),
            census: self.world.census(),
            field: self.life.field().stats(),
            hover: self.hover_info(),
            history: self.history.clone(),
            selected: self.selected_view(),
        }
    }

    /// 光标下面那一格的属性。
    ///
    /// 每帧都算一遍，因为光标每帧都可能动。成本是常数：一次 `cell_at`
    /// 加四次查表，和"扫一遍世界"完全不是一个量级。
    fn hover_info(&self) -> Option<vivarium_ui::state::CellInfo> {
        let (x, y) = self.cell_under_cursor()?;
        let (xs, ys) = (x as isize, y as isize);
        let material = self.world.get(xs, ys);

        // 深度：先找这一列的**水面线**（最上面那一格水），再算 y - 水面线。
        //
        // 不能"往上数到第一个非水格就停"：站在岛上时往上数会先数到石头，
        // 于是"离水面多高"变成了"头顶有几格石头"。按列找水面线对任何形状都成立。
        //
        // 单独算而不是从光照反推：站在岛的阴影里时"深度很浅而光为 0"，
        // 这两个数必须分开显示，否则用户会以为光场算错了。
        let waterline = (0..self.world.height() as i32)
            .find(|yy| self.world.get(xs, *yy as isize) == MaterialKind::Water);
        let depth = match waterline {
            // 水面之上是负数，水面那一格是 0。
            Some(wl) => y - wl,
            // 整列没有水（纯陆地或纯天空），"深度"没有意义。
            None => 0,
        };

        let owner = self.world.owner(xs, ys);
        let organism = if owner.organism == 0 {
            None
        } else {
            self.life.organism(owner.organism).map(|o| {
                let cell = o
                    .cells
                    .iter()
                    .find(|c| o.cell_at(c) == (x, y))
                    .map(|c| c.kind)
                    .unwrap_or(CellType::Producer);
                vivarium_ui::state::HoverOrganism {
                    id: o.id,
                    cells: o.cells.len(),
                    generation: o.generation,
                    energy: (o.energy, o.max_energy()),
                    producers: o.count(CellType::Producer),
                    cell,
                }
            })
        };

        Some(vivarium_ui::state::CellInfo {
            x,
            y,
            material,
            organism,
            light: self.life.field().light_at(x as f32, y as f32),
            temp: self.life.field().temp_at(x as f32, y as f32),
            depth,
        })
    }

    /// 把"界面里选中的那个 id"翻译成一个值快照。
    ///
    /// 生物可能已经死了——那时 `organism()` 返回 `None`，界面显示"已死亡"
    /// 比显示一组冻结的数字诚实。
    fn selected_view(&self) -> Option<SelectedOrganism> {
        let id = self.ui.state.selected?;
        let org = self.life.organism(id)?;
        let mut cells = [0usize; vivarium_core::CellType::COUNT];
        for cell in &org.cells {
            cells[cell.kind.as_u8() as usize] += 1;
        }
        // 形状按朝向镜像之后再交出去。**镜像只有一份实现**（`offset_x`），
        // 界面拿到的就是世界里那个朝向——缩略图不会和画面左右相反。
        let shape = org
            .cells
            .iter()
            .map(|c| (org.offset_x(c.offset) as i16, c.offset.1, c.kind.as_u8()))
            .collect();
        // 直连权重（17×6）。只取这一层，见 `SelectedOrganism::direct`。
        let mut direct = Vec::with_capacity(INPUTS * OUTPUTS);
        for i in 0..INPUTS {
            for o in 0..OUTPUTS {
                direct.push(org.genome.weights[Genome::w_io(i, o)]);
            }
        }
        let max_energy = org.max_energy().max(1) as f32;
        let energy_trace = org
            .energy_trace()
            .map(|e| (e as f32 / max_energy).clamp(0.0, 1.0))
            .collect();

        Some(SelectedOrganism {
            id: org.id,
            anchor: org.anchor,
            energy: org.energy,
            max_energy: org.max_energy(),
            age: (org.age, MAX_AGE),
            speed: org.speed(),
            smell: org.smell_radius(),
            upkeep: org.upkeep(),
            move_cost: org.move_cost(),
            mass: org.mass(),
            cells,
            generation: org.generation,
            parent: org.parent,
            shape,
            energy_trace,
            inputs: org.last_inputs,
            outputs: org.last_out,
            direct,
        })
    }

    /// 执行界面发来的命令。
    fn handle_command(&mut self, cmd: vivarium_ui::Command, event_loop: &ActiveEventLoop) {
        use vivarium_ui::Command as C;
        // 界面自己就能处理的（选中、工具、笔刷）先让它处理掉。
        if self.ui.apply(cmd.clone()) {
            return;
        }
        match cmd {
            C::TogglePause => {
                self.scale = if self.scale.is_paused() {
                    TimeScale::X1
                } else {
                    TimeScale::Paused
                };
            }
            C::Step => {
                // 单步 = 暂停状态下跑一个 tick。不暂停时点它没有意义，
                // 所以顺手把状态设成暂停——"按了单步，画面停住走一步"。
                self.scale = TimeScale::Paused;
                self.life.step(&mut self.world);
                self.sim.step(&mut self.world);
            }
            C::SetScale(scale) => self.scale = scale,
            C::Reseed => {
                self.reseed();
                let tick = self.world.tick();
                self.ui.log(
                    tick,
                    vivarium_ui::Level::Info,
                    format!("重铺地形，种子 {}", self.seed),
                );
            }
            C::NewWorld {
                width,
                height,
                seed,
                terrain,
                life,
            } => self.new_world(width, height, seed, terrain, life),
            C::SaveWorld(name) => self.save_world(&name),
            C::LoadWorld(path) => self.load_world(&path),
            C::RefreshSaves => self.refresh_saves(),
            C::CenterView => {
                let size = (self.world.width() as u32, self.world.height() as u32);
                self.renderer.set_viewport(Viewport::centered_on(size));
            }
            C::Zoom(factor) => self.renderer.zoom_by(factor),
            C::RunFor(_) => {}
            C::PlaceBody(cells) => {
                // 只记下来，等用户在世界里点一下再落。
                self.pending_body = Some(cells);
            }
            C::SaveBody { name, cells } => {
                let tick = self.world.tick();
                match save_body(&name, &cells) {
                    Ok(path) => {
                        self.ui.state.library = load_library();
                        self.ui.log(
                            tick,
                            vivarium_ui::Level::Info,
                            format!("已存进生物库：{}（{}）", name, path.display()),
                        );
                    }
                    Err(e) => {
                        self.ui
                            .log(tick, vivarium_ui::Level::Error, format!("保存失败：{e}"))
                    }
                }
            }
            C::DeleteBody(name) => {
                let tick = self.world.tick();
                if let Err(e) = delete_body(&name) {
                    self.ui
                        .log(tick, vivarium_ui::Level::Error, format!("删除失败：{e}"));
                } else {
                    self.ui.state.library = load_library();
                    self.ui
                        .log(tick, vivarium_ui::Level::Info, format!("已删除：{name}"));
                }
            }
            C::Quit => event_loop.exit(),
            // 界面已经处理过的（Select / SetTool / SetBrush / SetMaterial / SetCell）。
            _ => {}
        }
    }

    fn refresh_title(&mut self) {
        self.fps_frames += 1;
        let elapsed = self.fps_window_start.elapsed();
        if elapsed < Duration::from_millis(500) {
            return;
        }
        self.fps = self.fps_frames as f32 / elapsed.as_secs_f32();
        self.fps_frames = 0;
        self.fps_window_start = Instant::now();

        let stats = self.renderer.stats();
        let sim = self.sim.stats();
        let workers = self.sim.workers();
        let life = self.life.stats();
        let total = self.world.chunk_count();
        let size = self.window.inner_size();
        let px = self.renderer.pixels_per_cell((size.width, size.height));
        self.window.set_title(&format!(
            "Vivarium — tick {} · {:.0} fps · {:.1} px/格 · {} · 活跃块 {}/{} · 工人 {} · 生物 {} · 能量 {} · 本帧上传 {}/{} 块 · 累计 {} 块",
            self.world.tick(),
            self.fps,
            px,
            scale_label(self.scale),
            sim.active_chunks,
            total,
            workers,
            life.organisms,
            life.energy,
            stats.last_uploaded_chunks,
            total,
            stats.total_uploaded_chunks,
        ));
    }
}

/// 把字节数写成给人看的大小。
///
/// 用 1024 而不是 1000：存档列表要回答的是"这个档大不大"，
/// 而"1.0 MB"和 Windows 资源管理器里看到的是同一个数——
/// 两处不一致会让人以为自己找错了文件。
fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{:.1} MB", b / MB)
    }
}

fn scale_label(scale: TimeScale) -> &'static str {
    match scale {
        TimeScale::Paused => "已暂停",
        TimeScale::X1 => "1×",
        TimeScale::X5 => "5×",
        TimeScale::X50 => "50×",
        TimeScale::Max => "最大",
    }
}

// ---------------------------------------------------------------------------
// 笔刷
// ---------------------------------------------------------------------------
//
// 这两个函数是**自由函数**而不是 `Game` 的方法，只为了让它们能在没有窗口、
// 没有 GPU 的情况下被测。画笔逻辑（哪些格该涂、哪些不能涂）是纯数据操作，
// 和窗口没有半点关系——把它挂在 `Game` 上，就意味着它只能靠"人点一下试试"来验证。

/// 油漆桶一次最多涂多少格。
///
/// 一片水在 512² 里有三万多格，涂完要几十毫秒——可以接受。但"把整个世界
/// 填满"是点一下卡一秒，所以封顶，并在日志里说清楚没涂完。
const FILL_CAP: usize = 60_000;

/// 在 `(x, y)` 盖一个半径 `r` 的圆。
///
/// 返回改了几格（测试用它判断"到底有没有落笔"）。
fn stamp_circle(world: &mut World, x: i32, y: i32, r: i32, kind: MaterialKind) -> usize {
    let mut n = 0;
    for dy in -r..=r {
        for dx in -r..=r {
            if dx * dx + dy * dy > r * r {
                continue;
            }
            let (xs, ys) = ((x + dx) as isize, (y + dy) as isize);
            if !world.in_bounds(xs, ys) {
                continue;
            }
            // **不覆盖生物。** 一格要么是材质要么是细胞，画笔只能改材质那一半；
            // 硬涂上去会让占格表和材质打架（那条鱼还占着这格，而格子已经变成石头）。
            if world.is_occupied(xs, ys) {
                continue;
            }
            if world.get(xs, ys) == kind {
                continue;
            }
            world.set(xs, ys, kind);
            n += 1;
        }
    }
    n
}

/// 油漆桶：把和 `(x, y)` 连成一片的同种材质换成 `kind`。
///
/// 返回 `(涂了几格, 有没有撞上限)`。
///
/// 用显式栈而不是递归：512² 里一片水有三万多格，递归会直接把栈打爆。
fn flood_fill_region(
    world: &mut World,
    x: i32,
    y: i32,
    kind: MaterialKind,
    cap: usize,
) -> (usize, bool) {
    let target = world.get(x as isize, y as isize);
    if target == kind {
        return (0, false);
    }
    let mut stack = vec![(x, y)];
    let mut seen = std::collections::HashSet::new();
    let mut filled = 0usize;
    while let Some((cx, cy)) = stack.pop() {
        if filled >= cap {
            return (filled, true);
        }
        let (xs, ys) = (cx as isize, cy as isize);
        if !world.in_bounds(xs, ys) || !seen.insert((cx, cy)) {
            continue;
        }
        // 生物占着的格子既不算"同种材质"，也不该被改——直接当成墙。
        if world.is_occupied(xs, ys) || world.get(xs, ys) != target {
            continue;
        }
        world.set(xs, ys, kind);
        filled += 1;
        stack.push((cx + 1, cy));
        stack.push((cx - 1, cy));
        stack.push((cx, cy + 1));
        stack.push((cx, cy - 1));
    }
    (filled, false)
}

// ---------------------------------------------------------------------------
// 地形
// ---------------------------------------------------------------------------
//
// 地貌生成已经搬进 `vivarium_core::worldgen`。
//
// 原先这里有一份，`bins/sim` 和 `crates/render` 的预览测试里各有一份，
// 三份已经开始漂移。世界地貌是**设计决定**（SPEC 第 2.0 节：横板侧视、
// 重力向下、海洋 + 岛屿），不该有三份互相矛盾的实现。
//
// M1 起，生成出来的地形不再是静态的：沙会塌、水会流、气会浮、火会烧，
// 全部由 `vivarium_core::MaterialSim` 驱动。启动画面因此会自己演化——
// 岛上的沙往海里滑、水找平、气泡从沉积层里浮上来。

// ---------------------------------------------------------------------------
// ASCII 降级路径
// ---------------------------------------------------------------------------

fn run_ascii(opts: &Options) -> ExitCode {
    let mut world = match World::new(opts.width, opts.height) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("无法创建世界：{e}");
            return ExitCode::FAILURE;
        }
    };

    let mut rng = Rng::new(opts.seed);
    let layout = worldgen::sea_and_islands(&mut world, &mut rng);

    // 40 行而不是 30：512² 的剖面里，"天空 + 岛屿 + 水面线"这一段就有
    // 几十格高，30 行只够装下岛的实体部分，屏幕上会只剩一片 '#'。
    let view_width = 120.min(world.width());
    let view_height = 40.min(world.height());
    let mut camera = Camera::fit_bottom_left(&world, view_width, view_height);
    // 直接贴左下角的话，512² 世界里那近百格厚的岩层会把整个视野填满，
    // 屏幕上只剩一片 '#'——那等于什么都没看到。
    // 由地貌自己告诉我们该往哪儿看（见 `WorldLayout::focus_left/focus_top`）。
    let target_x = layout.focus_left(view_width);
    let target_y = layout.focus_top(view_height);
    camera.pan(
        &world,
        target_x as isize - camera.x as isize,
        target_y as isize - camera.y as isize,
    );

    let mut renderer = AsciiRenderer::new(camera);

    let hash_before = world.hash();
    let mut sim = MaterialSim::new();
    for _ in 0..opts.ticks {
        sim.step(&mut world);
    }
    let hash_after = world.hash();

    if let Err(e) = renderer.render(&world) {
        eprintln!("渲染失败：{e}");
        return ExitCode::FAILURE;
    }

    println!("Vivarium — ASCII 降级视图（渲染器 = {}）", renderer.name());
    println!(
        "{}×{} 世界 · 横板侧视 · 水面线 y={} · 海床线 y={} · 陆地 {} 列",
        world.width(),
        world.height(),
        layout.waterline,
        layout.seabed,
        layout.land_columns()
    );
    println!("跑了 {} tick", opts.ticks);
    println!("hash {hash_before:#018x} → {hash_after:#018x}");
    println!();
    println!("{}", renderer.frame());

    ExitCode::SUCCESS
}

// ---------------------------------------------------------------------------
// 测试：画笔
// ---------------------------------------------------------------------------
//
// 这两条测的是**纯数据操作**，不需要窗口也不需要 GPU。落笔逻辑挂在这里
// 而不是 `Game` 上，就是为了让它们能跑。
#[cfg(test)]
mod tests {
    use super::*;

    /// 一片水，中间一条石墙，用来测"填充会不会越界"。
    fn pool_with_wall() -> World {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 0, 64, 64, MaterialKind::Water);
        // 竖墙把水池切成左右两半。
        w.fill_rect(32, 0, 1, 64, MaterialKind::Stone);
        w
    }

    #[test]
    fn a_round_brush_paints_a_disc_not_a_square() {
        let mut w = pool_with_wall();
        let n = stamp_circle(&mut w, 10, 10, 3, MaterialKind::Stone);
        assert!(n > 20, "半径 3 的圆至少二十几格，实际 {n}");

        // 圆内。
        assert_eq!(w.get(10, 10), MaterialKind::Stone, "圆心必须涂到");
        assert_eq!(w.get(13, 10), MaterialKind::Stone, "半径上的点必须涂到");
        // 圆外：正方形的角。
        assert_eq!(
            w.get(13, 13),
            MaterialKind::Water,
            "正方形的角不该被涂到——那不是圆"
        );
    }

    #[test]
    fn the_brush_never_paints_over_a_body() {
        let mut w = pool_with_wall();
        let mut sim = LifeSim::new(&mut w);
        let id = sim
            .spawn_injected(&mut w, LifeSim::simple_fish((10, 10), Genome::blank(), 500))
            .unwrap();
        let spots: Vec<(isize, isize)> = sim
            .organism(id)
            .unwrap()
            .footprint()
            .map(|(x, y)| (x as isize, y as isize))
            .collect();
        assert!(!spots.is_empty());

        // 把整条鱼连周围一起涂成石头。
        stamp_circle(&mut w, 10, 10, 6, MaterialKind::Stone);

        for (x, y) in spots {
            assert!(
                w.get(x, y) != MaterialKind::Stone,
                "({x}, {y}) 是身体格，画笔不能把它涂成石头"
            );
            assert!(w.is_occupied(x, y), "身体格必须还归那条鱼");
        }
    }

    #[test]
    fn flood_fill_stops_at_a_wall() {
        let mut w = pool_with_wall();
        let (n, capped) = flood_fill_region(&mut w, 5, 5, MaterialKind::Sand, FILL_CAP);
        assert!(!capped);
        assert_eq!(n, 32 * 64, "左半边是 32×64 格水");

        // 左边全变沙，右边一格没动，墙也没动。
        assert_eq!(w.get(0, 0), MaterialKind::Sand);
        assert_eq!(w.get(31, 63), MaterialKind::Sand);
        assert_eq!(w.get(33, 0), MaterialKind::Water, "填充越过了墙");
        assert_eq!(w.get(32, 0), MaterialKind::Stone, "墙本身不该被涂");
    }

    #[test]
    fn flood_fill_gives_up_at_the_cap() {
        // 上限存在的理由：一次点击不该让整个程序停一秒。
        let mut w = pool_with_wall();
        let (n, capped) = flood_fill_region(&mut w, 5, 5, MaterialKind::Sand, 100);
        assert!(capped, "超过上限时必须报出来");
        assert_eq!(n, 100);
        assert_eq!(w.get(5, 5), MaterialKind::Sand, "涂过的那些要真的涂上");
    }

    #[test]
    fn flood_fill_into_the_same_material_does_nothing() {
        let mut w = pool_with_wall();
        let (n, capped) = flood_fill_region(&mut w, 5, 5, MaterialKind::Water, FILL_CAP);
        assert_eq!(n, 0);
        assert!(!capped);
    }
}

// ---------------------------------------------------------------------------
// 生物库
// ---------------------------------------------------------------------------
//
// 存在**可执行文件旁边的 `bodies/` 目录**里，格式是纯文本：
//
// ```text
// 三节虫
// 0 0 2
// 1 0 2
// 0 -1 1
// ```
//
// 第一行是名字，之后每行是 `x y 细胞类型`（坐标相对锚点）。
//
// ## 为什么不用 JSON / 二进制
//
// 一个 `serde` 依赖就是几十万行代码和几分钟编译时间，而这里要存的只是
// 十几个三元组。纯文本还带来一个实际好处：**用户能直接打开看、能手改、
// 能贴进 issue**。存档格式的门槛越低，它被分享的机会越多。
//
// 代价是没有转义，所以名字要过滤掉路径分隔符——见 `safe_file_name`。

/// 生物库目录。可执行文件旁边。
fn library_dir() -> std::path::PathBuf {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("bodies")
}

/// 把名字变成安全的文件名。
///
/// 只保留字母、数字、汉字和几个安全的符号，其余一律换成 `_`。
/// **不能只去掉 `..`**：Windows 上 `:` `*` `?` 都不合法，而用户起名字时
/// 完全可能用它们。换掉而不是报错，是因为"存不进去"比"名字被改了一个字符"
/// 更让人困惑。
fn safe_file_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_alphanumeric() || matches!(c, '-' | '_' | ' ') {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim().to_owned();
    if trimmed.is_empty() {
        "未命名".to_owned()
    } else {
        trimmed
    }
}

/// 扫一遍生物库目录。
///
/// 读不了的文件**跳过而不是报错**：目录里可能有用户手放的说明文件，
/// 一个坏文件不该让整个库都打不开。
fn load_library() -> Vec<vivarium_ui::LibraryEntry> {
    let dir = library_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("txt") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(body) = parse_body(&text) {
            out.push(body);
        }
    }
    // 名字排序：目录顺序在不同文件系统上不一样，不排的话列表每次启动都变。
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 解析一份身体文件。格式不对就返回 `None`（跳过这一个文件）。
fn parse_body(text: &str) -> Option<vivarium_ui::LibraryEntry> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let name = lines.next()?.trim().to_owned();
    let mut cells = Vec::new();
    for line in lines {
        let mut parts = line.split_whitespace();
        let x: i16 = parts.next()?.parse().ok()?;
        let y: i16 = parts.next()?.parse().ok()?;
        let k: u8 = parts.next()?.parse().ok()?;
        cells.push((x, y, k));
    }
    if cells.is_empty() {
        return None;
    }
    Some(vivarium_ui::LibraryEntry { name, cells })
}

/// 存一份身体。返回写到的路径。
fn save_body(name: &str, cells: &[(i16, i16, u8)]) -> std::io::Result<std::path::PathBuf> {
    let dir = library_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.txt", safe_file_name(name)));
    let mut text = String::with_capacity(32 + cells.len() * 10);
    text.push_str(name.trim());
    text.push('\n');
    for &(x, y, k) in cells {
        text.push_str(&format!("{x} {y} {k}\n"));
    }
    std::fs::write(&path, text)?;
    Ok(path)
}

/// 删一份身体。
fn delete_body(name: &str) -> std::io::Result<()> {
    let path = library_dir().join(format!("{}.txt", safe_file_name(name)));
    std::fs::remove_file(path)
}

#[cfg(test)]
mod library_tests {
    use super::*;

    /// 存一轮再读回来，细胞一格不差。
    ///
    /// 这条测试守的是"格式和解析器对得上"——手写格式最容易犯的错就是
    /// 写的时候多一个字段、读的时候少一个，而症状是"存进去的身体少了几格"，
    /// 从界面上完全看不出来。
    #[test]
    fn body_round_trips_through_text() {
        let cells = vec![(0i16, 0i16, 2u8), (1, 0, 2), (0, -1, 1), (-1, 0, 0)];
        let mut text = String::from("三节虫\n");
        for &(x, y, k) in &cells {
            text.push_str(&format!("{x} {y} {k}\n"));
        }
        let parsed = parse_body(&text).expect("应当解析得出来");
        assert_eq!(parsed.name, "三节虫");
        assert_eq!(parsed.cells, cells);
    }

    /// 坏文件被跳过，而不是让整个库打不开。
    #[test]
    fn malformed_body_is_skipped() {
        assert!(parse_body("").is_none());
        assert!(parse_body("只有名字\n").is_none());
        assert!(parse_body("名字\n不是数字 0 2\n").is_none());
        // 空行不算坏：手写文件常常多一个换行。
        assert!(parse_body("名字\n\n0 0 2\n").is_some());
    }

    /// 名字里的非法字符被换掉，而不是报错或逃出目录。
    #[test]
    fn file_names_cannot_escape_the_directory() {
        // 每个非法字符换一个 `_`：`.` `.` `/` 三个 → `___`。
        // 关键是**没有 `/` 也没有 `..` 剩下**，名字就逃不出目录。
        assert_eq!(safe_file_name("../etc/passwd"), "___etc_passwd");
        assert!(!safe_file_name("../x").contains('/'));
        assert!(!safe_file_name("../x").contains(".."));
        assert_eq!(safe_file_name("a:b*c?"), "a_b_c_");
        assert_eq!(safe_file_name("   "), "未命名");
        // 汉字要保留——中文名字是常态。
        assert_eq!(safe_file_name("银鱼"), "银鱼");
    }
}
