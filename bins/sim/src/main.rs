//! Vivarium headless 模拟器。
//!
//! 这个 bin **不依赖任何渲染库**。它存在的全部理由，是让"跑几千上万代"变得廉价——
//! 验证进化靠长跑，不靠盯画面。
//!
//! ```text
//! cargo run -p vivarium-sim -- --help
//! cargo run -p vivarium-sim -- --ticks 10000 --seed 42
//! cargo run -p vivarium-sim -- --width 128 --height 128 --ticks 200 --ascii
//! cargo run -p vivarium-sim -- --ticks 3000 --ignite 40 --report-every 500
//! ```
//!
//! 输出里的 `hash` 是网格的确定性指纹。同种子 + 同参数 ⇒ 同 hash，
//! 这是 SPEC 里"可复现性"那条验收断言的命令行入口。
//!
//! ## 两个比 hash 更重要的数字
//!
//! - **活跃块数**：本 tick 真正被检查的分块数量。SPEC 第 2.4 节说得很清楚，
//!   材质层的成本取决于活跃格子比例，不是网格尺寸。世界平静下来之后，
//!   这个数字应当掉到 0——掉不下去就说明有东西在永远抖动，
//!   跳块优化形同虚设。
//! - **材质变化表**：跑了 N 个 tick 之后，每种材质多了多少少了多少。
//!   守恒断言出错时，一眼就能看出是哪种材质凭空出现或消失了。

use std::process::ExitCode;
use std::time::Instant;

use vivarium_core::life_sim::{stock_surface, LifeSim};
use vivarium_core::{worldgen, MaterialKind, MaterialSim, Rng, World, WorldLayout};
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
    ignite: usize,
    fish: usize,
    threads: usize,
    ascii: bool,
    quiet: bool,
    report_every: u64,
    /// 关掉海底的藻席。用来做 A/B：想知道藻席改变了什么，就得能跑没有它的世界。
    no_algae: bool,
    /// 形态发生模式：身体由基因组长出来，而不是用手工体型表。
    morph: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            ticks: DEFAULT_TICKS,
            seed: DEFAULT_SEED,
            ignite: 0,
            fish: 0,
            threads: 0,
            ascii: false,
            quiet: false,
            report_every: 0,
            no_algae: false,
            morph: false,
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
    --ignite <N>         在最高的岛顶点燃 N 格火（看它上浮、十几 tick 后烧完）  [默认 0 = 不点火]
    --fish <N>           在水面附近投放 N 条手工设计的鱼  [默认 0 = 没有生物]
    --threads <N>        材质层的并行工人数  [默认 0 = 自动：按本机核数与当 tick 活量收敛]
    --report-every <N>   每 N 个 tick 打印一次进度     [默认 0 = 只在结束时打印]
    --no-algae           关掉海底的藻席（对照实验用）
    --morph              身体由基因组**长出来**（形态发生），而不是用手工体型表
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
            "--no-algae" => args.no_algae = true,
            "--morph" => args.morph = true,
            "--width" => args.width = take_usize(&mut it, "--width")?,
            "--height" => args.height = take_usize(&mut it, "--height")?,
            "--ticks" => args.ticks = take_u64(&mut it, "--ticks")?,
            "--seed" => args.seed = take_u64(&mut it, "--seed")?,
            "--ignite" => args.ignite = take_usize(&mut it, "--ignite")?,
            "--fish" => args.fish = take_usize(&mut it, "--fish")?,
            "--threads" => args.threads = take_usize(&mut it, "--threads")?,
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

// ---------------------------------------------------------------------------
// 地形
// ---------------------------------------------------------------------------
//
// 地貌生成在 `vivarium_core::worldgen`（SPEC 第 2.0 节：横板侧视、
// 重力向下、海洋 + 岛屿）。这里只负责调用，不负责实现——
// 早先 sim / game / 渲染预览各有一份副本，已经开始漂移了。

