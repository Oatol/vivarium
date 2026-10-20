//! ASCII 渲染器。
//!
//! 零 GPU 依赖，两个用途：
//!
//! 1. 在终端里直接看模拟：`cargo run -p vivarium-sim -- --ascii`
//! 2. 作为 headless 调试的主力视图——**不需要窗口就能判断"世界到底有没有在动"**。
//!
//! 第二点是它真正的价值。当你跑一万代长测时，出问题的往往不是"画面不对"，
//! 而是"某个区域的材质卡住不动了"，而 ASCII 视图是最快能看到这件事的东西。

use vivarium_core::World;

use crate::{palette, RenderError, Renderer};

/// 相机视图：世界坐标下的一个矩形区域。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Camera {
    /// 视图左上角的世界 X 坐标。
    pub x: usize,
    /// 视图左上角的世界 Y 坐标。
    pub y: usize,
    /// 视图宽（字符列数）。
    pub width: usize,
    /// 视图高（字符行数）。
    pub height: usize,
}

impl Camera {
    /// 直接构造。
    pub const fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// 覆盖世界左上角，按世界尺寸裁剪。
    pub fn fit_top_left(world: &World, width: usize, height: usize) -> Self {
        Self::new(0, 0, width.min(world.width()), height.min(world.height()))
    }

    /// 覆盖世界左下角，按世界尺寸裁剪。
    ///
    /// 地形通常堆在底部，所以这是"想立刻看到东西"时最常用的相机位置——
    /// 贴顶的话你只会看到一片空天。
    pub fn fit_bottom_left(world: &World, width: usize, height: usize) -> Self {
        let w = width.min(world.width());
        let h = height.min(world.height());
        Self::new(0, world.height() - h, w, h)
    }

    /// 覆盖世界中心，按世界尺寸裁剪。
    pub fn fit_center(world: &World, width: usize, height: usize) -> Self {
        let w = width.min(world.width());
        let h = height.min(world.height());
        Self::new((world.width() - w) / 2, (world.height() - h) / 2, w, h)
    }

    /// 把视图平移 `dx` / `dy`，并夹在 `world` 边界内。
    pub fn pan(&mut self, world: &World, dx: isize, dy: isize) {
        let max_x = world.width().saturating_sub(self.width) as isize;
        let max_y = world.height().saturating_sub(self.height) as isize;
        self.x = (self.x as isize + dx).clamp(0, max_x.max(0)) as usize;
        self.y = (self.y as isize + dy).clamp(0, max_y.max(0)) as usize;
    }
}

/// 把世界画成文本的渲染器。
///
/// 内部持有一个可复用的 `String` 缓冲——每帧重新分配会让长跑时的内存曲线变得难看，
/// 而 SPEC 的验收里有一条"稳态无持续增长"。
#[derive(Debug, Clone)]
pub struct AsciiRenderer {
    buffer: String,
    camera: Camera,
}

impl AsciiRenderer {
    /// 用给定相机创建渲染器。
    pub fn new(camera: Camera) -> Self {
        Self {
            buffer: String::new(),
            camera,
        }
    }

    /// 覆盖世界中心的相机，尺寸自动裁剪。
    pub fn centered_on(world: &World, width: usize, height: usize) -> Self {
        Self::new(Camera::fit_center(world, width, height))
    }

    /// 当前相机。
    pub const fn camera(&self) -> Camera {
        self.camera
    }

    /// 替换相机。
    pub fn set_camera(&mut self, camera: Camera) {
        self.camera = camera;
    }

    /// 上一帧的文本内容。
    pub fn frame(&self) -> &str {
        &self.buffer
    }

    /// 上一帧的行数。
    pub fn line_count(&self) -> usize {
        self.buffer.lines().count()
    }
}

impl Renderer for AsciiRenderer {
    fn resize(&mut self, width: u32, height: u32) {
        self.camera.width = self.camera.width.min(width as usize);
        self.camera.height = self.camera.height.min(height as usize);
        self.camera.x = self.camera.x.min(width as usize);
        self.camera.y = self.camera.y.min(height as usize);
    }

