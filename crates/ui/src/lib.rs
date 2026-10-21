//! # vivarium-ui
//!
//! 界面层：主题、自绘控件、面板骨架。
//!
//! ## 三条边界
//!
//! 1. **不依赖 wgpu / winit。** 这里只有 `egui`。把三角形提交给 GPU 是
//!    `bins/game` 的事——和 `vivarium-render` 不依赖 winit 是同一条理由：
//!    让"界面长什么样"可以在没有显卡的机器上被测试和预览。
//! 2. **不认识模拟。** 输入是 [`state::Snapshot`]（只读快照），
//!    输出是 [`state::Command`]（要主循环去做的事）。界面里没有
//!    `&mut World`，所以"点了暂停"这件事能单独测。
//! 3. **不自带配色。** 材质和细胞的颜色一律取自
//!    [`vivarium_core::palette`]——底部调色板上的色块必须和世界里
//!    那一格**完全同色**，否则"点这个会画出什么"要重新学一遍。
//!
//! ## 参照
//!
//! **布局照 ALIEN**：左工具 / 中视口 / 右检查器 / 下控制台。
//! 那三个分区对应"操作 / 观察 / 追溯"三种动作，不是随便分的。
//!
//! **皮肤照 The Powder Toy**：纯黑面板、1px 实线边框、悬停提亮、按下整体反色。
//! 没有圆角、没有阴影、没有渐变——世界里每一格像素都是有意义的，
//! 界面不该有装饰性的柔和边缘。
//!
//! ## 现状：骨架
//!
//! 面板、主题、控件都是真的，能跑、能点、能显示真实数据。但**落到世界上的
//! 笔刷还没接**：工具选中状态是真的，`SetMaterial` / `SetBrush` 这类命令
//! 已经发出来了，主循环收到之后先记日志。把命令接到材质层的写入上
//! 是下一步。

pub mod panels;
pub mod state;
pub mod theme;
pub mod widgets;

pub use state::{Command, Console, Level, Snapshot, Tool, UiState};
pub use widgets::BtnState;

/// 一帧的产出。
///
/// 主循环只关心两件事：**要不要重绘**（`repaint`）和**要做什么**
/// （`commands`）。前者是真的需要：egui 是惰性的，没有输入就不重绘，
/// 而这里的界面要显示每帧变化的 tick 和 fps，所以每帧都得说"再画一次"。
#[derive(Debug)]
pub struct Output {
    /// 这一帧攒下的命令，按发生顺序。
    pub commands: Vec<Command>,
    /// 下一帧是否需要立刻重绘。
    pub repaint: bool,
    /// 中央可用矩形（逻辑像素）。世界该画在这里。
    pub central: egui::Rect,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            repaint: true,
            central: egui::Rect::NOTHING,
        }
    }
}

/// 界面的入口。
///
/// 只装了一个 `egui::Context`——它是 egui 的全部状态（字体、动画时间、
/// 输入、上一次的布局记忆）。持有它而不是每帧新建，是因为**布局记忆**
/// （滚动位置、面板宽度）就存在里面。
pub struct Ui {
    ctx: egui::Context,
    /// 上一次成功装载的中文字体路径。装不上就是 `None`，
    /// 那时界面标签会变成方块——启动横幅里会说出来。
    pub font: Option<String>,
}

impl Ui {
    /// 新建，并装好主题与字体。
    pub fn new() -> Self {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let font = theme::install_cjk_font(&ctx);
        Self { ctx, font }
    }

    /// 底层的 `egui::Context`。接线层要用它来喂输入、取纹理增量。
    pub fn ctx(&self) -> &egui::Context {
        &self.ctx
    }

    /// 跑一帧界面。
    ///
    /// `raw_input` 由 `egui-winit` 从窗口事件里攒出来。界面自己**不碰**
    /// 窗口事件——那样它就得认识 winit，而 winit 是应用层的东西。
    pub fn show(
        &mut self,
        state: &mut UiState,
        snap: &Snapshot,
        raw_input: egui::RawInput,
    ) -> (Output, egui::FullOutput) {
        let mut central = egui::Rect::NOTHING;
        let full = self.ctx.run_ui(raw_input, |ui| {
            central = panels::draw(ui, state, snap);
        });

        let commands = state.take_commands();
        (
            Output {
                commands,
                // 每帧都重绘：tick 和 fps 一直在变，省这一帧只会让数字看起来卡住。
                repaint: true,
                central,
            },
            full,
        )
    }
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1100.0, 840.0),
            )),
            ..Default::default()
        }
    }

    /// 一帧界面能在**没有显卡**的情况下跑完。
    ///
    /// 这条测试是这一层不依赖 wgpu 的回报：布局、配色、控件逻辑全都能
    /// 在纯 CPU 上验证，CI 里不需要任何图形环境。
    #[test]
    fn a_frame_runs_headless() {
        let mut ui = Ui::new();
        let mut state = UiState::new();
        let snap = Snapshot {
            organisms: 42,
            cells: 480,
            killers: 3,
            history: vec![0.1, 0.3, 0.2, 0.6, 0.9],
            ..Default::default()
        };

        let (out, mut full) = ui.show(&mut state, &snap, screen());
        assert!(out.repaint);
        // epaint 的 `TexturesDelta` 带"必须被消费"的断言：不交给 GPU 就 drop
        // 会 panic。真实运行时由 `egui-wgpu` 上传，测试里手动清掉。
        full.textures_delta.clear();

        // 真的产生了可绘制的图元——不是"跑通了但什么都没画"。
        let prims = ui.ctx().tessellate(full.shapes, 1.0);
        assert!(!prims.is_empty(), "一帧界面应当产出可绘制的图元");

        let verts: usize = prims
            .iter()
            .map(|p| match &p.primitive {
                egui::epaint::Primitive::Mesh(m) => m.vertices.len(),
                _ => 0,
            })
            .sum();
        assert!(verts > 1000, "界面太简陋了：只有 {verts} 个顶点");
    }

    /// 面板不能把整个窗口盖满——中间必须留给世界。
    ///
    /// 这一条是布局的**核心不变量**：世界是主角，界面是框。如果哪天
    /// 加面板加过头把中央盖住了，画面会变成"一堆面板"，而且不会报错。
    #[test]
    fn the_centre_stays_free_for_the_world() {
        let mut ui = Ui::new();
        let mut state = UiState::new();
        let snap = Snapshot::default();
        let (out, mut full) = ui.show(&mut state, &snap, screen());
        full.textures_delta.clear();

        let expect_w = 1100.0 - theme::TOOL_STRIP_W - theme::INSPECTOR_W;
        let expect_h = 840.0 - theme::TOP_BAR_H - theme::PALETTE_H - theme::CONSOLE_H;
        assert!(
            out.central.width() >= expect_w - 1.0,
            "中央宽度 {} 小于预期 {expect_w}",
            out.central.width()
        );
        assert!(
            out.central.height() >= expect_h - 1.0,
            "中央高度 {} 小于预期 {expect_h}",
            out.central.height()
        );
    }
}
