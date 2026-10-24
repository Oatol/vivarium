//! 像素渲染器（wgpu）。
//!
//! ## 分层
//!
//! 这个模块里有三块，职责严格分开：
//!
//! | 类型 | 需要 GPU | 职责 |
//! |---|---|---|
//! | [`PixelUploader`] | 否 | 世界网格 → RGBA 字节，按 64×64 块增量维护 |
//! | [`Blitter`] | 是 | 纹理上传 + 全屏四边形管线（**不含窗口**） |
//! | [`PixelRenderer`] | 是 | 在 [`Blitter`] 之上加 surface 与呈现 |
//!
//! 拆成这样的理由很实际：[`PixelUploader`] 才是容易写错的地方（行主序索引、
//! 块边界、增量判定），而它完全不碰显卡，所以能脱离 GPU 单元测试。
//! GPU 那两块尽量薄，薄到一眼能看完。
//!
//! ## 为什么按块上传，而不是每帧整张重传
//!
//! 512² 的 RGBA 纹理是 1 MiB。每帧重传在 60 FPS 下也就 60 MiB/s，
//! 现代 GPU 完全不在乎带宽——**真正贵的是 CPU 侧的调色板转换**：
//! 262144 个格子逐个查表、逐字节写入。
//!
//! 分块之后，静止的块连查表都省了。这与 SPEC 第 2.4 节的判断一致：
//! 落沙层的成本取决于**活跃格子比例**，不是网格大小。渲染侧同理。
//!
//! ## 一个顺带的好处
//!
//! [`CHUNK_SIZE`] 是 64，RGBA 是 4 字节，所以块内一行恰好 256 字节——
//! 正好等于 wgpu 的 `COPY_BYTES_PER_ROW_ALIGNMENT`。
//! 块上传天然满足行对齐，不需要手动补 padding。下面有一条编译期断言盯着这件事。

#[cfg(test)]
use vivarium_core::organelle::{Organelles, GAS};
use vivarium_core::{field, AirField, CellOwner, MaterialCell, ViewMode, World, CHUNK_SIZE};

use crate::{palette, RenderError, Renderer};

// ---------------------------------------------------------------------------
// letterbox
// ---------------------------------------------------------------------------

/// 相机：看世界的哪一块、放大多少。
///
/// `zoom = 1.0` 表示整个世界刚好铺满窗口（默认，生态视角）。
/// 想看细胞结构就得放大：512² 的世界在 1100 像素宽的窗口里只有
/// 2.1 像素/格，一个细胞是两个像素，谈不上"结构"；
/// `zoom = 8` 是 17 像素/格，这时候细胞才真的是一颗细胞。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// 视口中心（格坐标）。
    pub center: (f32, f32),
    /// 放大倍数。`1.0` = 整个世界，越大看得越近。
    pub zoom: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            center: (0.0, 0.0),
            zoom: 1.0,
        }
    }
}

/// 放大倍数的上下限。
///
/// 下限 1.0：再往外缩就会看到世界之外，而世界之外没有东西可看。
/// 上限 64.0：一格 130 多像素，再放大只是把同一颗细胞拉成马赛克。
pub const MIN_ZOOM: f32 = 1.0;
pub const MAX_ZOOM: f32 = 64.0;

impl Viewport {
    /// 新建相机，对准世界中心。
    pub fn centered_on(world: (u32, u32)) -> Self {
        Self {
            center: (world.0 as f32 * 0.5, world.1 as f32 * 0.5),
            zoom: 1.0,
        }
    }

    /// 视口覆盖多少格。
    pub fn view_size(&self, world: (u32, u32)) -> (f32, f32) {
        let zoom = self.zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        (world.0 as f32 / zoom, world.1 as f32 / zoom)
    }

    /// 把相机夹回世界里。
    ///
    /// 不夹的话会看到世界之外——那一片是背景色，看起来像"世界被裁掉了"，
    /// 而实际上只是相机飘出去了。
    pub fn clamped(self, world: (u32, u32)) -> Self {
        let (vw, vh) = self.view_size(world);
        let clamp = |c: f32, size: f32, view: f32| {
            if view >= size {
                size * 0.5
            } else {
                c.clamp(view * 0.5, size - view * 0.5)
            }
        };
        Self {
            center: (
                clamp(self.center.0, world.0 as f32, vw),
                clamp(self.center.1, world.1 as f32, vh),
            ),
            zoom: self.zoom.clamp(MIN_ZOOM, MAX_ZOOM),
        }
    }
}

/// letterbox 的结果。
///
/// 前两个字段是**给 shader 的**（裁剪空间缩放 / 平移），后三个是**给 CPU 的**
/// （世界实际画在屏幕上的哪一块）。两者必须由同一次计算得出：
/// 分开算就会漂移，而漂移的症状是"点哪都差一点"——那种 bug 靠眼睛几乎找不到。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Letterbox {
    /// 裁剪空间缩放。
    scale: [f32; 2],
    /// 裁剪空间平移。
    offset: [f32; 2],
    /// 世界左上角在屏幕上的像素坐标。
    origin: [f32; 2],
    /// 世界在屏幕上的像素尺寸。
    drawn: [f32; 2],
}

/// blit 管线的 uniform。
///
/// 除了 letterbox，还带上相机视口与整张调色板。颜色从 CPU 搬到 GPU 之后，
/// "一格长什么样"就只有 shader 一个地方在回答——不会出现
/// "CPU 算的颜色和 shader 算的不一致"这种最难查的一类 bug。
///
/// 布局必须与 `blit.wgsl` 里的 `Scene` 逐字节一致：**560 字节**
/// （48 字节头部 + 32 × 16 字节调色板），每个 vec4 落在 16 的倍数上。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct SceneUniforms {
    scale: [f32; 2],
    offset: [f32; 2],
    view: [f32; 4],
    world: [f32; 2],
    px_per_cell: f32,
    /// 显示滤镜（[`ViewMode::as_u32`]）。0 材质 / 1 温度 / 2 光照 / 3 物种。
    view_mode: u32,
    palette: [[f32; 4]; palette::PALETTE_LEN],
}

impl SceneUniforms {
    fn new(
        world: (u32, u32),
        target: (u32, u32),
        viewport: Viewport,
        content: Option<[f32; 4]>,
        srgb_output: bool,
        view_mode: ViewMode,
    ) -> Self {
        let lb = letterbox(world, target, content);
        let vp = viewport.clamped(world);
        let (vw, vh) = vp.view_size(world);
        let px_per_cell = if vw > 0.0 { lb.drawn[0] / vw } else { 1.0 };
        Self {
            scale: lb.scale,
            offset: lb.offset,
            view: [vp.center.0 - vw * 0.5, vp.center.1 - vh * 0.5, vw, vh],
            world: [world.0 as f32, world.1 as f32],
            px_per_cell,
            view_mode: view_mode.as_u32(),
            palette: palette::scene_palette(srgb_output),
        }
    }
}

/// 计算 letterbox：把世界按原宽高比塞进目标矩形，多余的部分留黑边。
///
/// 世界是正方形而窗口是宽屏时，宁可左右留黑边，也不要把格子拉成非正方形——
/// 像素一旦不是方的，"一个格子"这个视觉单位就崩了，玩家再也数不清自己画了几格。
///
/// 任一维度为 0 时退化为不缩放。这不是防御性编程：窗口最小化时
/// 交换链尺寸真的会是 0。
fn letterbox(world: (u32, u32), target: (u32, u32), content: Option<[f32; 4]>) -> Letterbox {
    let (tw, th) = (target.0 as f32, target.1 as f32);
    if world.0 == 0 || world.1 == 0 || tw <= 0.0 || th <= 0.0 {
        return Letterbox {
            scale: [1.0, 1.0],
            offset: [0.0, 0.0],
            origin: [0.0, 0.0],
            drawn: [tw, th],
        };
    }

    // 可用区域：界面把它算出来交给渲染器（中央那块）。
    //
    // 世界**只画在这一块里**，而不是整窗铺满再让面板盖上去——
    // 那样一来面板底下藏着一半世界，二来"点哪儿是哪格"的换算
    // 会和眼睛看到的位置差一大截。前者是观感问题，后者是可用性问题。
    let [cx0, cy0, cx1, cy1] = content.unwrap_or([0.0, 0.0, tw, th]);
    let cw = (cx1 - cx0).max(1.0);
    let ch = (cy1 - cy0).max(1.0);

    // 每格多少像素：取两个方向里更紧的那个，保证整张世界都放得下。
    let s = (cw / world.0 as f32).min(ch / world.1 as f32);
    let drawn = [world.0 as f32 * s, world.1 as f32 * s];
    // 在可用区域里居中。
    let origin = [cx0 + (cw - drawn[0]) * 0.5, cy0 + (ch - drawn[1]) * 0.5];

    Letterbox {
        scale: [drawn[0] / tw, drawn[1] / th],
        // 裁剪空间里 y 向上，屏幕坐标 y 向下，所以纵向要取反。
        offset: [
            (origin[0] + drawn[0] * 0.5) / tw * 2.0 - 1.0,
            1.0 - (origin[1] + drawn[1] * 0.5) / th * 2.0,
        ],
        origin,
        drawn,
    }
}

// ---------------------------------------------------------------------------
// 统计
// ---------------------------------------------------------------------------

/// 渲染统计。
///
/// 存在的理由是让"增量上传到底有没有生效"这件事**可观测**。
/// 少了它，一个静默退化成全量上传的实现你只能靠猜——
/// 而画面看起来是一模一样的。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PixelStats {
    /// 成功画出的帧数。
    pub drawn_frames: u64,
    /// 因为交换链暂时不可用而跳过的帧数。**这不是错误**，下一帧通常就好了。
    pub skipped_frames: u64,
    /// 上一帧实际上传的块数。
    pub last_uploaded_chunks: usize,
    /// 累计上传的块数。稳态下它的增速应当远低于"帧数 × 总块数"。
    pub total_uploaded_chunks: u64,
}

/// 一帧没能画成的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSkip {
    /// 等交换链超时。
    Timeout,
    /// 窗口被遮挡（最小化、被别的窗口盖住）。
    Occluded,
    /// 交换链配置过期，已重新配置。
    Outdated,
    /// 交换链丢失，已尝试重新配置。
    Lost,
}

impl std::fmt::Display for FrameSkip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Timeout => "交换链超时",
            Self::Occluded => "窗口被遮挡",
            Self::Outdated => "交换链配置过期",
            Self::Lost => "交换链丢失",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// 上传侧（纯 CPU，可测）
// ---------------------------------------------------------------------------

/// 把世界网格转成 RGBA 像素，并按块增量维护。
///
/// **不持有任何 GPU 资源。** 这是刻意的：这个结构里全是索引算术，
/// 最容易写错，也最需要测试，而测试不该要求一台有显卡的机器。
/// 语义键的最高位：这一格是细胞而不是材质。
///
/// 用最高位而不是"organism != 0"来判断，是因为 shader 拿到的是一个 u32，
/// 没有第二个字段告诉它"这是不是生物"。材质 id 只用到低 8 位，
/// 所以最高位永远空着，拿来做标志位不要钱。
pub const CELL_BIT: u32 = 0x8000_0000;