    fn render(&mut self, world: &World) -> Result<(), RenderError> {
        self.buffer.clear();

        let cols = self
            .camera
            .width
            .min(world.width().saturating_sub(self.camera.x));
        let rows = self
            .camera
            .height
            .min(world.height().saturating_sub(self.camera.y));

        // 预留容量，避免长跑时反复扩容。
        self.buffer.reserve(cols * rows + rows);

        for row in 0..rows {
            let y = (self.camera.y + row) as isize;
            for col in 0..cols {
                let x = (self.camera.x + col) as isize;
                // 一格非材质即细胞（SPEC §2.1）：先问有没有主人，再问材质。
                // 顺序不能反——细胞格的材质字段是 `Empty`，反着问就永远看不见生物。
                if world.is_occupied(x, y) {
                    self.buffer.push(palette::CELL_GLYPH);
                } else {
                    self.buffer.push(palette::glyph(world.get(x, y)));
                }
            }
            self.buffer.push('\n');
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        "ascii"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivarium_core::{CellOwner, MaterialKind};

    fn world_with_stone(x: isize, y: isize) -> World {
        let mut w = World::new(64, 64).unwrap();
        w.set(x, y, MaterialKind::Stone);
        w
    }

    #[test]
    fn organisms_render_as_at_signs() {
        let mut w = World::new(64, 64).unwrap();
        w.set(20, 5, MaterialKind::Water);
        w.set_owner(20, 5, CellOwner::new(1, 0, 0));
        let mut r = AsciiRenderer::new(Camera::new(20, 5, 4, 2));
        r.render(&w).unwrap();
        let first = r.frame().lines().next().unwrap();
        assert_eq!(
            first.chars().next().unwrap(),
            palette::CELL_GLYPH,
            "细胞格的材质是水，但画出来必须是生物"
        );
    }

    #[test]
    fn renders_exact_dimensions() {
        let w = World::new(64, 64).unwrap();
        let mut r = AsciiRenderer::new(Camera::new(0, 0, 10, 4));
        r.render(&w).unwrap();
        assert_eq!(r.line_count(), 4);
        for line in r.frame().lines() {
            assert_eq!(line.chars().count(), 10);
        }
    }

    #[test]
    fn camera_offset_is_respected() {
        let w = world_with_stone(20, 5);
        let mut r = AsciiRenderer::new(Camera::new(20, 5, 4, 2));
        r.render(&w).unwrap();
        let first = r.frame().lines().next().unwrap();
        assert_eq!(first.chars().next().unwrap(), '#');
    }

    #[test]
    fn out_of_range_camera_is_clamped_not_panicking() {
        let w = World::new(64, 64).unwrap();
        let mut r = AsciiRenderer::new(Camera::new(60, 60, 32, 32));
        r.render(&w).unwrap();
        // 世界只有 64 宽，从 60 开始最多画 4 列。
        for line in r.frame().lines() {
            assert!(line.chars().count() <= 4);
        }
    }

    #[test]
    fn render_does_not_mutate_world() {
        // 这条直接对应 SPEC 的验收断言：渲染不得改变模拟结果。
        let mut w = World::new(128, 128).unwrap();
        w.fill_rect(0, 0, 40, 40, MaterialKind::Water);
        w.set(70, 70, MaterialKind::Fire);
        let before = w.hash();

        let mut r = AsciiRenderer::new(Camera::new(0, 0, 128, 128));
        r.render(&w).unwrap();
        r.render(&w).unwrap();

        assert_eq!(w.hash(), before);
    }

    #[test]
    fn bottom_left_camera_sits_on_the_floor() {
        let w = World::new(512, 512).unwrap();
        let c = Camera::fit_bottom_left(&w, 120, 40);
        assert_eq!(c.x, 0);
        assert_eq!(c.y, 512 - 40);
        assert_eq!((c.width, c.height), (120, 40));
    }

    #[test]
    fn bottom_left_camera_clamps_to_small_world() {
        // 世界尺寸必须是 CHUNK_SIZE 的整数倍，所以这里用 64×64 而不是 64×32。
        // 请求 120×40 的视图会被夹到世界的 64 宽，高度保持 40，
        // 于是视图贴住左下角：x=0，y=64-40=24。
        let w = World::new(64, 64).unwrap();
        let c = Camera::fit_bottom_left(&w, 120, 40);
        assert_eq!((c.x, c.y), (0, 24));
        assert_eq!((c.width, c.height), (64, 40));
        // 视图底边必须正好落在世界底边上。
        assert_eq!(c.y + c.height, w.height());
    }

    #[test]
    fn centered_camera_lands_in_the_middle() {
        let w = World::new(512, 512).unwrap();
        let c = Camera::fit_center(&w, 120, 40);
        assert_eq!(c.x, (512 - 120) / 2);
        assert_eq!(c.y, (512 - 40) / 2);
        assert_eq!(c.width, 120);
        assert_eq!(c.height, 40);
    }

    #[test]
    fn centered_camera_clamps_to_small_world() {
        let w = World::new(64, 64).unwrap();
        let c = Camera::fit_center(&w, 120, 40);
        assert_eq!(c.x, 0);
        assert_eq!(c.width, 64);
    }

    #[test]
    fn pan_stays_inside_the_world() {
        let w = World::new(512, 512).unwrap();
        let mut c = Camera::new(0, 0, 120, 40);
        c.pan(&w, -50, -50);
        assert_eq!((c.x, c.y), (0, 0));
        c.pan(&w, 10_000, 10_000);
        assert_eq!((c.x, c.y), (512 - 120, 512 - 40));
    }
}