/// 材质的中文短名。仅用于控制台报告。
fn material_label(kind: MaterialKind) -> &'static str {
    match kind {
        MaterialKind::Empty => "空",
        MaterialKind::Stone => "石",
        MaterialKind::Sand => "沙",
        MaterialKind::Water => "水",
        MaterialKind::Fire => "火",
        MaterialKind::Gas => "气",
        MaterialKind::Remains => "骸",
        MaterialKind::Corpse => "尸",
        MaterialKind::Algae => "藻",
    }
}

/// 在一行里紧凑地列出所有材质的格子数。
fn census_line(census: &[usize; MaterialKind::COUNT]) -> String {
    MaterialKind::ALL
        .iter()
        .map(|k| format!("{} {}", material_label(*k), census[k.as_u8() as usize]))
        .collect::<Vec<_>>()
        .join("  ")
}

/// 在最高的岛顶撒 `count` 格火，用来观察燃烧与蔓延。
///
/// 点火位置贴着地表往上放一格——直接放在地表会立刻被地形埋掉。
fn ignite_peak(world: &mut World, layout: &WorldLayout, count: usize) {
    if count == 0 {
        return;
    }
    let peak = layout.peak_x();
    let half = count / 2;
    for i in 0..count {
        let x = (peak + i).saturating_sub(half).min(world.width() - 1);
        let surface = layout.surface[x];
        if surface <= 0 {
            continue;
        }
        // 往上一格点火。若那里不是空的（比如正好是岩石），就跳过。
        let y = surface - 1;
        if world.get(x as isize, y) == MaterialKind::Empty {
            world.set(x as isize, y, MaterialKind::Fire);
        }
    }
}

/// 跨 tick 累积的性能与工作量统计。
#[derive(Default)]
struct RunStats {
    active_chunk_sum: u64,
    visited_sum: u64,
    moved_sum: u64,
    reacted_sum: u64,
    /// 累计腐烂掉的尸体格数。
    ///
    /// 这个数字必须能看见：尸体是唯一一种"生物制造、生物层消化"的材质，
    /// 消化速度跟不上制造速度时世界会被尸体填满，而那种崩坏从
    /// "生物数"和"能量账"上完全看不出来——两者都会是健康的。
    decayed_sum: u64,
    workers_max: usize,
    ticks: u64,
}

impl RunStats {
    fn record(&mut self, sim: &MaterialSim, life: &LifeSim) {
        let s = sim.stats();
        self.active_chunk_sum += s.active_chunks as u64;
        self.visited_sum += s.visited as u64;
        self.moved_sum += s.moved as u64;
        self.reacted_sum += s.reacted as u64;

        // 峰值而不是末值：跑到最后世界已经快静了，只剩一两块在动，
        // 那时的工人数反映不了这次运行实际用到的并行度。
        self.workers_max = self.workers_max.max(sim.workers());
        self.decayed_sum += life.stats().corpses_rotted as u64;
        self.ticks += 1;
    }
}

