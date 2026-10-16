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

use vivarium_core::{MaterialCell, World, CHUNK_SIZE};

use crate::{palette, RenderError, Renderer};

// ---------------------------------------------------------------------------
// letterbox
// ---------------------------------------------------------------------------

/// blit 管线的 uniform：裁剪空间下的缩放与平移。
///
/// 16 字节，正好是 uniform 缓冲的最小对齐要求。
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct BlitUniforms {
    scale: [f32; 2],
    offset: [f32; 2],
}

/// 计算 letterbox：把世界按原宽高比塞进目标矩形，多余的部分留黑边。
///
/// 世界是正方形而窗口是宽屏时，宁可左右留黑边，也不要把格子拉成非正方形——
/// 像素一旦不是方的，"一个格子"这个视觉单位就崩了，玩家再也数不清自己画了几格。
///
/// 任一维度为 0 时退化为不缩放。这不是防御性编程：窗口最小化时
/// 交换链尺寸真的会是 0。
fn letterbox(world: (u32, u32), target: (u32, u32)) -> BlitUniforms {
    if world.0 == 0 || world.1 == 0 || target.0 == 0 || target.1 == 0 {
        return BlitUniforms {
            scale: [1.0, 1.0],
            offset: [0.0, 0.0],
        };
    }

    let world_aspect = world.0 as f32 / world.1 as f32;
    let target_aspect = target.0 as f32 / target.1 as f32;

    let scale = if world_aspect > target_aspect {
        // 世界比窗口更宽 → 左右顶满，上下留边。
        [1.0, target_aspect / world_aspect]
    } else {
        // 世界比窗口更高 → 上下顶满，左右留边。
        [world_aspect / target_aspect, 1.0]
    };

    BlitUniforms {
        scale,
        offset: [0.0, 0.0],
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
#[derive(Debug, Clone)]
pub struct PixelUploader {
    width: usize,
    height: usize,
    chunk_cols: usize,
    chunk_rows: usize,
    /// 上一次上传时每个格子的材质 id。
    ///
    /// 初值填 `u8::MAX`——它不是任何合法材质，于是"首帧整张都是脏的"
    /// 这件事不需要额外的标志位来记，比较结果自然就全为真。
    shadow: Vec<u8>,
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
            shadow: vec![u8::MAX; width * height],
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

        let cells = world.cells();
        let mut dirty_count = 0usize;
        for cy in 0..self.chunk_rows {
            for cx in 0..self.chunk_cols {
                let changed = self.chunk_changed(cells, cx, cy);
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
    fn chunk_changed(&self, cells: &[MaterialCell], cx: usize, cy: usize) -> bool {
        for row in 0..CHUNK_SIZE {
            let base = (cy * CHUNK_SIZE + row) * self.width + cx * CHUNK_SIZE;
            for col in 0..CHUNK_SIZE {
                if self.shadow[base + col] != cells[base + col].kind.as_u8() {
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
        let mut out = 0usize;
        for row in 0..CHUNK_SIZE {
            let base = (cy * CHUNK_SIZE + row) * self.width + cx * CHUNK_SIZE;
            for col in 0..CHUNK_SIZE {
                let i = base + col;
                let kind = cells[i].kind;
                let [r, g, b] = palette::rgb(kind);
                self.chunk_pixels[out] = r;
                self.chunk_pixels[out + 1] = g;
                self.chunk_pixels[out + 2] = b;
                self.chunk_pixels[out + 3] = 255;
                out += 4;
                self.shadow[i] = kind.as_u8();
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
    sampler: wgpu::Sampler,
    uniforms: wgpu::Buffer,

    texture: Option<wgpu::Texture>,
    view: Option<wgpu::TextureView>,
    bind_group: Option<wgpu::BindGroup>,
    uploader: Option<PixelUploader>,
    world_size: (u32, u32),
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
        let world_format = if output_format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vivarium-blit-layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(16),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
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

        // 最近邻采样，不是双线性。
        // 这是像素画的硬要求：双线性会把 1 格宽的生物糊成一团灰。
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("vivarium-world-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("vivarium-blit-uniforms"),
            size: 16,
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
            sampler,
            uniforms,
            texture: None,
            view: None,
            bind_group: None,
            uploader: None,
            world_size: (0, 0),
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

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("vivarium-blit-bind-group"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.uniforms.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        self.uploader = Some(PixelUploader::new(world));
        self.texture = Some(texture);
        self.view = Some(view);
        self.bind_group = Some(bind_group);
        self.world_size = size;
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
                let pixels = uploader.build_chunk(world, cx, cy);
                write_chunk(&self.queue, texture, cx, cy, pixels);
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
        let uniforms = letterbox(self.world_size, target_size);
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

/// 把一个 64×64 块写进世界纹理。
///
/// 独立成自由函数是为了让它不借用 `Blitter`——`upload` 里 uploader 已经
/// 可变借走了，再来一个 `&mut self` 就打架了。
fn write_chunk(queue: &wgpu::Queue, texture: &wgpu::Texture, cx: usize, cy: usize, pixels: &[u8]) {
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
            width: CHUNK_SIZE as u32,
            height: CHUNK_SIZE as u32,
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

    /// 世界纹理占多少字节。
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
        drop(target);

        // 4. 呈现。wgpu 30 把 present 挪到了 Queue 上。
        self.blitter.queue.submit(Some(encoder.finish()));
        self.blitter.queue.present(frame);

        self.last_skip = None;
        self.blitter.stats.drawn_frames += 1;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "pixel(wgpu)"
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use vivarium_core::{worldgen, MaterialKind, MaterialSim, Rng};

    // --- letterbox ------------------------------------------------------

    #[test]
    fn letterbox_keeps_square_world_square() {
        // 正方形世界放进宽屏窗口：上下顶满，左右留黑边。
        // 写反了很容易——"世界比窗口窄"对应的是**横向**缩小，不是纵向。
        let u = letterbox((512, 512), (1920, 1080));
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
        let u = letterbox((2048, 512), (1000, 1000));
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
                let u = letterbox(world, target);
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
                let u = letterbox(world, target);
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
    fn letterbox_survives_zero_sized_target() {
        // 窗口最小化时交换链尺寸真的是 0。这里不能出现 NaN 或除零。
        let u = letterbox((512, 512), (0, 0));
        assert_eq!(u.scale, [1.0, 1.0]);
        assert!(u.scale.iter().all(|v| v.is_finite()));
    }

    // --- 上传器 ---------------------------------------------------------

    fn world_with(cells: &[(isize, isize, MaterialKind)]) -> World {
        let mut w = World::new(128, 128).unwrap();
        for &(x, y, k) in cells {
            w.set(x, y, k);
        }
        w
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

    #[test]
    fn chunk_pixels_match_the_palette() {
        let mut w = World::new(128, 128).unwrap();
        w.set(3, 1, MaterialKind::Fire);
        let mut u = PixelUploader::new(&w);

        let pixels = u.build_chunk(&w, 0, 0);
        assert_eq!(pixels.len(), PixelUploader::chunk_byte_len());

        // 块内第 1 行第 3 列 → 字节偏移 (row * CHUNK_SIZE + col) * 4。
        let off = (CHUNK_SIZE + 3) * 4;
        let [r, g, b] = palette::rgb(MaterialKind::Fire);
        assert_eq!(&pixels[off..off + 4], &[r, g, b, 255]);

        // 旁边一格是空，应当是背景色。
        let off_empty = (CHUNK_SIZE + 4) * 4;
        let [r, g, b] = palette::rgb(MaterialKind::Empty);
        assert_eq!(&pixels[off_empty..off_empty + 4], &[r, g, b, 255]);
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

        let [sr, sg, sb] = palette::rgb(MaterialKind::Sand);
        let [wr, wg, wb] = palette::rgb(MaterialKind::Water);
        let row0 = &pixels[0..4];
        let row1 = &pixels[CHUNK_SIZE * 4..CHUNK_SIZE * 4 + 4];
        assert_eq!(row0, &[sr, sg, sb, 255]);
        assert_eq!(row1, &[wr, wg, wb, 255]);
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

    /// 建一个离屏 device。返回 `None` 表示这台机器上跑不了 wgpu。
    ///
    /// 这里用 `force_fallback_adapter: true`——测试要的是"管线对不对"，
    /// 不是"显卡快不快"，软件光栅化（WARP / lavapipe）完全够用，
    /// 而且在没有独显的 CI 上也能跑。
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
    }
}