/// 一个格子在画面上"长什么样"的紧凑编码。
///
/// - 无主：材质 id（0..7）
/// - 有主：`0x1_0000 | organism`
///
/// 两者不可能撞号，所以"有没有生物"这件事不需要额外的标志位，
/// 直接比这个 u32 就够了。用 u32 而不是 u8 是因为 organism 编号会超过 255，
/// 截断之后"第 300 条鱼和第 44 条鱼"会被当成同一个视觉状态，移动时就不重画。
#[inline]
fn visual_key(cells: &[MaterialCell], owners: &[CellOwner], i: usize) -> u32 {
    let owner = owners[i];
    if owner.is_some() {
        // 最高位标记"这一格是细胞"，**低 13 位是细胞器位掩码**，
        // 中间 18 位是编号。
        //
        // v0.41 换过一次布局：原来低 8 位放的是 `CellType`，而颜色现在
        // 按细胞器混（叶绿体绿 + 气囊淡蓝 = 薄荷），那个有损的"代表类型"
        // 不够用了。位宽是这样分的：
        //
        // | 位 | 用途 |
        // |---|---|
        // | 31 | 这一格是细胞 |
        // | 13..30 | 有机体编号（18 位 = 26 万，够一个会话用） |
        // | 0..12 | 细胞器（13 位，`Organelles` 正好用满） |
        //
        // **编号从 23 位缩到 18 位是有代价的**：编号超过 262143 会回绕，
        // 于是两个不同个体可能撞上同一个视觉编号、被画成同一具身体。
        // 现实里到不了（8000 tick 几百次出生），而且撞了也只是视觉上的。
        CELL_BIT | (owner.organism << ORGANISM_SHIFT) | owner.organelles as u32
    } else {
        cells[i].kind.as_u8() as u32
    }
}

/// 有机体编号在视觉键里的起始位。
///
/// 必须与 `blit.wgsl` 里的 `ORGANISM_SHIFT` 一致。
pub const ORGANISM_SHIFT: u32 = 13;

#[derive(Debug, Clone)]
pub struct PixelUploader {
    width: usize,
    height: usize,
    chunk_cols: usize,
    chunk_rows: usize,
    /// 上一次上传时每个格子的**视觉键**。
    ///
    /// 视觉键 = 材质 id，或者"这一格被某个有机体占了"。
    /// 之所以不能只看材质：细胞格的材质字段永远是 `Empty`，
    /// 只比材质的话，一条鱼游过水面时渲染器会认为什么都没变，鱼根本不会出现。
    ///
    /// 初值填 `u32::MAX`——它不是任何合法视觉键，于是"首帧整张都是脏的"
    /// 这件事不需要额外的标志位来记，比较结果自然就全为真。
    shadow: Vec<u32>,
    /// 本帧需要重新上传的块掩码。
    dirty: Vec<bool>,
    dirty_count: usize,
    /// 单个块的紧凑像素缓冲。复用它，避免每帧分配。
    chunk_pixels: Vec<u8>,
}

impl PixelUploader {
    /// 单个块的一行占多少字节。
    ///
    /// 恰好是 256，也就是 wgpu 要求的行对齐粒度。见下面的编译期断言。
    pub const fn chunk_bytes_per_row() -> u32 {
        (CHUNK_SIZE * 4) as u32
    }

    /// 单个块的像素字节数。
    pub const fn chunk_byte_len() -> usize {
        CHUNK_SIZE * CHUNK_SIZE * 4
    }

    /// 为给定世界建立上传器。首帧会全量上传一次。
    pub fn new(world: &World) -> Self {
        let width = world.width();
        let height = world.height();
        let chunk_cols = world.chunk_cols();
        let chunk_rows = world.chunk_rows();
        Self {
            width,
            height,
            chunk_cols,
            chunk_rows,
            shadow: vec![u32::MAX; width * height],
            dirty: vec![false; chunk_cols * chunk_rows],
            dirty_count: 0,
            chunk_pixels: vec![0u8; Self::chunk_byte_len()],
        }
    }

    /// 世界宽度（格）。
    pub const fn width(&self) -> usize {
        self.width
    }

    /// 世界高度（格）。
    pub const fn height(&self) -> usize {
        self.height
    }

    /// 这一块实际有多少**列**（残块小于 `CHUNK_SIZE`）。
    ///
    /// ## 为什么必须夹
    ///
    /// `chunk_cols` / `chunk_rows` 是 `div_ceil`，所以最后一块可能只覆盖
    /// 世界的一部分。而下面两个循环原来写的是 `0..CHUNK_SIZE`——于是
    /// **只要世界的宽或高不是 64 的倍数，它就会读到世界外面**。
    ///
    /// 这不是理论问题：预设里的"大 768×728"（728 不是 64 的倍数）
    /// 一点就 panic（`index out of bounds: the len is 559104 but the index
    /// is 559104`——那正好是第 728 行的第一格）。而滑块的取值范围
    /// （宽 20..600、高 20..450）里**绝大多数尺寸都踩这条**，
    /// 只是 256 / 512 恰好是 64 的倍数，所以一直没被发现。
    #[inline]
    pub const fn chunk_w(&self, cx: usize) -> usize {
        let rest = self.width - cx * CHUNK_SIZE;
        if rest < CHUNK_SIZE {
            rest
        } else {
            CHUNK_SIZE
        }
    }

    /// 这一块实际有多少**行**。见 [`Self::chunk_w`]。
    #[inline]
    pub const fn chunk_h(&self, cy: usize) -> usize {
        let rest = self.height - cy * CHUNK_SIZE;
        if rest < CHUNK_SIZE {
            rest
        } else {
            CHUNK_SIZE
        }
    }

    /// 分块列数。
    pub const fn chunk_cols(&self) -> usize {
        self.chunk_cols
    }

    /// 分块行数。
    pub const fn chunk_rows(&self) -> usize {
        self.chunk_rows
    }

    /// 上一轮 [`Self::prepare`] 标出的脏块数。
    pub const fn dirty_chunk_count(&self) -> usize {
        self.dirty_count
    }

    /// 扫描一遍网格，标出这一帧需要重新上传的块，返回脏块数。
    ///
    /// 块内只要有一格变了就整块重传。粒度够细（64×64），
    /// 而"逐格比较"在连续内存上跑得比直觉快得多——它是一条能自动向量化的
    /// 字节比较循环，512² 全扫也就几十微秒。
    pub fn prepare(&mut self, world: &World) -> usize {
        debug_assert_eq!(
            (world.width(), world.height()),
            (self.width, self.height),
            "世界尺寸变了，uploader 必须重建"
        );

        let mut dirty_count = 0usize;
        for cy in 0..self.chunk_rows {
            for cx in 0..self.chunk_cols {
                let changed = self.chunk_changed(world, cx, cy);
                self.dirty[cy * self.chunk_cols + cx] = changed;
                if changed {
                    dirty_count += 1;
                }
            }
        }
        self.dirty_count = dirty_count;
        dirty_count
    }

    /// 本块是否有格子与上次上传时不同。
    fn chunk_changed(&self, world: &World, cx: usize, cy: usize) -> bool {
        let cells = world.cells();
        let owners = world.owners();
        for row in 0..self.chunk_h(cy) {
            let base = (cy * CHUNK_SIZE + row) * self.width + cx * CHUNK_SIZE;
            for col in 0..self.chunk_w(cx) {
                let i = base + col;
                if self.shadow[i] != visual_key(cells, owners, i) {
                    return true;
                }
            }
        }
        false
    }

    /// 本块是否需要在这次渲染里上传。
    #[inline]
    pub fn is_dirty_chunk(&self, cx: usize, cy: usize) -> bool {
        self.dirty[cy * self.chunk_cols + cx]
    }

    /// 重新生成一个块的紧凑像素，同时更新影子副本。
    ///
    /// 返回的切片可以**直接**喂给 `Queue::write_texture`：行是紧挨着的，
    /// 行距就是 [`Self::chunk_bytes_per_row`]。
    ///
    /// 注意它会顺带更新影子副本，所以同一个块不要在一次 `prepare` 之后
    /// 反复调用——第二次会算出一模一样的内容，纯属浪费。
    pub fn build_chunk(&mut self, world: &World, cx: usize, cy: usize) -> &[u8] {
        let cells = world.cells();
        let owners = world.owners();
        let (cw, ch) = (self.chunk_w(cx), self.chunk_h(cy));
        for row in 0..ch {
            // 行距**永远是** `CHUNK_SIZE`（也就是 256 字节），哪怕这一块只有
            // 40 列——`write_texture` 要求 `bytes_per_row` 是 256 的倍数，
            // 而 `extent.width` 才是"真正拷多少列"。两者不能混用。
            let mut out = row * CHUNK_SIZE * 4;
            let base = (cy * CHUNK_SIZE + row) * self.width + cx * CHUNK_SIZE;
            for col in 0..cw {
                let i = base + col;
                // 上传的是**语义键**，不是颜色。颜色、形状、膜的轮廓光
                // 全在 shader 里算——一格一像素的纹理画不出圆，
                // 而放大之后一个格子要占十几个像素，那些像素必须有内容可画。
                let key = visual_key(cells, owners, i);
                self.chunk_pixels[out..out + 4].copy_from_slice(&key.to_ne_bytes());
                out += 4;
                self.shadow[i] = key;
            }
        }
        &self.chunk_pixels
    }
}

// 块上传的整条链路都建立在"一行 256 字节"之上：`bytes_per_row` 用它，
// `chunk_pixels` 的布局也用它。哪天真去改 CHUNK_SIZE，这条断言会先炸，
// 而不是等到画面上出现一条斜向错位的纹路。
const _: () = assert!(PixelUploader::chunk_bytes_per_row() == wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);

// ---------------------------------------------------------------------------
// GPU 侧：blit 管线（不含窗口）
// ---------------------------------------------------------------------------

/// 世界纹理 + blit 管线。
///
/// **不含 surface**，所以可以在离屏纹理上单独跑起来——这正是测试里做的事。
/// 拆出来的另一个好处是 [`PixelRenderer`] 只剩下"管窗口"这一件事。
struct Blitter {
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// surface 的输出格式。管线必须与它一致。
    output_format: wgpu::TextureFormat,
    /// 世界纹理格式。
    ///
    /// 刻意跟随 surface 的 sRGB 属性：如果纹理是 sRGB 而 surface 不是
    /// （或反过来），颜色会被转换两次，画面会明显发灰或过暗。
    world_format: wgpu::TextureFormat,
    bind_group_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    uniforms: wgpu::Buffer,

    texture: Option<wgpu::Texture>,
    view: Option<wgpu::TextureView>,
    /// 场纹理：`Rg32Float`，尺寸是**粗网格**（世界 / `AIR_CELL`）。
    field_texture: Option<wgpu::Texture>,
    field_view: Option<wgpu::TextureView>,
    field_size: (u32, u32),
    /// 上一次上传的场版本号。相同就跳过上传。
    field_revision: u64,
    bind_group: Option<wgpu::BindGroup>,
    uploader: Option<PixelUploader>,
    world_size: (u32, u32),
    viewport: Viewport,
    /// 世界该画在屏幕上的哪一块（物理像素 `x0,y0,x1,y1`）。`None` = 整窗。
    ///
    /// 界面层每帧把它算出来交进来（中央那块矩形）。渲染器不猜——
    /// 猜出来的值一旦和面板尺寸对不上，"点哪儿是哪格"就会整体偏。
    content: Option<[f32; 4]>,
    /// 当前显示滤镜。
    view_mode: ViewMode,
    stats: PixelStats,
}

