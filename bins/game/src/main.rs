//! Vivarium 图形前端。
//!
//! ## 状态：M0 第一步已完成，第二步待接入
//!
//! 目前这个 bin 只做两件事：
//!
//! 1. 报告渲染后端状态（[`PixelRenderer`] 尚未初始化）；
//! 2. 给出一个**能立刻用的替代方案**——用 ASCII 视图跑同样的模拟。
//!
//! 之所以不做成"启动就 panic 的占位符"，是因为一个能跑出东西的降级路径，
//! 比一句"尚未实现"有用得多：你现在就能看到世界长什么样。

use std::process::ExitCode;

use vivarium_core::{MaterialKind, Rng, World};
use vivarium_render::ascii::{AsciiRenderer, Camera};
use vivarium_render::pixel::PixelRenderer;
use vivarium_render::Renderer;

/// 降级预览用的小世界。
const PREVIEW_SIZE: usize = 128;
const PREVIEW_TICKS: u64 = 60;
const PREVIEW_VIEW: usize = 100;

fn main() -> ExitCode {
    let pixel = PixelRenderer::new();
    println!("Vivarium — 图形前端");
    println!("渲染后端 {} 就绪状态: {}", pixel.name(), pixel.is_initialized());
    println!();
    println!("wgpu 渲染器尚未接入（M0 第二步）。");
    println!("在那之前，下面用 ASCII 渲染器跑一个降级预览——");
    println!("它能验证同一套模拟内核，只是画得不好看。");
    println!();

    let mut world = match World::new(PREVIEW_SIZE, PREVIEW_SIZE) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("无法创建世界: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut rng = Rng::new(7);
    seed_preview(&mut world, &mut rng);

    let mut renderer = AsciiRenderer::new(Camera::fit_bottom_left(&world, PREVIEW_VIEW, 30));

    // 跑一段，看看世界有没有在动。
    let hash_before = world.hash();
    for _ in 0..PREVIEW_TICKS {
        world.step();
    }
    let hash_after = world.hash();

    if let Err(e) = renderer.render(&world) {
        eprintln!("渲染失败: {e}");
        return ExitCode::FAILURE;
    }

    println!(
        "{}x{} 世界，跑完 {} tick，渲染器 = {}",
        world.width(),
        world.height(),
        PREVIEW_TICKS,
        renderer.name()
    );
    println!("hash {hash_before:#018x} -> {hash_after:#018x}");
    println!();
    println!("{}", renderer.frame());
    println!();
    println!("下一步：接入 wgpu，把这份 ASCII 换成真正的像素画面。");

    ExitCode::SUCCESS
}

/// 铺一小片地形，让预览有东西可看。
fn seed_preview(world: &mut World, rng: &mut Rng) {
    let w = world.width();
    let h = world.height();
    let floor_top = h - h / 6;

    world.fill_rect(0, floor_top as isize, w, h - floor_top, MaterialKind::Stone);

    let mut surface = floor_top as isize;
    for x in 0..w as isize {
        match rng.gen_range_u32(0, 3) {
            0 => surface = (surface + 1).min(floor_top as isize + 6),
            1 => surface = (surface - 1).max(1),
            _ => {}
        }
        let thickness = rng.gen_range_usize(1, 4);
        world.fill_rect(x, surface, 1, thickness, MaterialKind::Sand);
    }

    let pool_x = (w / 4) as isize;
    world.fill_rect(pool_x, (floor_top as isize - 10).max(1), w / 5, 10, MaterialKind::Water);

    for _ in 0..(w / 6) {
        let x = rng.gen_range_usize(0, w) as isize;
        let y = rng.gen_range_usize(0, (floor_top as isize - 20).max(1) as usize) as isize;
        world.fill_rect(x, y, 2, 2, MaterialKind::Gas);
    }
}