fn print_state(world: &World, label: &str) {
    let census = world.census();
    let total = (world.width() * world.height()) as f64;

    println!(
        "{label}  tick {}  hash {:#018x}  dirty_chunks {}/{}",
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
}

/// 打印"初始 → 最终"的材质变化表。
///
/// 守恒断言出错时，这张表比"哈希不一样"有用得多：它直接告诉你是哪种材质
/// 凭空多出来或少掉了。
fn print_delta(before: &[usize; MaterialKind::COUNT], after: &[usize; MaterialKind::COUNT]) {
    let mut lines = Vec::new();
    for kind in MaterialKind::ALL {
        let i = kind.as_u8() as usize;
        if before[i] == after[i] {
            continue;
        }
        let d = after[i] as isize - before[i] as isize;
        lines.push(format!(
            "  {:<8} {:>9} → {:>9}   {:+}",
            kind.name(),
            before[i],
            after[i],
            d
        ));
    }
    if lines.is_empty() {
        println!("材质变化: 无（逐种守恒）");
    } else {
        println!("材质变化（初始 → 最终）:");
        for line in lines {
            println!("{line}");
        }
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
    let layout = worldgen::sea_and_islands(&mut world, &mut rng);
    ignite_peak(&mut world, &layout, args.ignite);

    let census_before = world.census();

    if !args.quiet {
        println!(
            "Vivarium sim — {}x{}  seed {}  ticks {}",
            args.width, args.height, args.seed, args.ticks
        );
        println!(
            "地貌：横板侧视 · 海洋 + 岛屿 · 水面线 y={} · 海床线 y={} · 陆地 {} 列",
            layout.waterline,
            layout.seabed,
            layout.land_columns()
        );
        if args.ignite > 0 {
            println!(
                "点火：最高岛峰顶 x={} 附近 {} 格",
                layout.peak_x(),
                args.ignite
            );
        }
        println!("初始状态:");
        print_state(&world, "");
        println!();
    }

    let mut sim = if args.threads == 0 {
        MaterialSim::new()
    } else {
        MaterialSim::with_threads(args.threads)
    };
    // 生命层必须在材质层**之前**跑：生物挤开的水要在同一个 tick 里流走。
    // 反过来（先材质后生物）会让"鱼游过去"和"水合拢"差一拍，
    // 视觉上像是鱼在水里拖了一条尾迹。
    let mut life = LifeSim::new(&mut world);
    life.set_food_growth(!args.no_algae);
    if args.fish > 0 {
        let ids = stock_surface(&mut world, &mut life, args.fish, 300, args.morph);
        if !args.quiet {
            println!("投放 {} 条鱼（水面附近）", ids.len());
            println!();
        }
    }

    let mut stats = RunStats::default();
    let start = Instant::now();
    for i in 0..args.ticks {
        life.step(&mut world);
        sim.step(&mut world);
        stats.record(&sim, &life);

        if args.report_every > 0 && (i + 1) % args.report_every == 0 {
            let s = sim.stats();
            let l = life.stats();
            println!(
                "  [{:>8}/{}]  hash {:#018x}  活跃块 {:>3}/{}  移动 {:>6}  反应 {:>5}  生物 {:>3}  体型 {:>2}-{:<2}  飘 {:>2}  杀手 {:>2}  落 {:>3}  能量 {:>6}  代数 {:>3}(均 {:.1})  |  {}",
                i + 1,
                args.ticks,
                world.hash(),
                s.active_chunks,
                world.chunk_count(),
                s.moved,
                s.reacted,
                l.organisms,
                l.min_cells,
                l.max_cells,
                l.stranded,
                l.killers,
                l.fell,
                l.energy,
                l.max_generation,
                l.avg_generation,
                census_line(&world.census()),
            );
        }
    }
    let elapsed = start.elapsed();

    println!("最终状态:");
    print_state(&world, "");
    if !elapsed.is_zero() && args.ticks > 0 {
        let ms_per_tick = elapsed.as_secs_f64() * 1000.0 / args.ticks as f64;
        let avg_active = stats.active_chunk_sum as f64 / stats.ticks as f64;
        let avg_visited = stats.visited_sum as f64 / stats.ticks as f64;
        println!(
            "  用时      {:?}  ·  {ms_per_tick:.4} ms/tick  ·  平均活跃块 {avg_active:.1}/{}  ·  平均访问 {avg_visited:.0} 格  ·  工人 {}（峰值）",
            elapsed,
            world.chunk_count(),
            stats.workers_max
        );
        println!(
            "  累计      访问 {} 格 · 移动 {} 次 · 反应 {} 次 · 腐烂尸体 {} 格",
            stats.visited_sum, stats.moved_sum, stats.reacted_sum, stats.decayed_sum
        );
    }
    println!();
    print_delta(&census_before, &world.census());

    let l = life.stats();
    println!();
    if l.stranded > 0 {
        for o in life.organisms() {
            let cells: Vec<(i32, i32)> = o.footprint().collect();
            let ys: Vec<i32> = cells.iter().map(|c| c.1).collect();
            let (y0, y1) = (*ys.iter().min().unwrap(), *ys.iter().max().unwrap());
            let xs: Vec<i32> = cells.iter().map(|c| c.0).collect();
            let x0 = *xs.iter().min().unwrap();
            let surface = (0..world.height() as i32)
                .find(|&y| world.get(x0 as isize, y as isize) == MaterialKind::Water)
                .unwrap_or(-1);
            if y0 < surface {
                println!(
                    "  [飘] id {} 身体 y {}-{} 该列水面 {} 细胞 {} 能量 {}",
                    o.id,
                    y0,
                    y1,
                    surface,
                    o.cell_count(),
                    o.energy
                );
            }
        }
    }
    println!(
        "生命层  生物 {} · 细胞 {} · 体型 {}-{} 格 · 深度 {}-{} · 移动 左{} 右{} 上{} 下{} · 能量 {} · 飘在天上的 {} 个 · 杀手 {} 枚 · 食物 {} 格",
        l.organisms,
        l.cells,
        l.min_cells,
        l.max_cells,
        l.shallowest,
        l.deepest,
        l.moved[0],
        l.moved[1],
        l.moved[2],
        l.moved[3],
        l.energy,
        l.stranded,
        l.killers,
        life.food_cells(&world)
    );
    println!(
        "谱系    最大代数 {} · 平均代数 {:.1} · 与初代基因距离 {:.3}",
        l.max_generation, l.avg_generation, l.gene_distance
    );
    let audit = life.audit(&world);
    let mt = life.moved_total();
    let total: u64 = mt.iter().sum();
    if total > 0 {
        println!("  重力累计下落 {} 次", life.fell_total());
        println!(
            "移动累计  左 {} 右 {} 上 {} 下 {}  （垂直占 {:.1}%）",
            mt[0],
            mt[1],
            mt[2],
            mt[3],
            (mt[2] + mt[3]) as f64 * 100.0 / total as f64
        );
    }
    println!(
        "能量账  注入 {} · 消耗 {} · 残骸 {} · 生物 {} · 差额 {}{}",
        audit.injected,
        audit.dissipated,
        audit.remains,
        audit.organisms,
        audit.leak(),
        if audit.is_balanced() {
            "（守恒）"
        } else {
            "  ← 不守恒！"
        }
    );

    if args.ascii {
        println!();
        // 相机由地貌自己定位：对准最高的那座岛，让天空、岛屿轮廓和水体边界
        // 同时入画。早先是硬编码"贴地表往上挪四分之三"，那只在"地面在下三分之一"
        // 这一种地形下成立；换成海洋 + 岛屿之后，那个公式会把视野停在空水里。
        let view_height = ASCII_VIEW_HEIGHT.min(world.height());
        let view_width = ASCII_VIEW_WIDTH.min(world.width());
        let mut camera = Camera::fit_bottom_left(&world, view_width, view_height);
        let target_x = layout.focus_left(view_width);
        let target_y = layout.focus_top(view_height);
        camera.pan(
            &world,
            target_x as isize - camera.x as isize,
            target_y as isize - camera.y as isize,
        );

        let mut renderer = AsciiRenderer::new(camera);
        if let Err(e) = renderer.render(&world) {
            eprintln!("ASCII 渲染失败: {e}");
            return ExitCode::FAILURE;
        }
        println!(
            "ASCII 视图（{view_width}x{view_height}，对准最高的岛 x={}，水面线 y={}，渲染器 = {}）:",
            layout.peak_x(),
            layout.waterline,
            renderer.name()
        );
        println!("{}", renderer.frame());
    }

    ExitCode::SUCCESS
}
