//! egui 的 GPU 与窗口接线。
//!
//! ## 为什么单独一个文件
//!
//! `vivarium-ui` 里没有任何 wgpu / winit 代码（那是有意的，见它的 crate 文档）。
//! 把它搬到屏幕上需要三件东西：喂输入（`egui-winit`）、建管线并上传纹理
//! （`egui-wgpu`）、在渲染器那一帧里画出来（`PixelRenderer::render_with`）。
//! 这三件事都只属于应用层，所以全在这个文件里。
//!
//! ## 一帧的顺序（顺序错了就会闪、会漏字、会崩）
//!
//! ```text
//! 1. take_egui_input      把攒下的窗口事件变成 RawInput
//! 2. ui.show              跑界面逻辑，得到 shapes 和 textures_delta
//! 3. handle_platform_output  光标形状、IME 位置（不处理的话输入法候选框会飘）
//! 4. tessellate           把 shapes 变成三角形（纯 CPU）
//! 5. update_texture       把字体图集等纹理传上 GPU —— 必须在 update_buffers 之前
//! 6. update_buffers       把顶点/索引传上 GPU —— 必须在 render 之前
//! 7. render              画进**世界已经画好的**那个 target（LoadOp::Load）
//! 8. free_texture         释放这一帧不用的纹理 —— 必须在 render 之后
//! ```
//!
//! 第 5 步和第 8 步是最容易搞反的：纹理还没传就画，字是白的；
//! 纹理先释放再画，wgpu 直接报验证错误。

use std::sync::Arc;

use vivarium_ui::state::{Snapshot, UiState};
use vivarium_ui::{Command, Ui};

/// 界面 + 它在 GPU 上的那一半。
pub struct Egui {
    /// 界面本身（主题、面板、状态都在它和 `UiState` 里）。
    ui: Ui,
    /// 窗口事件 → `RawInput` 的转换器。
    winit: egui_winit::State,
    /// egui 的渲染器：一张字体图集、两条顶点缓冲、一条管线。
    renderer: egui_wgpu::Renderer,
    /// 界面的可变状态（选中的工具、笔刷、控制台内容）。
    pub state: UiState,
    /// 上一帧界面算出来的中央矩形（逻辑像素）。世界该画在这里。
    central: egui::Rect,
    /// 是否处于「界面吃掉了这次输入」的状态。
    ///
    /// 主循环用它决定要不要把鼠标事件交给世界。不判断的话，
    /// 点面板上的按钮会同时在画布上画一笔。
    pub pointer_over_ui: bool,
    /// 上一帧有没有模态面板盖住世界（生物工坊）。
    ///
    /// 鼠标事件比渲染先到，所以这里只能是**上一帧**的值——一帧的延迟
    /// 在"点下去要不要落到世界上"这个问题上完全够用：模态打开的那一帧
    /// 用户还没来得及点。
    pub modal: bool,
}

impl Egui {
    /// 建好界面与 GPU 资源。
    pub fn new(
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        window: &Arc<winit::window::Window>,
    ) -> Self {
        let ui = Ui::new();
        let ctx = ui.ctx().clone();

        let winit = egui_winit::State::new(
            ctx,
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            // 纹理上限交给 egui 自己按适配器能力裁剪。传 `None` 是安全的默认。
            None,
        );

        let renderer = egui_wgpu::Renderer::new(
            device,
            format,
            egui_wgpu::RendererOptions {
                // 关掉 dithering：这套界面是大面积纯色 + 1px 线，
                // dithering 会在深色块上撒噪点，看起来像蒙了层灰。
                dithering: false,
                ..Default::default()
            },
        );

        Self {
            ui,
            winit,
            renderer,
            state: UiState::new(),
            central: egui::Rect::NOTHING,
            pointer_over_ui: false,
            modal: false,
        }
    }

    /// 上一帧算出来的中央矩形（**逻辑像素**）。世界该画在这里。
    ///
    /// 主循环每帧拿它做两件事：告诉渲染器"世界画在哪一块"，
    /// 以及判断"鼠标是不是在世界里"。
    pub fn central(&self) -> egui::Rect {
        self.central
    }

    /// 中文字体装上了没有。横幅里要说出来——装不上时界面会满屏方块。
    pub fn font(&self) -> Option<&str> {
        self.ui.font.as_deref()
    }

    /// 把窗口事件交给 egui。返回它有没有消费掉这个事件。
    pub fn on_window_event(
        &mut self,
        window: &winit::window::Window,
        event: &winit::event::WindowEvent,
    ) -> bool {
        self.winit.on_window_event(window, event).consumed
    }