impl std::fmt::Debug for Blitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blitter")
            .field("output_format", &self.output_format)
            .field("world_format", &self.world_format)
            .field("world_size", &self.world_size)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl Blitter {
    /// 建立管线。这里会编译 WGSL——着色器写错的话，错误在这一步就暴露。
    fn new(device: wgpu::Device, queue: wgpu::Queue, output_format: wgpu::TextureFormat) -> Self {
        // 语义纹理：每格一个 u32。不再跟随 surface 的 sRGB 属性——
        // 这里存的是编号，不是颜色，颜色是 shader 查表算出来的。
        let world_format = wgpu::TextureFormat::R32Uint;

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vivarium-blit-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    // 顶点用 scale/offset，片段用视口与调色板——
                    // 同一个 uniform 两个阶段都要读。
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        // 必须写真实的 uniform 大小。写小了 wgpu 会认为
                        // 着色器要读的数组越界，直接拒绝建管线。
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<SceneUniforms>() as u64,
                        ),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        // 整数纹理不能被采样器过滤，只能 textureLoad。
                        // 这正好是我们要的：最近邻、不插值、格边界绝对清晰。
                        sample_type: wgpu::TextureSampleType::Uint,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        // 场纹理：r = 光照，g = 温度（0..1）。
                        //
                        // `filterable: false` 是因为 `Rg32Float` 在核心 WebGPU 里
                        // 不可过滤。这不是限制：着色器自己用四次 `textureLoad`
                        // 做双线性插值，比采样器更可控，而且粗网格的插值
                        // 本来就该由我们自己定义。
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::include_wgsl!("../shaders/blit.wgsl"));

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vivarium-blit-pipeline-layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            // wgpu 30 把 push constant 换成了 immediate data。我们一个字节都不用。
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vivarium-blit-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                // 顶点由 vertex_index 算出来，不需要顶点缓冲。
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: output_format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vivarium-blit-uniforms"),
            size: std::mem::size_of::<SceneUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            device,
            queue,
            output_format,
            world_format,
            bind_group_layout,
            pipeline,
            uniforms,
            texture: None,
            view: None,
            field_texture: None,
            field_view: None,
            field_size: (0, 0),
            field_revision: u64::MAX,
            bind_group: None,
            uploader: None,
            world_size: (0, 0),
            viewport: Viewport::default(),
            content: None,
            view_mode: ViewMode::Material,
            stats: PixelStats::default(),
        }
    }

    /// 世界纹理占多少字节。
    fn world_texture_bytes(&self) -> u64 {
        let (w, h) = self.world_size;
        u64::from(w) * u64::from(h) * 4
    }

    /// 世界尺寸变了就重建纹理与绑定组。
    fn ensure_world(&mut self, world: &World) -> Result<(), RenderError> {
        let size = (world.width() as u32, world.height() as u32);
        if self.texture.is_some() && self.world_size == size {
            return Ok(());
        }

        let max = self.device.limits().max_texture_dimension_2d;
        if size.0 > max || size.1 > max {
            return Err(RenderError::WorldTooLarge {
                width: size.0,
                height: size.1,
                max,
            });
        }

        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vivarium-world"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.world_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        self.uploader = Some(PixelUploader::new(world));
        // 换世界时相机回到中心。尺寸变了还留着旧中心，会直接飘到世界外。
        // 只在尺寸**真的变了**时重置：调用方可能刚设过相机，
        // 每次重建都覆盖的话，设好的视口会莫名其妙失效。
        if self.world_size != size {
            self.viewport = Viewport::centered_on(size);
        }
        self.texture = Some(texture);
        self.view = Some(view);
        self.world_size = size;
        // 场纹理只在**粗网格尺寸真的变了**时重建。
        //
        // 早先这里无条件把 `field_size` 清成 (0,0)，后果很隐蔽：
        // `upload_field` 通常比 `upload` 先跑（主循环要先传场再画），
        // 于是场被传进纹理 A、`ensure_world` 又建了纹理 B 并绑上它，
        // 而 B 的内容是**未定义的显存**——画面上四个滤镜都成了同一片怪色，
        // 不报错、不警告。
        self.ensure_field_texture(field_dims(world));
        // 无条件重建绑定组：世界纹理刚换过，而 `ensure_field_texture`
        // 在尺寸没变时是直接返回的——它不会顺手把绑定组补上。
        self.rebuild_bind_group();
        Ok(())
    }

    /// 确保场纹理存在且尺寸正确。
    fn ensure_field_texture(&mut self, size: (u32, u32)) {
        if self.field_texture.is_some() && self.field_size == size {
            return;
        }
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("vivarium-field"),
            size: wgpu::Extent3d {
                width: size.0.max(1),
                height: size.1.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rg32Float,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.field_view = Some(texture.create_view(&wgpu::TextureViewDescriptor::default()));
        self.field_texture = Some(texture);
        self.field_size = size;
        // 新纹理的内容是未定义的，必须重新传一次。
        self.field_revision = u64::MAX;
        self.rebuild_bind_group();
    }

    /// 重建绑定组。世界纹理或场纹理任何一个换了都要走一遍。
    fn rebuild_bind_group(&mut self) {
        let (Some(world_view), Some(field_view)) = (self.view.as_ref(), self.field_view.as_ref())
        else {
            return;
        };
        self.bind_group = Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vivarium-blit-bind-group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(world_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(field_view),
                },
            ],
        }));
    }

    /// 把场推上 GPU。版本号没变就什么都不做。
    ///
    /// 为什么按版本号而不是每帧都传：场每 8 tick 才算一次，而界面是 60 fps。
    /// 每帧传 128×128×8 字节 = 128 KB 看起来不多，但那是每秒 7.7 MB 的
    /// 无谓带宽，而且它会把"场到底有没有更新"这件事藏起来。
    fn upload_field(&mut self, field: &AirField) -> Result<(), RenderError> {
        let size = (field.cols() as u32, field.rows() as u32);
        if size.0 == 0 || size.1 == 0 {
            return Ok(());
        }
        self.ensure_field_texture(size);
        if self.field_revision == field.revision() {
            return Ok(());
        }
        let Some(texture) = self.field_texture.as_ref() else {
            return Ok(());
        };

        // 每行必须补齐到 256 字节：`write_texture` 的硬要求，
        // 而 128 列 × 8 字节 = 1024 刚好够、16 列 × 8 = 128 就不够。
        // 不补齐在 64² 的世界里会直接 panic。
        let row_bytes = size.0 as usize * 8;
        let padded = row_bytes.div_ceil(256) * 256;
        let mut bytes = vec![0u8; padded * size.1 as usize];
        for y in 0..size.1 as usize {
            for x in 0..size.0 as usize {
                let i = y * size.0 as usize + x;
                let o = y * padded + x * 8;
                let light = field.light().get(i).copied().unwrap_or(0.0);
                let temp = field.temp().get(i).copied().unwrap_or(0.0);
                bytes[o..o + 4].copy_from_slice(&light.clamp(0.0, 1.0).to_le_bytes());
                let t = (temp / field::TEMP_DISPLAY_MAX).clamp(0.0, 1.0);
                bytes[o + 4..o + 8].copy_from_slice(&t.to_le_bytes());
            }
        }

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &bytes,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded as u32),
                rows_per_image: Some(size.1),
            },
            wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
        );
        self.field_revision = field.revision();
        Ok(())
    }

    /// 把世界的新像素推上 GPU，返回上传的块数。
    fn upload(&mut self, world: &World) -> Result<usize, RenderError> {
        self.ensure_world(world)?;

        let uploader = self.uploader.as_mut().expect("ensure_world 刚建好它");
        let texture = self.texture.as_ref().expect("ensure_world 刚建好它");

        let dirty = uploader.prepare(world);
        if dirty == 0 {
            self.stats.last_uploaded_chunks = 0;
            return Ok(0);
        }

        for cy in 0..uploader.chunk_rows() {
            for cx in 0..uploader.chunk_cols() {
                if !uploader.is_dirty_chunk(cx, cy) {
                    continue;
                }
                // 尺寸要在 `build_chunk` **之前**取：它可变借用 uploader。
                let (cw, ch) = (uploader.chunk_w(cx), uploader.chunk_h(cy));
                let pixels = uploader.build_chunk(world, cx, cy);
                write_chunk(&self.queue, texture, cx, cy, cw, ch, pixels);
            }
        }

        self.stats.last_uploaded_chunks = dirty;
        self.stats.total_uploaded_chunks += dirty as u64;
        Ok(dirty)
    }

    /// 清屏色。取调色板的背景色，这样世界之外的留边和世界的空天是同一个颜色。
    fn clear_color(&self) -> wgpu::Color {
        let [r, g, b] = palette::BACKGROUND;
        let convert = |c: u8| -> f64 {
            let s = f64::from(c) / 255.0;
            if self.output_format.is_srgb() {
                // 目标是 sRGB 时，清屏色要按**线性**空间给，硬件负责编码回去。
                // 少了这一步，背景会比世界里的空天暗一大截，接缝肉眼可见。
                if s <= 0.040_45 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            } else {
                s
            }
        };
        wgpu::Color {
            r: convert(r),
            g: convert(g),
            b: convert(b),
            a: 1.0,
        }
    }

    /// 往目标纹理上画一帧。调用方负责提交 encoder。
    fn draw(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        target_size: (u32, u32),
    ) {
        let uniforms = SceneUniforms::new(
            self.world_size,
            target_size,
            self.viewport,
            self.content,
            self.output_format.is_srgb(),
            self.view_mode,
        );
        self.queue
            .write_buffer(&self.uniforms, 0, bytemuck::bytes_of(&uniforms));

        let Some(bind_group) = self.bind_group.as_ref() else {
            return;
        };

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vivarium-blit-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(self.clear_color()),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
}

/// 场纹理的粗网格尺寸（列, 行）。
///
/// 独立成函数而不是内联：渲染器算一次、测试断言一次，两处必须一致。
fn field_dims(world: &World) -> (u32, u32) {
    let cell = vivarium_core::AIR_CELL;
    (
        world.width().div_ceil(cell) as u32,
        world.height().div_ceil(cell) as u32,
    )
}

/// 把一个 64×64 块写进世界纹理。
///
/// 独立成自由函数是为了让它不借用 `Blitter`——`upload` 里 uploader 已经
/// 可变借走了，再来一个 `&mut self` 就打架了。
/// `w` / `h` 是这一块**实际**覆盖的格数（残块小于 `CHUNK_SIZE`）。
///
/// 拷一个完整的 64×64 到世界边缘会**越界**：纹理只有世界那么大，
/// 而 `origin` 是块的原点。`extent` 才是"拷多少"，`bytes_per_row` /
/// `rows_per_image` 只是缓冲区布局，两者必须分开。
#[allow(clippy::too_many_arguments)]
fn write_chunk(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    cx: usize,
    cy: usize,
    w: usize,
    h: usize,
    pixels: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: (cx * CHUNK_SIZE) as u32,
                y: (cy * CHUNK_SIZE) as u32,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        pixels,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(PixelUploader::chunk_bytes_per_row()),
            rows_per_image: Some(CHUNK_SIZE as u32),
        },
        wgpu::Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
    );
}

// ---------------------------------------------------------------------------
// 面向应用的渲染器
// ---------------------------------------------------------------------------

/// wgpu 像素渲染器。
///
/// 与 [`crate::ascii::AsciiRenderer`] 实现同一个 [`Renderer`] trait，
/// 读同一份 `&World`——"切换渲染器不改变模拟结果"这条验收断言
/// 因此是编译期成立的，不是靠自觉。
pub struct PixelRenderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    config: wgpu::SurfaceConfiguration,
    adapter_info: wgpu::AdapterInfo,
    blitter: Blitter,
    last_skip: Option<FrameSkip>,
}

impl std::fmt::Debug for PixelRenderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PixelRenderer")
            .field("adapter", &self.adapter_info.name)
            .field("backend", &self.adapter_info.backend)
            .field("surface_format", &self.config.format)
            .field("surface_size", &(self.config.width, self.config.height))
            .field("world_size", &self.blitter.world_size)
            .finish_non_exhaustive()
    }
}

