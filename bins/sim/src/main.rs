//! Vivarium headless 模拟器。
//!
//! 这个 bin **不依赖任何渲染库**。它存在的全部理由，是让"跑几千上万代"变得廉价——
//! 验证进化靠长跑，不靠盯画面。
//!
//! ```text
//! cargo run -p vivarium-sim -- --help
//! cargo run -p vivarium-sim -- --ticks 10000 --seed 42
//! cargo run -p vivarium-sim -- --width 128 --height 128 --ticks 200 --ascii
//! ```
//!
//! 输出里的 `hash` 是网格的确定性指纹。同种子 + 同参数 ⇒ 同 hash，
//! 这是 SPEC 里"可复现性"那条验收断言的命令行入口。

use std::process::ExitCode;
use std::time::{Duration, Instant};

use vivarium_core::{MaterialKind, Rng, World};
use vivarium_render::ascii::{AsciiRenderer, Camera};
use vivarium_render::Renderer;

/// 默认世界尺寸。SPEC：开发期 512²，发布配置 1024²，运行时可通过参数调整。
const DEFAULT_WIDTH: usize = 512;
const DEFAULT_HEIGHT: usize = 512;
const DEFAULT_TICKS: u64 = 1_000;
const DEFAULT_SEED: u64 = 42;

/// ASCII 视图尺寸——终端放不下 512 列。
const ASCII_VIEW_WIDTH: usize = 120;
const ASCII_VIEW_HEIGHT: usize = 40;

struct Args {
    width: usize,
    height: usize,
    ticks: u64,
    seed: u64,
    ascii: bool,
    quiet: bool,
    report_every: u64,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            ticks: DEFAULT_TICKS,
            seed: DEFAULT_SEED,
            ascii: false,
            quiet: false,
            report_every: 0,
        }
    }
}

fn usage() -> String {
    format!(
        "\
Vivarium headless 模拟器

用法:
    vivarium-sim [选项]

选项:
    --width <N>          世界宽度（必须是 64 的倍数）  [默认 {DEFAULT_WIDTH}]
    --height <N>         世界高度（必须是 64 的倍数）  [默认 {DEFAULT_HEIGHT}]
    --ticks <N>          推进的 tick 数                [默认 {DEFAULT_TICKS}]
    --seed <N>           随机种子                      [默认 {DEFAULT_SEED}]
    --report-every <N>   每 N 个 tick 打印一次进度     [默认 0 = 只在结束时打印]
    --ascii              结束时打印 ASCII 视图
    --quiet              只输出最终报告
    -h, --help           显示本帮助"
    )
}

enum Parsed {
    Run(Box<Args>),
    Help,
}

fn parse_args() -> Result<Parsed, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);

    while let Some(flag) = it.next() {
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--ascii" => args.ascii = true,
            "--quiet" => args.quiet = true,
            "--width" => args.width = take_usize(&mut it, "--width")?,
            "--height" => args.height = take_usize(&mut it, "--height")?,
            "--ticks" => args.ticks = take_u64(&mut it, "--ticks")?,
            "--seed" => args.seed = take_u64(&mut it, "--seed")?,
            "--report-every" => args.report_every = take_u64(&mut it, "--report-every")?,
            other => return Err(format!("未知参数: {other}\n\n{}", usage())),
        }
    }

    Ok(Parsed::Run(Box::new(args)))
}

