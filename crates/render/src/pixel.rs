//! 像素渲染器（wgpu）。
//!
//! ## 状态：M0 第二步，尚未接入
//!
//! 接入后这里会持有：
//!
//! - `wgpu::Surface` / `Device` / `Queue`
//! - 一张与世界同尺寸的 RGBA 纹理（512² 时约 1 MiB）
//! - 一个全屏四边形管线，把纹理采样到 surface
//! - **脏块增量上传**：只更新动过的区域，而不是每帧重传整张纹理
//!
//! ## 为什么现在留一个空实现而不是只写 TODO 注释
//!
//! 因为 `bins/game` 需要能编译，而且 [`crate::Renderer`] 的"双实现"关系
//! 应当在类型系统里现在就成立——这样接入 wgpu 时，接口不用改，
//! 只需把内部填满。

use vivarium_core::World;

use crate::{RenderError, Renderer};

/// wgpu 像素渲染器。
///
/// 目前 `render` 一律返回 [`RenderError::NotInitialized`]；
/// 图形前端在接入 wgpu 之前，请改用 [`crate::ascii::AsciiRenderer`]。
#[derive(Debug, Default)]
pub struct PixelRenderer {
    initialized: bool,
}

impl PixelRenderer {
    /// 创建未初始化的渲染器。真正的 wgpu 初始化会在 `init` 中完成。
    pub const fn new() -> Self {
        Self {
            initialized: false,
        }
    }

    /// 渲染后端是否已就绪。
    pub const fn is_initialized(&self) -> bool {
        self.initialized
    }
}

impl Renderer for PixelRenderer {
    fn resize(&mut self, _width: u32, _height: u32) {
        // 纹理会在 wgpu 接入后随尺寸重建。
    }

    fn render(&mut self, _world: &World) -> Result<(), RenderError> {
        Err(RenderError::NotInitialized)
    }

    fn name(&self) -> &'static str {
        "pixel(wgpu)"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_not_initialized_before_wgpu_is_wired() {
        let mut r = PixelRenderer::new();
        let w = World::new(64, 64).unwrap();
        assert!(!r.is_initialized());
        assert_eq!(r.render(&w), Err(RenderError::NotInitialized));
    }
}
