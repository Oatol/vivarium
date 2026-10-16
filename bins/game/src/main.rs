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
//! | 键 | 作用 |
//! |---|---|
//! | 空格 | 暂停 / 继续 |
//! | 1 / 2 / 3 / 4 | 速度 1× / 5× / 50× / 最大 |
//! | R | 重新铺一遍地形（用来验证增量上传确实在工作） |
//! | Esc | 退出 |

use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use vivarium_app::{FrameBudget, TimeScale};
use vivarium_core::{worldgen, MaterialKind, Rng, World, WorldLayout};
use vivarium_render::ascii::{AsciiRenderer, Camera};
use vivarium_render::pixel::PixelRenderer;
use vivarium_render::Renderer;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
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
}

impl Default for Options {
    fn default() -> Self {
        Self {
            ascii: false,
            width: DEFAULT_SIZE,
            height: DEFAULT_SIZE,
            seed: DEFAULT_SEED,
            ticks: 60,
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
    let raw = args.next().ok_or_else(|| format!("{flag} 后面缺一个数字"))?;
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

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                game.renderer.resize(size.width, size.height);
                game.window.request_redraw();
            }

            WindowEvent::KeyboardInput { event, .. } => {
                // 忽略松键与自动重复，否则按住 R 会以每秒 30 次的速度重铺地形。
                if event.state == ElementState::Pressed && !event.repeat {
                    game.on_key(&event.logical_key, event_loop);
                }
            }

            WindowEvent::RedrawRequested => game.frame(),

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
    /// 初始地貌的两条基准线。横幅要用，将来相机取景也要用。
    layout: WorldLayout,
    rng: Rng,
    seed: u64,

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
        let renderer =
            PixelRenderer::new(&instance, surface, size.width, size.height).map_err(|e| e.to_string())?;

        let mut world = World::new(opts.width, opts.height).map_err(|e| e.to_string())?;
        let mut rng = Rng::new(opts.seed);
        let layout = worldgen::sea_and_islands(&mut world, &mut rng);

        Ok(Self {
            window,
            instance,
            renderer,
            world,
            layout,
            rng,
            seed: opts.seed,
            scale: TimeScale::X1,
            budget: FrameBudget::default(),
            last_tick_cost: Duration::ZERO,
            fps_window_start: Instant::now(),
            fps_frames: 0,
            fps: 0.0,
        })
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

  空格 暂停/继续 · 1/2/3/4 速度 · R 重铺地形 · Esc 退出",
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
        )
    }

    fn on_key(&mut self, key: &Key, event_loop: &ActiveEventLoop) {
        match key {
            Key::Named(NamedKey::Escape) => event_loop.exit(),
            Key::Named(NamedKey::Space) => {
                self.scale = if self.scale.is_paused() {
                    TimeScale::X1
                } else {
                    TimeScale::Paused
                };
            }
            Key::Character(text) => match text.as_str() {
                "1" => self.scale = TimeScale::X1,
                "2" => self.scale = TimeScale::X5,
                "3" => self.scale = TimeScale::X50,
                "4" => self.scale = TimeScale::Max,
                "r" | "R" => self.reseed(),
                _ => {}
            },
            _ => {}
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
    }

    fn frame(&mut self) {
        let tick_start = Instant::now();
        let ticks = self.budget.plan_ticks(self.scale, self.last_tick_cost);
        for _ in 0..ticks {
            self.world.step();
        }
        if ticks > 0 {
            self.last_tick_cost = tick_start.elapsed() / ticks;
        }

        if let Err(e) = self.renderer.render(&self.world) {
            eprintln!("渲染失败：{e}");
        }

        self.refresh_title();
        // 一直重画：模拟速度与帧率是解耦的，暂停时也应当保持窗口响应。
        self.window.request_redraw();
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
        let total = self.world.chunk_count();
        self.window.set_title(&format!(
            "Vivarium — tick {} · {:.0} fps · {} · 本帧上传 {}/{} 块 · 累计 {} 块",
            self.world.tick(),
            self.fps,
            scale_label(self.scale),
            stats.last_uploaded_chunks,
            total,
            stats.total_uploaded_chunks,
        ));
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
// 地形
// ---------------------------------------------------------------------------
//
// 地貌生成已经搬进 `vivarium_core::worldgen`。
//
// 原先这里有一份，`bins/sim` 和 `crates/render` 的预览测试里各有一份，
// 三份已经开始漂移。世界地貌是**设计决定**（SPEC 第 2.0 节：横板侧视、
// 重力向下、海洋 + 岛屿），不该有三份互相矛盾的实现。
//
// 顺带一提：生成出来的地形是静态的——沙不会塌、水不会流、火不会烧。
// 那是 M1 的材质流动规则要接上的事。

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
    for _ in 0..opts.ticks {
        world.step();
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