impl PixelRenderer {
    /// 从已经建好的 surface 初始化。
    ///
    /// 之所以要外部传 `surface` 而不是自己建窗口：这样 `vivarium-render`
    /// 不必依赖 winit。窗口是应用层的事，画像素不是。
    pub fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> Result<Self, RenderError> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
            apply_limit_buckets: false,
        }))
        .map_err(|e| RenderError::GpuInit(format!("没有可用的图形适配器：{e}")))?;

        let adapter_info = adapter.get_info();
        let limits = adapter.limits();

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vivarium-device"),
            required_features: wgpu::Features::empty(),
            required_limits: limits,
            ..Default::default()
        }))
        .map_err(|e| RenderError::GpuInit(format!("创建设备失败：{e}")))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(wgpu::TextureFormat::is_srgb)
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| RenderError::GpuInit("交换链没有可用格式".into()))?;

        // Mailbox 有就用——它把"提交一帧"和"垂直同步"解耦，输入延迟明显更低。
        // 这是上帝模拟游戏，玩家会频繁拖动画笔，延迟比省电重要。
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let alpha_mode = caps
            .alpha_modes
            .first()
            .copied()
            .unwrap_or(wgpu::CompositeAlphaMode::Auto);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            // 0 是非法尺寸：窗口最小化时 inner_size 会给 0。
            width: width.max(1),
            height: height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let blitter = Blitter::new(device.clone(), queue, format);

        Ok(Self {
            surface,
            device,
            config,
            adapter_info,
            blitter,
            last_skip: None,
        })
    }

    /// 显卡与后端信息，用于启动日志。
    pub const fn adapter_info(&self) -> &wgpu::AdapterInfo {
        &self.adapter_info
    }

    /// 交换链格式。
    pub const fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// 当前交换链尺寸。
    pub const fn surface_size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// GPU 设备。界面层要用它建自己的管线。
    ///
    /// 暴露 device 是**刻意的**，不是封装泄漏：整个程序只有一个
    /// `wgpu::Device`，谁想画东西都得用它。让应用层自己再建一个 device
    /// 才是真的错——两个 device 之间的纹理不能共享，画面会变成黑的。
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// 提交队列。
    pub fn queue(&self) -> &wgpu::Queue {
        &self.blitter.queue
    }

    /// 世界纹理占多少字节。
    /// 当前相机。
    pub fn viewport(&self) -> Viewport {
        self.blitter.viewport
    }

    /// 直接设置相机。会被夹回世界范围内。
    pub fn set_viewport(&mut self, viewport: Viewport) {
        self.blitter.viewport = viewport.clamped(self.blitter.world_size);
    }

    /// 按倍数缩放。锚点是视口中心，不是鼠标——没有鼠标坐标时这是唯一合理的选择。
    pub fn zoom_by(&mut self, factor: f32) {
        let mut vp = self.blitter.viewport;
        vp.zoom = (vp.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        self.blitter.viewport = vp.clamped(self.blitter.world_size);
    }

    /// 平移多少格。正 `dx` 向右，正 `dy` 向下。
    pub fn pan_cells(&mut self, dx: f32, dy: f32) {
        let mut vp = self.blitter.viewport;
        vp.center.0 += dx;
        vp.center.1 += dy;
        self.blitter.viewport = vp.clamped(self.blitter.world_size);
    }

    /// 设定世界该画在屏幕上的哪一块（**物理像素**，`x0,y0,x1,y1`）。
    ///
    /// 界面每帧算一次交给这里。传 `None` 恢复"铺满整窗"。
    ///
    /// 为什么要让界面来定：面板尺寸是界面说了算的，而"世界画在哪"
    /// 必须和"中央可用区域"逐像素一致。渲染器自己算一遍的话，
    /// 两个数一旦不同步，症状是"鼠标点偏了半个面板宽度"——
    /// 而那看起来像是拾取算法写错了。
    pub fn set_content_rect(&mut self, rect: Option<(f32, f32, f32, f32)>) {
        self.blitter.content = rect.map(|(x0, y0, x1, y1)| [x0, y0, x1, y1]);
    }

    /// 切换显示滤镜。
    pub fn set_view_mode(&mut self, mode: ViewMode) {
        self.blitter.view_mode = mode;
    }

    /// 当前显示滤镜。
    pub const fn view_mode(&self) -> ViewMode {
        self.blitter.view_mode
    }

    /// 把光场 / 温度场推上 GPU。
    ///
    /// 由调用方在 `render_with` **之前**调。做成独立的一步而不是塞进
    /// `render_with` 的参数里，是因为场住在生命层：`render_with` 只认识
    /// `&World`，让它去认识 `&AirField` 就把渲染层和生命层绑死了。
    pub fn upload_field(&mut self, field: &AirField) {
        if let Err(e) = self.blitter.upload_field(field) {
            eprintln!("场上传失败：{e}");
        }
    }

    /// 以**屏幕上某一点**为锚点缩放。
    ///
    /// 和 `zoom_by` 的区别是锚点：`zoom_by` 绕视口中心缩，鼠标在哪儿都一样。
    /// 画画的时候这很难用——你想放大看的那一格，会随缩放跑到画面外去。
    ///
    /// 做法是"缩之前记下指针下面是哪一格，缩之后把相机推回去"。
    /// 这样指针下面**始终是同一格**，手感就是"把那一格拉近"。
    pub fn zoom_at(&mut self, factor: f32, screen: (f32, f32), target: (u32, u32)) {
        let Some(before) = self.cell_at(screen, target) else {
            self.zoom_by(factor);
            return;
        };
        self.zoom_by(factor);
        let Some(after) = self.cell_at(screen, target) else {
            return;
        };
        self.pan_cells(before.0 - after.0, before.1 - after.1);
    }

    /// 把相机对准某一格。
    pub fn look_at(&mut self, x: f32, y: f32) {
        let mut vp = self.blitter.viewport;
        vp.center = (x, y);
        self.blitter.viewport = vp.clamped(self.blitter.world_size);
    }

    /// 一格在屏幕上占多少像素。标题栏用它显示"现在是几倍"。
    pub fn pixels_per_cell(&self, target: (u32, u32)) -> f32 {
        let world = self.blitter.world_size;
        let lb = letterbox(world, target, self.blitter.content);
        let (vw, _) = self.blitter.viewport.view_size(world);
        if vw > 0.0 {
            lb.drawn[0] / vw
        } else {
            1.0
        }
    }

    /// 屏幕坐标（物理像素）→ 世界格坐标。
    ///
    /// 返回的是**连续坐标**（可以落在格子的任意位置），不是整数格号：
    /// 取整是调用方的事——画笔要取整，镜头对准要保留小数。
    ///
    /// ## 为什么必须和 `pixels_per_cell` 用同一套算法
    ///
    /// 世界纹理是按 letterbox 缩放进交换链的：正方形世界放进宽屏窗口时
    /// 左右留黑边。如果这里忘了算黑边，鼠标点击会整体偏一段，
    /// 而且**只在非正方形窗口里偏**——这种 bug 在自己机器上很难撞见。
    pub fn cell_at(&self, screen: (f32, f32), target: (u32, u32)) -> Option<(f32, f32)> {
        if target.0 == 0 || target.1 == 0 {
            return None;
        }
        let world = self.blitter.world_size;
        let lb = letterbox(world, target, self.blitter.content);
        let (drawn_w, drawn_h) = (lb.drawn[0], lb.drawn[1]);
        if drawn_w <= 0.0 || drawn_h <= 0.0 {
            return None;
        }
        let origin = (lb.origin[0], lb.origin[1]);

        // 归一化到"画出来的那一块"里。落在黑边上就是 `None`——
        // 那是世界之外，点它不该有任何效果。
        let u = (screen.0 - origin.0) / drawn_w;
        let v = (screen.1 - origin.1) / drawn_h;
        if !(0.0..=1.0).contains(&u) || !(0.0..=1.0).contains(&v) {
            return None;
        }

        let vp = self.blitter.viewport;
        let (vw, vh) = vp.view_size(world);
        let left = vp.center.0 - vw * 0.5;
        let top = vp.center.1 - vh * 0.5;
        Some((left + u * vw, top + v * vh))
    }

    pub fn world_texture_bytes(&self) -> u64 {
        self.blitter.world_texture_bytes()
    }

    /// 渲染统计。
    pub const fn stats(&self) -> PixelStats {
        self.blitter.stats
    }

    /// 上一帧被跳过的原因（如果被跳过了）。
    pub const fn last_skip(&self) -> Option<FrameSkip> {
        self.last_skip
    }

    /// 上一帧上传的块数。用来观察增量上传有没有生效。
    pub const fn uploaded_chunks(&self) -> usize {
        self.blitter.stats.last_uploaded_chunks
    }

    /// 渲染一帧，并在同一个 encoder 里让调用方画叠加层。
    ///
    /// ## 为什么要一个回调，而不是"先渲染、再让调用方提交"
    ///
    /// 交换链的帧必须由**拿到它的那个人**提交并 present。如果把这个职责
    /// 拆出去，调用方就得同时管 frame、encoder、present 三件事，
    /// 而它们全都只在 `render` 里有意义。回调让"画在世界之上"这件事
    /// 变成一次函数调用，不多不少。
    ///
    /// 回调拿到的 `TextureView` 与 `render` 用的是同一个，所以叠加层
    /// 天然在世界之上，不需要任何深度/混合的特殊处理——egui 的管线
    /// 自己会做 alpha 混合。
    pub fn render_with<F>(&mut self, world: &World, overlay: F) -> Result<(), RenderError>
    where
        F: FnOnce(&mut wgpu::CommandEncoder, &wgpu::TextureView),
    {
        self.render_frame(world, overlay)
    }

    fn reconfigure(&mut self) {
        self.surface.configure(&self.device, &self.config);
    }

    fn note_skip(&mut self, reason: FrameSkip) {
        self.last_skip = Some(reason);
        self.blitter.stats.skipped_frames += 1;
    }
}

impl Renderer for PixelRenderer {
    fn resize(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if (width, height) == (self.config.width, self.config.height) {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.reconfigure();
    }

    fn render(&mut self, world: &World) -> Result<(), RenderError> {
        self.render_frame(world, |_, _| {})
    }

    fn name(&self) -> &'static str {
        "pixel(wgpu)"
    }
}

impl PixelRenderer {
    /// 真正的渲染流程。`overlay` 在世界的 blit 之后、present 之前执行。
    fn render_frame<F>(&mut self, world: &World, overlay: F) -> Result<(), RenderError>
    where
        F: FnOnce(&mut wgpu::CommandEncoder, &wgpu::TextureView),
    {
        // 1. 把世界的新像素推上 GPU。静止的块在这里被完全跳过。
        self.blitter.upload(world)?;

        // 2. 拿一帧交换链。失败的情况分两种：等一等就好，和真的出错了。
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                // 还能画，但配置已经不再最优，顺手重配一下。
                self.reconfigure();
                frame
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.reconfigure();
                self.note_skip(FrameSkip::Outdated);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.reconfigure();
                self.note_skip(FrameSkip::Lost);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Timeout => {
                self.note_skip(FrameSkip::Timeout);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                self.note_skip(FrameSkip::Occluded);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                return Err(RenderError::SurfaceValidation);
            }
        };

