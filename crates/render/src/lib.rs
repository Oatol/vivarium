//! # vivarium-render
//!
//! 渲染层。**只读世界，绝不写回。**
//!
//! ## 契约：切换渲染器不得改变模拟结果
//!
//! 这条验收断言由两个设计共同保证：
//!
//! 1. [`Renderer`] 的方法只接收 `&World`——接口签名层面就杜绝了渲染器写回网格的可能；
//! 2. 两个渲染器共用同一份 [`palette`] 语义映射，视觉可以不同，语义必须一致。
//!
//! ## M0 状态
//!
//! | 渲染器 | 状态 |
//! |---|---|
//! | [`ascii::AsciiRenderer`] | ✅ 已完成。零 GPU 依赖，终端里能看，headless 调试主力 |
//! | [`pixel::PixelRenderer`] | ⏳ wgpu 实现，M0 第二步接入 |

pub mod ascii;
pub mod palette;
pub mod pixel;

use vivarium_core::World;

/// 渲染错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    /// 渲染目标尺寸与世界不匹配。
    SizeMismatch {
        expected: (u32, u32),
        actual: (u32, u32),
    },
    /// 渲染后端尚未初始化。
    NotInitialized,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch { expected, actual } => write!(
                f,
                "render target size mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::NotInitialized => write!(f, "renderer is not initialized"),
        }
    }
}

impl std::error::Error for RenderError {}

/// 渲染器统一接口。
///
/// 注意 `render` 接收的是 `&World` 而不是 `&mut World`。这不是随手写的——
/// SPEC 里"切换 Pixel / Ascii 渲染器后 10,000 tick 的网格哈希不变"这条断言，
/// 就靠这个签名在编译期兜底。
pub trait Renderer {
    /// 渲染目标尺寸变化时重建内部资源。
    fn resize(&mut self, width: u32, height: u32);

    /// 渲染一帧。
    fn render(&mut self, world: &World) -> Result<(), RenderError>;

    /// 渲染器名称，用于日志与验收断言。
    fn name(&self) -> &'static str;
}