fn take_usize(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<usize, String> {
    let raw = it
        .next()
        .ok_or_else(|| format!("{flag} 缺少取值\n\n{}", usage()))?;
    raw.parse::<usize>()
        .map_err(|_| format!("{flag} 需要一个非负整数，收到: {raw}"))
}

fn take_u64(it: &mut impl Iterator<Item = String>, flag: &str) -> Result<u64, String> {
    let raw = it
        .next()
        .ok_or_else(|| format!("{flag} 缺少取值\n\n{}", usage()))?;
    raw.parse::<u64>()
        .map_err(|_| format!("{flag} 需要一个非负整数，收到: {raw}"))
}

/// 铺一片确定性的初始地形。
///
/// 刻意写得简单——M0 的职责只是"世界能被看见、能被复现"，
/// 真正的材质流动规则属于 M3。这里的唯一硬要求是：**同种子必须给出同一个世界**。
/// 返回值是基岩顶面的 Y 坐标，供相机对准地表使用。
fn seed_world(world: &mut World, rng: &mut Rng) -> isize {
    let w = world.width();
    let h = world.height();

    // 石头基岩：底部八分之一
    let floor_top = h - h / 8;
    world.fill_rect(0, floor_top as isize, w, h - floor_top, MaterialKind::Stone);

    // 起伏的沙层：一条随机游走的表面线
    let mut surface = floor_top as isize;
    let lowest = (floor_top as isize - 16).max(1);
    for x in 0..w as isize {
        match rng.gen_range_u32(0, 4) {
            0 => surface = (surface + 1).min(floor_top as isize + 4),
            1 => surface = (surface - 1).max(lowest),
            _ => {}
        }
        let thickness = rng.gen_range_usize(1, 5);
        world.fill_rect(x, surface, 1, thickness, MaterialKind::Sand);
    }

    // 一个水洼，制造材质边界
    let pool_w = (w / 8).max(8);
    let pool_x = (w / 3) as isize;
    let pool_top = (floor_top as isize - 12).max(1);
    world.fill_rect(pool_x, pool_top, pool_w, 12, MaterialKind::Water);

    // 若干团可燃气体，给火留点戏（M3 之后才会真的烧起来）
    let blob_count = w / 4;
    let sky = (floor_top as isize - 24).max(1) as usize;
    for _ in 0..blob_count {
        let x = rng.gen_range_usize(0, w) as isize;
        let y = rng.gen_range_usize(0, sky) as isize;
        world.fill_rect(x, y, 2, 2, MaterialKind::Gas);
    }

    floor_top as isize
}

fn print_report(world: &World, elapsed: Duration) {
    let census = world.census();
    let total = (world.width() * world.height()) as f64;

    println!(
        "tick {}  hash {:#018x}  dirty_chunks {}/{}",
        world.tick(),
        world.hash(),
        world.dirty_chunk_count(),
        world.chunk_count()
    );

    for kind in MaterialKind::ALL {
        let n = census[kind.as_u8() as usize];
        if n == 0 {
            continue;
        }
        let pct = n as f64 / total * 100.0;
        println!("  {:<8} {:>9}  {:>5.2}%", kind.name(), n, pct);
    }

    if !elapsed.is_zero() {
        println!("  用时      {:?}", elapsed);
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Parsed::Help) => {
            println!("{}", usage());
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Run(a)) => *a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let mut world = match World::new(args.width, args.height) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("无法创建世界: {e}");
            return ExitCode::from(2);
        }
    };

    let mut rng = Rng::new(args.seed);
    let surface = seed_world(&mut world, &mut rng);

    if !args.quiet {
        println!(
            "Vivarium sim — {}x{}  seed {}  ticks {}",
            args.width, args.height, args.seed, args.ticks
        );
        println!("初始状态:");
        print_report(&world, Duration::ZERO);
        println!();
    }

    let start = Instant::now();
    for i in 0..args.ticks {
        world.step();
        if args.report_every > 0 && (i + 1) % args.report_every == 0 {
            println!(
                "  [{:>8}/{}]  hash {:#018x}",
                i + 1,
                args.ticks,
                world.hash()
            );
        }
    }
    let elapsed = start.elapsed();

    println!("最终状态:");
    print_report(&world, elapsed);

    if args.ascii {
        println!();
        // 相机对准地表：让视野的四分之三落在地表之上，
        // 这样一眼能看到天空、材质边界和基岩，而不是一大片石头或一大片空天。
        let view_y = surface.saturating_sub((ASCII_VIEW_HEIGHT * 3 / 4) as isize) as usize;
        let mut renderer = AsciiRenderer::new(Camera::new(
            0,
            view_y,
            ASCII_VIEW_WIDTH,
            ASCII_VIEW_HEIGHT,
        ));
        if let Err(e) = renderer.render(&world) {
            eprintln!("ASCII 渲染失败: {e}");
            return ExitCode::FAILURE;
        }
        println!(
            "ASCII 视图（{}x{}，对准地表 y={}，渲染器 = {}）:",
            ASCII_VIEW_WIDTH,
            ASCII_VIEW_HEIGHT,
            view_y,
            renderer.name()
        );
        println!("{}", renderer.frame());
    }

    ExitCode::SUCCESS
}