        // 3. 画。
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("vivarium-frame"),
            });
        self.blitter.draw(
            &mut encoder,
            &target,
            (self.config.width, self.config.height),
        );

        // 3b. 叠加层（界面）。**同一个 encoder、同一个 target**，
        // 所以界面画在世界之上，且和世界一起提交、一起 present。
        //
        // 换成一个"渲染器返回纹理、应用层再画一次"的写法也能work，
        // 但那要多一次全屏拷贝，而且要多管一张纹理的生命周期。
        // 回调最省事：调用方拿到 encoder 和 view，画完就返回。
        overlay(&mut encoder, &target);
        drop(target);

        // 4. 呈现。wgpu 30 把 present 挪到了 Queue 上。
        self.blitter.queue.submit(Some(encoder.finish()));
        self.blitter.queue.present(frame);

        self.last_skip = None;
        self.blitter.stats.drawn_frames += 1;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use vivarium_core::life_sim::{stock_surface, LifeSim};
    use vivarium_core::{worldgen, CellType, MaterialKind, MaterialSim, Rng};

    // --- letterbox ------------------------------------------------------

    #[test]
    fn letterbox_keeps_square_world_square() {
        // 正方形世界放进宽屏窗口：上下顶满，左右留黑边。
        // 写反了很容易——"世界比窗口窄"对应的是**横向**缩小，不是纵向。
        let u = letterbox((512, 512), (1920, 1080), None);
        assert_eq!(u.scale[1], 1.0, "上下应当顶满");
        assert!(
            (u.scale[0] - 1080.0 / 1920.0).abs() < 1e-6,
            "横向应当缩到 0.5625"
        );
        assert_eq!(u.offset, [0.0, 0.0]);
        // 落到像素上确实是个正方形：0.5625 × 1920 = 1080，1.0 × 1080 = 1080。
        assert!((u.scale[0] * 1920.0 - u.scale[1] * 1080.0).abs() < 1e-3);
    }

    #[test]
    fn letterbox_keeps_wide_world_wide() {
        // 4:1 的世界放进 1:1 的窗口：左右顶满，上下留黑边。
        let u = letterbox((2048, 512), (1000, 1000), None);
        assert_eq!(u.scale[0], 1.0, "左右应当顶满");
        assert!((u.scale[1] - 0.25).abs() < 1e-6, "纵向应当缩到 0.25");
        // 像素层面依然是 4:1。
        let on_screen = (u.scale[0] * 1000.0) / (u.scale[1] * 1000.0);
        assert!((on_screen - 4.0).abs() < 1e-3);
    }

    #[test]
    fn letterbox_preserves_world_aspect_ratio() {
        // 这才是 letterbox 真正的不变量：**屏幕上的宽高比 == 世界的宽高比**。
        // 上面两条测的是具体数值，这条测的是性质——数值写反了它能抓住。
        for world in [(512, 512), (1024, 256), (256, 1024), (640, 640)] {
            for target in [
                (800, 600),
                (600, 800),
                (1920, 1080),
                (1080, 1920),
                (1000, 1000),
            ] {
                let u = letterbox(world, target, None);
                let on_screen_w = u.scale[0] * target.0 as f32;
                let on_screen_h = u.scale[1] * target.1 as f32;
                let world_aspect = world.0 as f32 / world.1 as f32;
                let screen_aspect = on_screen_w / on_screen_h;
                assert!(
                    (screen_aspect - world_aspect).abs() < 1e-3,
                    "{world:?} 放进 {target:?}：屏幕宽高比 {screen_aspect} ≠ 世界宽高比 {world_aspect}"
                );
            }
        }
    }

    #[test]
    fn letterbox_never_upscales_beyond_the_short_axis() {
        // 两个方向都不能超过 1.0，否则世界会被裁掉。
        for world in [(512, 512), (1024, 256), (256, 1024)] {
            for target in [(800, 600), (600, 800), (1920, 1080)] {
                let u = letterbox(world, target, None);
                assert!(u.scale[0] <= 1.0 + 1e-6, "{world:?} -> {target:?}");
                assert!(u.scale[1] <= 1.0 + 1e-6, "{world:?} -> {target:?}");
                // 至少一个方向要顶满，否则画面会凭空缩小。
                assert!(
                    (u.scale[0] - 1.0).abs() < 1e-6 || (u.scale[1] - 1.0).abs() < 1e-6,
                    "{world:?} -> {target:?}"
                );
            }
        }
    }

    #[test]
    fn letterbox_into_a_content_rect_centers_the_world() {
        // 800×600 的窗口，中央那块是 (100,50)-(500,350)，也就是 400×300。
        // 世界 512² 要整个放进去：每格 min(400/512, 300/512) = 0.586 像素，
        // 于是画出来是 300×300，在内容区里水平居中、垂直顶满。
        let u = letterbox((512, 512), (800, 600), Some([100.0, 50.0, 500.0, 350.0]));
        assert!((u.drawn[0] - 300.0).abs() < 0.01, "{}", u.drawn[0]);
        assert!((u.drawn[1] - 300.0).abs() < 0.01, "{}", u.drawn[1]);
        assert!((u.origin[0] - 150.0).abs() < 0.01, "{}", u.origin[0]);
        assert!((u.origin[1] - 50.0).abs() < 0.01, "{}", u.origin[1]);
        // 屏幕上的宽高比必须还是 1:1。
        assert!((u.drawn[0] - u.drawn[1]).abs() < 1e-3);

        // 内容区外（左侧面板那块）不该落进世界坐标里。
        let px = u.origin[0];
        assert!(px > 100.0, "世界应当被推进内容区里，而不是贴着窗口左边");
    }

    #[test]
    fn letterbox_survives_zero_sized_target() {
        // 窗口最小化时交换链尺寸真的是 0。这里不能出现 NaN 或除零。
        let u = letterbox((512, 512), (0, 0), None);
        assert_eq!(u.scale, [1.0, 1.0]);
        assert!(u.scale.iter().all(|v| v.is_finite()));
    }

    // --- 上传器 ---------------------------------------------------------

    /// 从块字节里读出一格的语义键。
    fn key_at(px: &[u8], off: usize) -> u32 {
        u32::from_ne_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]])
    }

    fn world_with(cells: &[(isize, isize, MaterialKind)]) -> World {
        let mut w = World::new(128, 128).unwrap();
        for &(x, y, k) in cells {
            w.set(x, y, k);
        }
        w
    }

    // --- 细胞 -----------------------------------------------------------

    /// 走完一帧：标脏 → 把每个脏块都生成一遍（顺带刷新影子副本）。
    ///
    /// `prepare` 本身不更新影子副本——影子副本是 `build_chunk` 写的。
    /// 所以"再 prepare 一次应该 0 脏块"的前提是先真的上传过一帧。
    fn settle(up: &mut PixelUploader, w: &World) -> usize {
        let n = up.prepare(w);
        for cy in 0..w.chunk_rows() {
            for cx in 0..w.chunk_cols() {
                if up.is_dirty_chunk(cx, cy) {
                    up.build_chunk(w, cx, cy);
                }
            }
        }
        n
    }

    #[test]
    fn an_organism_shows_up_even_though_its_cell_is_empty_material() {
        let mut w = world_with(&[(10, 10, MaterialKind::Water)]);
        let mut up = PixelUploader::new(&w);
        settle(&mut up, &w);
        assert_eq!(up.prepare(&w), 0, "静止世界不该有脏块");

        // 细胞格的材质字段永远是 Empty——只比材质的话这里会漏掉。
        w.set_owner(10, 10, CellOwner::new(1, 0, 0));
        assert_eq!(up.prepare(&w), 1, "生物出现必须让所在分块变脏");
    }

    #[test]
    fn an_organism_swimming_across_chunks_dirties_both() {
        let mut w = World::new(128, 128).unwrap();
        let mut up = PixelUploader::new(&w);
        settle(&mut up, &w);
        assert_eq!(up.prepare(&w), 0);

        // 从 (63, 10) 挪到 (64, 10)：跨越 x 方向的块边界。
        w.set_owner(63, 10, CellOwner::new(1, 0, 0));
        settle(&mut up, &w);
        w.vacate(63, 10);
        w.set_owner(64, 10, CellOwner::new(1, 0, 0));
        assert_eq!(up.prepare(&w), 2, "旧块和新块都得重画");
    }

    #[test]
    fn organism_pixels_override_the_material_underneath() {
        let mut w = world_with(&[(10, 10, MaterialKind::Water)]);
        w.set_owner(10, 10, CellOwner::new(1, 0, Organelles::from_cell_type(CellType::Producer).0));
        let mut up = PixelUploader::new(&w);
        settle(&mut up, &w);
        let px = up.build_chunk(&w, 0, 0).to_vec();

        let off = (10 * CHUNK_SIZE + 10) * 4;
        let key = key_at(&px, off);
        assert_ne!(
            key,
            MaterialKind::Water.as_u8() as u32,
            "鱼站在水里时上传的是鱼，不是水"
        );
        let bits = Organelles::from_cell_type(CellType::Producer).0 as u32;
        assert_eq!(
            key,
            CELL_BIT | (1 << ORGANISM_SHIFT) | bits,
            "编号 1 的生产者细胞"
        );
    }

    /// **细胞器位掩码必须原样到 GPU**，因为颜色是在 shader 里混出来的。
    ///
    /// v0.41 之前这里传的是 `CellType`（0..7），于是"叶绿体 + 气囊"和
    /// 纯生产者拿到的是同一个字节（都降级成 `Producer`），画出来一模一样。
    /// 现在传 13 位位掩码，组合自己就是颜色。
    #[test]
    fn the_organelles_reach_the_gpu() {
        let mut w = world_with(&[(10, 10, MaterialKind::Water)]);
        // 一格"叶绿体 + 气囊"：这个组合在旧的类型枚举里根本表达不出来。
        let combo = Organelles::from_cell_type(CellType::Producer).with(GAS);
        w.set_owner(10, 10, CellOwner::new(7, 2, combo.0));
        let mut up = PixelUploader::new(&w);
        settle(&mut up, &w);
        let px = up.build_chunk(&w, 0, 0).to_vec();

        let key = key_at(&px, (10 * CHUNK_SIZE + 10) * 4);
        assert_eq!(key & 0x1FFF, combo.0 as u32, "细胞器在低 13 位");
        assert_eq!(
            (key >> ORGANISM_SHIFT) & 0x3_FFFF,
            7,
            "编号在中间 18 位"
        );
        assert_ne!(key & CELL_BIT, 0, "最高位标记这是细胞");

        // 光靠"代表类型"是分不出这两格的——这正是换布局的理由。
        assert_eq!(
            combo.primary_cell_type(),
            CellType::Producer,
            "这个组合的代表类型确实是生产者，所以按类型上色会丢掉气囊"
        );
    }

    #[test]
    fn changing_the_organelles_dirties_the_chunk() {
        // 同一个编号、同一个位置，只换了细胞器——画面必须跟着变。
        // 只按编号比的话这里会漏：器官换了，画面上什么都不变。
        let mut w = world_with(&[(10, 10, MaterialKind::Water)]);
        w.set_owner(10, 10, CellOwner::new(1, 0, Organelles::from_cell_type(CellType::Mouth).0));
        let mut up = PixelUploader::new(&w);
        settle(&mut up, &w);
        assert_eq!(up.prepare(&w), 0);

        w.set_owner(10, 10, CellOwner::new(1, 0, Organelles::from_cell_type(CellType::Killer).0));
        assert_eq!(up.prepare(&w), 1, "换了细胞器必须重画");
    }

    #[test]
    fn first_prepare_uploads_everything() {
        let w = World::new(128, 128).unwrap();
        let mut u = PixelUploader::new(&w);
        // 128² 分成 2×2 = 4 块。
        assert_eq!(u.prepare(&w), 4);
        assert_eq!(u.dirty_chunk_count(), 4);
        assert!(u.is_dirty_chunk(0, 0));
        assert!(u.is_dirty_chunk(1, 1));
    }

    #[test]
    fn second_prepare_uploads_nothing_when_world_is_static() {
        let w = world_with(&[(10, 10, MaterialKind::Stone)]);
        let mut u = PixelUploader::new(&w);
        for cy in 0..u.chunk_rows() {
            for cx in 0..u.chunk_cols() {
                u.build_chunk(&w, cx, cy);
            }
        }
        // 这就是增量上传的核心断言：世界没动，一个字节都不该重传。
        assert_eq!(u.prepare(&w), 0);
    }

    #[test]
    fn changing_one_cell_dirties_exactly_one_chunk() {
        let mut w = World::new(128, 128).unwrap();
        let mut u = PixelUploader::new(&w);
        for cy in 0..u.chunk_rows() {
            for cx in 0..u.chunk_cols() {
                u.build_chunk(&w, cx, cy);
            }
        }
        assert_eq!(u.prepare(&w), 0);

        // (10, 10) 落在块 (0, 0)。
        w.set(10, 10, MaterialKind::Water);
        assert_eq!(u.prepare(&w), 1);
        assert!(u.is_dirty_chunk(0, 0));
        assert!(!u.is_dirty_chunk(1, 0));
        assert!(!u.is_dirty_chunk(0, 1));
        assert!(!u.is_dirty_chunk(1, 1));
    }

    #[test]
    fn redundant_write_does_not_dirty_anything() {
        // World::set 写同值时不标脏，上传器也不该被惊动。
        let mut w = World::new(128, 128).unwrap();
        let mut u = PixelUploader::new(&w);
        for cy in 0..u.chunk_rows() {
            for cx in 0..u.chunk_cols() {
                u.build_chunk(&w, cx, cy);
            }
        }
        assert!(!w.set(5, 5, MaterialKind::Empty)); // 本来就是 Empty
        assert_eq!(u.prepare(&w), 0);
    }

    /// **残块（世界尺寸不是 64 的倍数）不能读到世界外面。**
    ///
    /// 这条是被一次真实的闪退逼出来的：预设"大 768×728"一点就
    /// `index out of bounds: the len is 559104 but the index is 559104`
    /// ——那正好是第 728 行第一格（728 × 768 = 559104），也就是世界末尾
    /// 的**下一个**位置。`chunk_rows` 是 `div_ceil`，最后一块覆盖
    /// 704..768，而世界只有 728 行。
    ///
    /// 用 67×70 而不是 600×450：三个残块方向（宽残、高残、都残）都要踩到，
    /// 而数字小到能一眼算出边界。
    #[test]
    fn a_non_aligned_world_does_not_read_past_the_end() {
        for (w, h) in [(67usize, 70usize), (70, 67), (130, 66), (600, 450)] {
            let world = World::new(w, h).unwrap();
            let mut u = PixelUploader::new(&world);
            assert_eq!(u.chunk_cols(), w.div_ceil(CHUNK_SIZE));
            assert_eq!(u.chunk_rows(), h.div_ceil(CHUNK_SIZE));

            // 首帧全脏，逐个块生成一遍——这就是崩溃的那条路径。
            assert_eq!(u.prepare(&world), u.chunk_cols() * u.chunk_rows());
            for cy in 0..u.chunk_rows() {
                for cx in 0..u.chunk_cols() {
                    let (cw, ch) = (u.chunk_w(cx), u.chunk_h(cy));
                    assert!((1..=CHUNK_SIZE).contains(&cw), "{w}×{h} 块宽 {cw}");
                    assert!((1..=CHUNK_SIZE).contains(&ch), "{w}×{h} 块高 {ch}");
                    u.build_chunk(&world, cx, cy);
                }
            }
            // 影子副本被**全部**格子刷新过，所以再来一次是 0 脏块。
            // 少刷一格的话这里会一直脏下去，而画面完全看不出来。
            assert_eq!(u.prepare(&world), 0, "{w}×{h} 上传之后应当没有脏块");
        }
    }

    /// 残块的一行仍然占满 256 字节（`bytes_per_row` 的硬要求），
    /// 但只有前 `chunk_w` 格是有效内容。
    #[test]
    fn a_partial_chunk_keeps_the_row_stride() {
        let mut w = World::new(67, 70).unwrap();
        // 最后一列（x=66）放一格火，看它落在残块的第 2 列。
        w.set(66, 69, MaterialKind::Fire);
        let mut u = PixelUploader::new(&w);

        let (cx, cy) = (1, 1);
        assert_eq!((u.chunk_w(cx), u.chunk_h(cy)), (3, 6));
        let pixels = u.build_chunk(&w, cx, cy).to_vec();
        assert_eq!(pixels.len(), PixelUploader::chunk_byte_len());

        // 块内第 5 行第 2 列（世界 69, 66）——行距必须是 CHUNK_SIZE。
        let off = (5 * CHUNK_SIZE + 2) * 4;
        assert_eq!(key_at(&pixels, off), MaterialKind::Fire.as_u8() as u32);
    }

    #[test]
    fn chunk_pixels_carry_the_material_id() {
        let mut w = World::new(128, 128).unwrap();
        w.set(3, 1, MaterialKind::Fire);
        let mut u = PixelUploader::new(&w);

        let pixels = u.build_chunk(&w, 0, 0);
        assert_eq!(pixels.len(), PixelUploader::chunk_byte_len());

        // 块内第 1 行第 3 列 → 字节偏移 (row * CHUNK_SIZE + col) * 4。
        let off = (CHUNK_SIZE + 3) * 4;
        assert_eq!(
            key_at(pixels, off),
            MaterialKind::Fire.as_u8() as u32,
            "材质格上传的是材质编号"
        );

        let off_empty = (CHUNK_SIZE + 4) * 4;
        assert_eq!(
            key_at(pixels, off_empty),
            MaterialKind::Empty.as_u8() as u32
        );
    }

    #[test]
    fn chunk_rows_are_packed_tightly() {
        // 紧凑是关键：write_texture 的行距必须正好是 chunk_bytes_per_row，
        // 中间不能有世界宽度造成的空隙。
        let mut w = World::new(128, 128).unwrap();
        // 让第 0 行和第 1 行有明显不同的颜色。
        for x in 0..64 {
            w.set(x, 0, MaterialKind::Sand);
            w.set(x, 1, MaterialKind::Water);
        }
        let mut u = PixelUploader::new(&w);
        let pixels = u.build_chunk(&w, 0, 0);

        assert_eq!(key_at(pixels, 0), MaterialKind::Sand.as_u8() as u32);
        assert_eq!(
            key_at(pixels, CHUNK_SIZE * 4),
            MaterialKind::Water.as_u8() as u32
        );
    }

    #[test]
    fn prepare_does_not_mutate_the_world() {
        // 直接对应 SPEC 的验收断言：渲染不得改变模拟结果。
        // PixelUploader 只拿到 &World，这条在签名层面就成立了；
        // 这里再钉一遍，防止将来有人为了"顺手修一下"把它改成 &mut World。
        let mut w = World::new(256, 256).unwrap();
        w.fill_rect(0, 0, 100, 60, MaterialKind::Water);
        w.fill_rect(200, 200, 40, 40, MaterialKind::Stone);
        w.set(77, 77, MaterialKind::Fire);
        let before = w.hash();

        let mut u = PixelUploader::new(&w);
        u.prepare(&w);
        for cy in 0..u.chunk_rows() {
            for cx in 0..u.chunk_cols() {
                u.build_chunk(&w, cx, cy);
            }
        }
        u.prepare(&w);

        assert_eq!(w.hash(), before);
    }

    #[test]
    fn chunk_row_matches_wgpu_alignment() {
        // 这条是上面那个 const 断言的可读版本，失败时报错信息更有用。
        assert_eq!(
            PixelUploader::chunk_bytes_per_row(),
            wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
        );
    }

    #[test]
    fn uploader_covers_every_cell_exactly_once() {
        // 边界检查：每个块都是完整的 64×64，不会因为世界边缘而缩水。
        let w = World::new(192, 128).unwrap(); // 3×2 块
        let u = PixelUploader::new(&w);
        assert_eq!(u.chunk_cols() * u.chunk_rows(), 6);
        assert_eq!(u.chunk_cols() * CHUNK_SIZE, w.width());
        assert_eq!(u.chunk_rows() * CHUNK_SIZE, w.height());
    }

    // --- GPU（没有适配器时自动跳过） --------------------------------------

    /// 把 GPU 测试串起来的全局锁。
    ///
    /// `cargo test` 默认多线程跑测试，于是好几个 `wgpu::Instance::new` +
    /// `request_device` 会**同时**发生。在 DX12 的软件适配器（WARP）上这不是
    /// 线程安全的：进程直接 `STATUS_ACCESS_VIOLATION` 退出，而且只崩整个测试
    /// 二进制、不告诉你崩在哪个测试里——现象是"跑三遍崩两遍，单线程跑全绿"。
    ///
    /// 所以每个碰 wgpu 的测试第一行先 `let _gpu = gpu_lock();`，
    /// 拿到它才能往下走。锁在测试结束时自动释放。
    fn gpu_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        // 某个 GPU 测试 panic 之后锁会中毒，但我们要的只是互斥，
        // 不关心上一次有没有炸——所以直接取回内部值。
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 建一个离屏 device。返回 `None` 表示这台机器上跑不了 wgpu。
    ///
    /// 这里用 `force_fallback_adapter: true`——测试要的是"管线对不对"，
    /// 不是"显卡快不快"，软件光栅化（WARP / lavapipe）完全够用，
    /// 而且在没有独显的 CI 上也能跑。
    ///
    /// **调用方必须先拿 [`gpu_lock`]。** 见那个函数的注释。
    fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: true,
            compatible_surface: None,
            apply_limit_buckets: false,
        }))
        .ok()?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("vivarium-test-device"),
            required_features: wgpu::Features::empty(),
            required_limits: adapter.limits(),
            ..Default::default()
        }))
        .ok()?;
        Some((device, queue))
    }

    #[test]
    fn gpu_pipeline_compiles_and_draws() {
        let _gpu = gpu_lock();
        let Some((device, queue)) = headless_device() else {
            eprintln!("跳过：本机没有可用的 wgpu 适配器");
            return;
        };

        // 目标纹理取 64×64，行距正好 256，读回时不用补 padding。
        let target_format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let mut blitter = Blitter::new(device.clone(), queue.clone(), target_format);

        let world = world_with(&[
            (0, 0, MaterialKind::Stone),
            (1, 0, MaterialKind::Sand),
            (2, 0, MaterialKind::Water),
            (3, 0, MaterialKind::Fire),
        ]);

        // 上传 + 画。任何着色器编译错误、绑定组布局不匹配都会在这里炸出来。
        let uploaded = blitter.upload(&world).expect("上传失败");
        assert_eq!(uploaded, world.chunk_count());

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-target"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("test-encoder"),
        });
        blitter.draw(&mut encoder, &target_view, (64, 64));
        queue.submit(Some(encoder.finish()));

        // 静止的世界再跑一轮：应当一个块都不传。
        assert_eq!(blitter.upload(&world).unwrap(), 0);

        device.poll(wgpu::PollType::wait_indefinitely()).ok();
    }

    #[test]
    fn gpu_blit_produces_the_palette_color() {
        let _gpu = gpu_lock();
        let Some((device, queue)) = headless_device() else {
            eprintln!("跳过：本机没有可用的 wgpu 适配器");
            return;
        };

        const SIZE: u32 = 64;
        let target_format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let mut blitter = Blitter::new(device.clone(), queue.clone(), target_format);

        // 整个世界刷成水，然后看左上角那一格读回来是不是水的颜色。
        let mut world = World::new(64, 64).unwrap();
        world.fill_all(MaterialKind::Water);

        blitter.upload(&world).expect("上传失败");

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test-target"),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("test-encoder"),
        });
        blitter.draw(&mut encoder, &target_view, (SIZE, SIZE));

        // 把结果拷回 CPU。行距必须是 256 的倍数，64×4 = 256 正好。
        let bytes_per_row = SIZE * 4;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test-readback"),
            size: u64::from(bytes_per_row) * u64::from(SIZE),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(SIZE),
                },
            },
            wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::PollType::wait_indefinitely()).ok();
        assert!(rx.recv().expect("映射回调没回来").is_ok(), "缓冲映射失败");

        let data = slice
            .get_mapped_range()
            .expect("缓冲已经映射过了，取视图不该失败");
        // 世界纹理是 sRGB、目标也是 sRGB，所以往返之后应当**逐字节相等**。
        // 如果哪天这里差了 1~2，多半是清屏色的线性转换漏了，或者两边格式不一致。
        let [r, g, b] = palette::rgb(MaterialKind::Water);
        assert_eq!(&data[0..4], &[r, g, b, 255], "左上角不是水的颜色");
        drop(data);
        readback.unmap();
    }

    // --- 人眼预览 ---------------------------------------------------------

    /// 预览图尺寸。4:3，用来顺带把 letterbox 的留边也画出来。
    const PREVIEW_W: u32 = 640;
    const PREVIEW_H: u32 = 480;
    /// 用 SPEC 里的默认世界尺寸——预览图要能代表真实启动画面。
    ///
    /// 早先用的是 256²，结果一座岛就占满整个画面，
    /// 看不出"海洋 + 岛屿"是什么样子。
    const PREVIEW_WORLD: usize = 512;

    /// 预览用的世界地貌。
    ///
    /// 直接调用 `vivarium_core::worldgen`——**不要再往这里抄一份地形生成**。
    /// M0 期间这段逻辑在 sim / game / 这里各有一份，三份已经开始漂移；
    /// 预览图只有在和真实启动画面用同一套地貌时才有意义。
    fn seed_preview_world(world: &mut World, rng: &mut Rng) -> worldgen::WorldLayout {
        worldgen::sea_and_islands(world, rng)
    }

    fn assert_color_close(actual: &[u8], expected: [u8; 4], what: &str) {
        for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
            assert!(
                (*a as i32 - *e as i32).abs() <= 1,
                "{what}：通道 {i} 期望 {e}，实际 {a}（容差 1）"
            );
        }
    }

    /// 把当前世界画进离屏纹理并读回像素。返回 `(像素, 本次上传的分块数)`。
    fn blit_and_read(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        blitter: &mut Blitter,
        target: &wgpu::Texture,
        target_view: &wgpu::TextureView,
        world: &World,
    ) -> (Vec<u8>, usize) {
        let uploaded = blitter.upload(world).expect("上传失败");
        assert!(
            uploaded <= world.chunk_count(),
            "上传的分块数不可能超过总块数"
        );

        // 640 × 4 = 2560，正好是 256 的整数倍，读回时不用补 padding。
        let bytes_per_row = PREVIEW_W * 4;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview-readback"),
            size: u64::from(bytes_per_row) * u64::from(PREVIEW_H),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("preview-encoder"),
        });
        blitter.draw(&mut encoder, target_view, (PREVIEW_W, PREVIEW_H));
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(PREVIEW_H),
                },
            },
            wgpu::Extent3d {
                width: PREVIEW_W,
                height: PREVIEW_H,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::PollType::wait_indefinitely()).ok();
        assert!(rx.recv().expect("映射回调没回来").is_ok(), "缓冲映射失败");

        let pixels: Vec<u8> = slice.get_mapped_range().expect("取映射视图失败").to_vec();
        readback.unmap();
        (pixels, uploaded)
    }

    /// 把像素写成 PNG。返回落盘路径。
    fn write_png(name: &str, pixels: &[u8]) -> std::path::PathBuf {
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(name);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).expect("建 target 目录失败");
        }
        let file = std::fs::File::create(&out).expect("创建 PNG 失败");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), PREVIEW_W, PREVIEW_H);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("写 PNG 头失败");
        writer.write_image_data(pixels).expect("写 PNG 数据失败");
        drop(writer);
        eprintln!("预览已写出：{}", out.display());
        assert!(out.metadata().expect("读不到刚写的文件").len() > 1024);
        out
    }

    #[test]
    fn cells_of_one_body_fuse_instead_of_leaving_a_seam() {
        let _gpu = gpu_lock();
        let Some((device, queue)) = headless_device() else {
            eprintln!("跳过：本机没有可用的 wgpu 适配器");
            return;
        };

        const SIZE: u32 = 256;
        let mut blitter = Blitter::new(
            device.clone(),
            queue.clone(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );

        // 16×16 的水，两格同族细胞横着并排。
        let mut world = World::new(64, 64).unwrap();
        world.fill_all(MaterialKind::Water);
        world.set_owner(5, 5, CellOwner::new(1, 0, Organelles::from_cell_type(CellType::Mover).0));
        world.set_owner(6, 5, CellOwner::new(1, 1, Organelles::from_cell_type(CellType::Mover).0));

        // 视口 4×4 格铺满 256 像素 → 每格 64 像素。
        // 格 5 的屏幕区间是 [96, 160)，格 6 是 [160, 224)，接缝正好在 x = 160。
        // 世界 64 格除以视口 4 格，所以 zoom = 16。
        blitter.viewport = Viewport {
            center: (5.5, 5.5),
            zoom: 16.0,
        };
        let px = blit_to_rgba(
            &device,
            &queue,
            &mut blitter,
            &world,
            Viewport {
                center: (5.5, 5.5),
                zoom: 16.0,
            },
            SIZE,
        );

        let at = |x: u32, y: u32| -> [u8; 3] {
            let off = ((y * SIZE + x) * 4) as usize;
            [px[off], px[off + 1], px[off + 2]]
        };

        // 两个细胞的中心：格 5 在 x≈128，格 6 在 x≈192，y 都是 5.5 → 128。
        let left = at(128, 128);
        let right = at(192, 128);
        let seam = at(160, 128);

        eprintln!(
            "格5中心 {:?} 接缝 {:?} 格6中心 {:?} 水 {:?}",
            left,
            seam,
            right,
            at(40, 128)
        );

        // 关键断言：接缝处的亮度必须接近两侧，而不是掉回水色。
        let luma = |c: [u8; 3]| c[0] as i32 + c[1] as i32 + c[2] as i32;
        let body = luma(left).max(luma(right));
        let water = luma(at(40, 128));
        assert!(
            luma(seam) > (body + water) / 2,
            "接缝 {:?} 掉回了水色 {:?}——同族的细胞必须连成一片，不能各画各的框",
            seam,
            at(40, 128)
        );
    }

    /// 跑一遍真正的 blit 管线，把结果读回 CPU。
    ///
    /// 刻意走 GPU 而不是在 CPU 上复刻着色器：复刻出来的是一份"我以为
    /// 着色器会这么画"的实现，它和着色器一起错的时候，看预览图是看不出来的。
    fn blit_to_rgba(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        blitter: &mut Blitter,
        world: &World,
        viewport: Viewport,
        size: u32,
    ) -> Vec<u8> {
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        // 必须先 upload：首次上传会建纹理，而建纹理会把相机重置到世界中心。
        // 反过来（先设相机再上传）会被那次重置悄悄抹掉，然后你会盯着一张
        // "细胞不见了"的图查很久——这个坑我踩过了。
        blitter.upload(world).expect("上传失败");
        blitter.viewport = viewport.clamped((world.width() as u32, world.height() as u32));

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview-target"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("preview-encoder"),
        });
        blitter.draw(&mut encoder, &view, (size, size));

        // 读回的行距必须是 256 的倍数，所以尺寸只能取 4 的倍数再对齐。
        let bytes_per_row = size * 4;
        assert_eq!(
            bytes_per_row % wgpu::COPY_BYTES_PER_ROW_ALIGNMENT,
            0,
            "预览尺寸 {size} 的行距 {bytes_per_row} 不是 256 的倍数"
        );
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("preview-readback"),
            size: u64::from(bytes_per_row) * u64::from(size),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(size),
                },
            },
            wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        let slice = readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        device.poll(wgpu::PollType::wait_indefinitely()).ok();
        rx.recv().expect("读回通道断了").expect("映射失败");

        let data = slice.get_mapped_range().expect("映射后读不到范围");
        let out = data.to_vec();
        drop(data);
        readback.unmap();
        out
    }

    fn write_png_sized(name: &str, pixels: &[u8], size: u32) -> std::path::PathBuf {
        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(name);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).expect("建 target 目录失败");
        }
        let file = std::fs::File::create(&out).expect("创建 PNG 失败");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), size, size);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("写 PNG 头失败");
        writer.write_image_data(pixels).expect("写 PNG 数据失败");
        drop(writer);
        eprintln!("预览已写出：{}", out.display());
        out
    }

    /// 放大到能看清细胞，检查"细胞到底长什么样"。
    ///
    /// ```text
    /// cargo test -p vivarium-render --lib -- --ignored --nocapture render_preview_cells
    /// ```
    #[test]
    #[ignore = "会写 PNG，按需手动跑"]
    fn render_preview_cells() {
        let _gpu = gpu_lock();
        let Some((device, queue)) = headless_device() else {
            eprintln!("跳过：本机没有可用的 wgpu 适配器");
            return;
        };

        const SIZE: u32 = 768;
        let mut blitter = Blitter::new(
            device.clone(),
            queue.clone(),
            wgpu::TextureFormat::Rgba8UnormSrgb,
        );

        let mut world = World::new(256, 256).unwrap();
        let mut rng = Rng::new(42);
        worldgen::sea_and_islands(&mut world, &mut rng);
        let mut sim = MaterialSim::new();
        for _ in 0..400 {
            sim.step(&mut world);
        }

        let mut life = LifeSim::new(&mut world);
        stock_surface(&mut world, &mut life, 14, 300, false);
        for _ in 0..200 {
            life.step(&mut world);
            sim.step(&mut world);
        }

        // 挑一条还活着的，把相机对准它的锚点。
        let Some(target) = life.organisms().first() else {
            panic!("一条鱼都没活下来，预览无从谈起");
        };
        let center = (target.anchor.0 as f32 + 0.5, target.anchor.1 as f32 + 0.5);
        eprintln!("对准 ({}, {})", center.0, center.1);

        for (name, zoom) in [
            ("render-cells-wide.png", 3.0),
            ("render-cells-close.png", 14.0),
        ] {
            let px = blit_to_rgba(
                &device,
                &queue,
                &mut blitter,
                &world,
                Viewport { center, zoom },
                SIZE,
            );
            write_png_sized(name, &px, SIZE);
        }
    }

    /// 把离屏渲染结果写成 PNG，供人眼检查。
    ///
    /// 默认 `#[ignore]`：它会写文件，不该混在日常 `cargo test` 里。
    /// 需要看的时候：
    ///
    /// ```text
    /// cargo test -p vivarium-render --lib -- --ignored --nocapture render_preview_png
    /// ```
    ///
    /// 为什么值得有这么一个测试：像素渲染的 bug（颜色偏了、上下颠倒、
    /// 留边位置错了、行列错位）**全都是看一眼就知道、但很难写出好断言**的那一类。
    /// 上面那些断言只能钉住少数几个采样点，这张图能一次性覆盖全部 307200 个像素。
    ///
    /// M1 起它写**两张**：`render-preview.png` 是刚生成的地貌，
    /// `render-preview-settled.png` 是材质层跑完之后的同一张图。
    /// 两张对比就能看出沙塌了、水找平了、气泡浮上去了——
    /// 这是"材质规则真的在工作"最直观的证据。
    #[test]
    #[ignore = "会写文件；需要人眼看时手动跑"]
    fn render_preview_png() {
        let _gpu = gpu_lock();
        let Some((device, queue)) = headless_device() else {
            eprintln!("跳过：本机没有可用的 wgpu 适配器");
            return;
        };

        let target_format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let mut blitter = Blitter::new(device.clone(), queue.clone(), target_format);

        let mut world = World::new(PREVIEW_WORLD, PREVIEW_WORLD).unwrap();
        let mut rng = Rng::new(2026_1016);
        let layout = seed_preview_world(&mut world, &mut rng);

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview-target"),
            size: wgpu::Extent3d {
                width: PREVIEW_W,
                height: PREVIEW_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // --- 第一张：刚生成的地貌 ---------------------------------------
        let (pixels, first_upload) =
            blit_and_read(&device, &queue, &mut blitter, &target, &target_view, &world);
        assert_eq!(
            first_upload,
            world.chunk_count(),
            "首帧没有任何参照，必须整张上传"
        );

        // --- 先断言几个"看一眼才知道对不对"的点 -------------------------

        let bpr = (PREVIEW_W * 4) as usize;
        let bg = palette::BACKGROUND;
        let [sr, sg, sb] = palette::rgb(MaterialKind::Stone);

        // 世界是正方形（1:1），目标是 4:3，所以左右应当各留 80 像素黑边。
        let bar = ((PREVIEW_W - PREVIEW_H) / 2) as usize; // 80

        // 取最下面一行——那里世界一定是石头，留边一定是背景色，
        // 两者颜色不同，所以这条断言真的能分辨出边界在哪。
        //
        // （不要拿天空那一行来测：留边色**故意**等于空天色，好让接缝看不见，
        //   在那里两者本来就该一模一样。）
        let row = (PREVIEW_H - 20) as usize;
        let bar_px = &pixels[row * bpr + 10 * 4..][..4];
        assert_color_close(bar_px, [bg[0], bg[1], bg[2], 255], "左侧留边");

        // 留边右边界之后两像素，应当已经是石头。
        let inside = &pixels[row * bpr + (bar + 2) * 4..][..4];
        assert_color_close(inside, [sr, sg, sb, 255], "留边右侧");

        // 反方向：留边左边界之前一像素，必须还是留边。
        let still_bar = &pixels[row * bpr + (bar - 1) * 4..][..4];
        assert_color_close(still_bar, [bg[0], bg[1], bg[2], 255], "留边左边界内侧");

        // 右边界同样要落在 640 - 80 = 560。
        let right_bar = &pixels[row * bpr + (PREVIEW_W as usize - bar + 1) * 4..][..4];
        assert_color_close(right_bar, [bg[0], bg[1], bg[2], 255], "右侧留边");

        // --- 再验一遍新地貌（海洋 + 岛屿）确实画出来了 ----------------
        //
        // 世界映射到屏幕上 480×480 的方块，左上角在 (80, 0)。
        let scale = PREVIEW_H as f32 / PREVIEW_WORLD as f32;
        let sx = |wx: usize| 80 + (wx as f32 * scale) as usize;
        let sy = |wy: usize| (wy as f32 * scale) as usize;

        // 挑一列确定是海的：地表远在水面线之下。
        let ocean_x = (0..PREVIEW_WORLD)
            .find(|&x| layout.surface[x] > layout.waterline as isize + 24)
            .expect("世界里的应当至少有一列是海");

        let sky = &pixels[sy(layout.waterline - 10) * bpr + sx(ocean_x) * 4..][..4];
        assert_color_close(sky, [bg[0], bg[1], bg[2], 255], "水面线之上的空天");

        let [wr, wg, wb] = palette::rgb(MaterialKind::Water);
        let sea = &pixels[sy(layout.waterline + 10) * bpr + sx(ocean_x) * 4..][..4];
        assert_color_close(sea, [wr, wg, wb, 255], "水面线之下的海水");

        // 海床线以下是基岩，不可能有水。
        let bed = &pixels[sy(layout.seabed + 8) * bpr + sx(ocean_x) * 4..][..4];
        assert_color_close(bed, [sr, sg, sb, 255], "海床线以下的基岩");

        write_png("render-preview.png", &pixels);

        // --- 第二张：材质层跑完之后 -------------------------------------
        //
        // 先让模拟把世界塌稳。512² 大约两百多 tick 就完全静止，
        // 这里多跑一截是为了拿到"稳态"而不是"正在塌"的中间态。
        let mut sim = MaterialSim::new();
        let mut settled_at = None;
        for t in 0..2000 {
            sim.step(&mut world);
            if world.dirty_chunk_count() == 0 {
                settled_at = Some(t);
                break;
            }
        }
        let settled_at = settled_at.expect("预览用的世界应当能塌稳");
        assert_eq!(world.dirty_chunk_count(), 0, "稳态世界的脏块数应当是 0");

        // 这一帧把模拟造成的变化追上 GPU。上传量应当大于 0——
        // 否则说明要么世界压根没变，要么影子副本漏更新了。
        let (settled, caught_up) =
            blit_and_read(&device, &queue, &mut blitter, &target, &target_view, &world);
        assert!(caught_up > 0, "世界明明变了，却一块都没上传");
        assert_ne!(
            settled, pixels,
            "跑了 {settled_at} tick 之后画面居然一模一样"
        );

        // 再画一帧：世界已经不动了，应当**一块都不传**，而且画面逐像素相同。
        //
        // 这条同时钉住两件事：增量上传确实按"内容有没有变"工作，
        // 以及渲染是纯函数——同一份世界画两次必须给出同一张图。
        let (again, idle) =
            blit_and_read(&device, &queue, &mut blitter, &target, &target_view, &world);
        assert_eq!(idle, 0, "静止世界不该再上传任何分块");
        assert_eq!(again, settled, "同一份世界重画应当逐像素相同");

        eprintln!(
            "世界在第 {settled_at} tick 塌稳；本帧上传 {caught_up}/{} 块，下一帧 {idle} 块；材质计数 {:?}",
            world.chunk_count(),
            world.census()
        );
        write_png("render-preview-settled.png", &settled);

        // --- 第三张：把鱼放进去 -----------------------------------------
        //
        // 这张图存在的意义不是"好看"，而是**证明生物真的出现在画面上**。
        // 细胞格的材质字段永远是 `Empty`，如果脏块检测只看材质，
        // 这条鱼在水里游的时候渲染器会认为什么都没变，画面上一条鱼也没有。
        let mut life = LifeSim::new(&mut world);
        let ids = stock_surface(&mut world, &mut life, 80, 300, false);
        assert!(!ids.is_empty(), "水面附近应当放得下鱼");

        for _ in 0..300 {
            life.step(&mut world);
            sim.step(&mut world);
        }
        assert!(!life.organisms().is_empty(), "三百 tick 之后鱼不该全死光");

        let (with_fish, _) =
            blit_and_read(&device, &queue, &mut blitter, &target, &target_view, &world);
        assert_ne!(with_fish, settled, "放鱼之后画面必须变了");

        // 挑一条鱼的第一格，它画出来必须是那个有机体的颜色。
        let sample = life
            .organisms()
            .first()
            .map(|o| o.anchor)
            .expect("至少有一条鱼");
        let (fx, fy) = (sample.0 as usize, sample.1 as usize);
        let fish_px = &with_fish[sy(fy) * bpr + sx(fx) * 4..][..4];
        let owner = world.owner(fx as isize, fy as isize);
        assert!(owner.is_some(), "锚点格必须被占用");
        let [fr, fg, fb] = palette::organism_rgb(owner.organism);
        assert_color_close(fish_px, [fr, fg, fb, 255], "鱼在画面上的颜色");

        eprintln!(
            "放鱼 {} 条，跑 300 tick 后剩 {} 条，能量账 {:?}",
            ids.len(),
            life.organisms().len(),
            life.audit(&world)
        );
        write_png("render-preview-life.png", &with_fish);
    }

    /// 四个显示滤镜各导一张 PNG，并钉住它们**必须互不相同**。
    ///
    /// 这条测试真正防的是"滤镜接上了但没生效"：`view_mode` 传进 uniform
    /// 之后如果哪个环节（打包、binding、着色器的分支）掉了，画面会
    /// 静静地退化成材质视图——不报错、不警告，看起来"只是没反应"。
    ///
    /// 跑法：
    /// ```text
    /// cargo test -p vivarium-render --lib -- --ignored --nocapture view_filter_preview_png
    /// ```
    #[test]
    #[ignore = "会写文件；需要人眼看时手动跑"]
    fn view_filter_preview_png() {
        let _gpu = gpu_lock();
        let Some((device, queue)) = headless_device() else {
            eprintln!("跳过：本机没有可用的 wgpu 适配器");
            return;
        };

        let target_format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let mut blitter = Blitter::new(device.clone(), queue.clone(), target_format);

        let mut world = World::new(PREVIEW_WORLD, PREVIEW_WORLD).unwrap();
        let mut rng = Rng::new(2026_1016);
        let layout = seed_preview_world(&mut world, &mut rng);

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("filter-target"),
            size: wgpu::Extent3d {
                width: PREVIEW_W,
                height: PREVIEW_H,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: target_format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // 让世界跑一会儿，再放鱼 + 点一把火 + 放一盏灯——
        // 四个滤镜各自都需要"有东西可看"：温度滤镜要有热点，
        // 光照滤镜要有明暗梯度，物种滤镜要有生物。
        let mut sim = MaterialSim::new();
        let mut life = LifeSim::new(&mut world);
        let ids = stock_surface(&mut world, &mut life, 60, 300, false);
        assert!(!ids.is_empty(), "水面附近应当放得下鱼");
        // 海床上放一盏灯：光照滤镜里应当出现一个明显的亮斑。
        life.field_mut()
            .add_source(layout.seabed as i32 - 6, layout.waterline as i32 + 60);
        for _ in 0..240 {
            life.step(&mut world);
            sim.step(&mut world);
        }

        let bpr = (PREVIEW_W * 4) as usize;
        let scale = PREVIEW_H as f32 / PREVIEW_WORLD as f32;
        let sx = |wx: usize| 80 + (wx as f32 * scale) as usize;
        let sy = |wy: usize| (wy as f32 * scale) as usize;
        let at = |px: &[u8], x: usize, y: usize| -> [u8; 4] {
            let o = y * bpr + x * 4;
            [px[o], px[o + 1], px[o + 2], px[o + 3]]
        };
        let luma = |c: [u8; 4]| u32::from(c[0]) + u32::from(c[1]) + u32::from(c[2]);

        let ocean_x = (0..PREVIEW_WORLD)
            .find(|&x| layout.surface[x] > layout.waterline as isize + 24)
            .expect("世界里的应当至少有一列是海");

        let mut shots: Vec<(ViewMode, Vec<u8>)> = Vec::new();
        for mode in ViewMode::ALL {
            blitter.view_mode = mode;
            blitter
                .upload_field(life.field())
                .expect("场上传不该失败");
            let (px, _) = blit_and_read(&device, &queue, &mut blitter, &target, &target_view, &world);
            shots.push((mode, px));
        }
        for (mode, px) in &shots {
            let name = match mode {
                ViewMode::Material => "view-material.png",
                ViewMode::Temperature => "view-temperature.png",
                ViewMode::Light => "view-light.png",
                ViewMode::Species => "view-species.png",
            };
            write_png(name, px);
        }

        // --- 光照滤镜：天空亮、深海暗 -----------------------------------
        //
        // 索引照 `ViewMode::ALL` 的顺序：0 材质 / 1 温度 / 2 光照 / 3 物种。
        // 写错一位的症状是"断言全过但看的是另一张图"——而两张图都很像"一片蓝"。
        let light = &shots[2].1;
        let sky = at(light, sx(ocean_x), sy(layout.waterline - 12));
        // 150 格深：光只剩 `1 - 150/180 ≈ 0.17`，色带上接近黑。
        let deep = at(light, sx(ocean_x), sy(layout.waterline + 150));
        assert!(
            luma(sky) > luma(deep) * 2,
            "光照滤镜里天空应当明显比深海亮：{sky:?} vs {deep:?}"
        );
        assert!(luma(deep) < 200, "一百五十格深的海水应当几乎是黑的：{deep:?}");

        // --- 温度滤镜：整屏都该是"冷"的（环境 20°C）--------------------
        let temp = &shots[1].1;
        let sea = at(temp, sx(ocean_x), sy(layout.waterline + 40));
        assert!(
            u32::from(sea[2]) > u32::from(sea[0]),
            "20°C 的环境应当是偏蓝的：{sea:?}"
        );

        // --- 物种滤镜：材质压成暗剪影 -----------------------------------
        let species = &shots[3].1;
        let bed = at(species, sx(ocean_x), sy(layout.seabed + 8));
        let [sr, sg, sb] = palette::rgb(MaterialKind::Stone);
        assert!(
            luma(bed) * 2 < luma([sr, sg, sb, 255]),
            "物种滤镜里的石头应当比材质视图暗得多：{bed:?}"
        );

        // --- 四张图必须互不相同 -----------------------------------------
        //
        // 这是这条测试的核心判据。少了它，"滤镜接上了但没生效"会
        // 安安静静地退化成材质视图，而画面上看起来只是"点了没反应"。
        for i in 0..shots.len() {
            for j in (i + 1)..shots.len() {
                assert_ne!(
                    shots[i].1, shots[j].1,
                    "{:?} 和 {:?} 画出来一模一样——滤镜没生效",
                    shots[i].0, shots[j].0
                );
            }
        }
        eprintln!(
            "四张滤镜图已写出：天空 {sky:?} / 深海 {deep:?} / 海水温度色 {sea:?}"
        );
    }
}