    /// 跑一帧界面，并返回这一帧要执行的命令。
    ///
    /// 只做**逻辑**部分（1~4 步）。真正的绘制在 [`Self::paint`] 里，
    /// 因为那一步必须发生在渲染器拿到交换链帧之后。
    pub fn run_frame(&mut self, window: &winit::window::Window, snap: &Snapshot) -> Frame {
        // 1. 输入。
        let input = self.winit.take_egui_input(window);

        // 2. 界面。
        let (out, full) = self.ui.show(&mut self.state, snap, input);
        self.central = out.central;

        // 3. 光标形状 / IME。不处理的话输入法候选框会飘到左上角。
        self.winit
            .handle_platform_output(window, full.platform_output.clone());

        // 4. 三角化。纯 CPU，和 GPU 无关。
        let paint_jobs = self.ui.ctx().tessellate(full.shapes, full.pixels_per_point);

        self.modal = out.modal;
        // 界面盖住了鼠标 = 鼠标在中央矩形之外。
        self.pointer_over_ui = !self.central.contains(
            self.ui
                .ctx()
                .pointer_latest_pos()
                .unwrap_or(egui::pos2(-1.0, -1.0)),
        );

        Frame {
            commands: out.commands,
            paint_jobs,
            textures_delta: full.textures_delta,
            pixels_per_point: full.pixels_per_point,
        }
    }

    /// 把一帧界面画到**已经画好世界的**那个 target 上。
    pub fn paint(
        &mut self,
        frame: Frame,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        size: (u32, u32),
    ) {
        let screen = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [size.0, size.1],
            pixels_per_point: frame.pixels_per_point,
        };

        // 5. 纹理。字体图集是第一帧建的，之后只在加了新字形时增量更新。
        //
        // 0.37 里 `set` 是 `(TextureId, SmallVec<ImageDelta>)`：同一个纹理
        // 一帧可能有**多个**增量（字体图集分块扩容时就是这样），
        // 只取第一个会丢字形——表现是"有些字永远显示不出来"。
        for (id, deltas) in &frame.textures_delta.set {
            for delta in deltas {
                self.renderer.update_texture(device, queue, *id, delta);
            }
        }

        // 6. 顶点 / 索引。
        self.renderer
            .update_buffers(device, queue, encoder, &frame.paint_jobs, &screen);

        // 7. 画。
        let desc = wgpu::RenderPassDescriptor {
            label: Some("vivarium-ui"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                resolve_target: None,
                ops: wgpu::Operations {
                    // **Load 不是 Clear**：世界已经画在这里了，清掉就只剩界面。
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            // wgpu 30 新加的字段。界面不做立体渲染，永远是 `None`。
            multiview_mask: None,
        };
        let pass = encoder.begin_render_pass(&desc);
        // egui 的 `render` 要 `RenderPass<'static>`（它把资源引用藏在内部，
        // 声明"这些资源活到 pass 结束"）。`forget_lifetime` 就是这个声明，
        // 而且是**消费 self 返回新值**，不是原地改。
        let mut pass = pass.forget_lifetime();
        self.renderer.render(&mut pass, &frame.paint_jobs, &screen);
        drop(pass);

        // 8. 释放。**必须在画完之后**——先释放再画就是拿一张已经销毁的纹理。
        for id in &frame.textures_delta.free {
            self.renderer.free_texture(id);
        }
    }

    /// 日志。主循环在发生事件时调，控制台里就能看到。
    pub fn log(&mut self, tick: u64, level: vivarium_ui::Level, text: impl Into<String>) {
        self.state.log(tick, level, text);
    }

    /// 执行一条命令。**不认识的命令返回 false**，由主循环自己处理
    /// （或者忽略）——这样加新命令时不会被迫在这里穷举。
    pub fn apply(&mut self, cmd: Command) -> bool {
        match cmd {
            Command::TogglePause => false,
            Command::Step => false,
            Command::SetScale(_) => false,
            Command::Reseed => false,
            Command::Drop(_) => false,
            Command::Ignite => false,
            Command::CenterView => false,
            Command::Zoom(_) => false,
            Command::Select(id) => {
                self.state.selected = id;
                true
            }
            Command::SetTool(tool) => {
                self.state.tool = tool;
                true
            }
            Command::SetBrush(n) => {
                self.state.brush = n;
                true
            }
            Command::SetMaterial(m) => {
                self.state.material = m;
                true
            }
            Command::SetCell(c) => {
                self.state.cell = c;
                true
            }
            Command::RunFor(_) => false,
            // 工坊的三条命令都要写世界 / 写磁盘，界面自己做不了。
            Command::PlaceBody(_) => false,
            Command::SaveBody { .. } => false,
            Command::DeleteBody(_) => false,
            Command::Quit => false,
        }
    }
}

/// 一帧界面里"要交给 GPU 的那一半"。
///
/// 分成 [`Egui::run_frame`] 和 [`Egui::paint`] 两步，是因为绘制必须发生在
/// 渲染器拿到交换链帧之后——而那一步在 `render_with` 的回调里，
/// 中间隔着一次 `&mut` 借用。拆开之后两边各自只借自己需要的东西。
pub struct Frame {
    /// 要执行的命令。
    pub commands: Vec<Command>,
    paint_jobs: Vec<egui::ClippedPrimitive>,
    textures_delta: egui::TexturesDelta,
    pixels_per_point: f32,
}
