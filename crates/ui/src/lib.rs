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
pub mod workshop;

pub use state::{Command, Console, Level, LibraryEntry, Snapshot, Tool, UiState, Workshop};
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
    /// 这一帧有没有模态面板盖住世界（生物工坊）。
    ///
    /// 主循环据此决定"鼠标点下去要不要落到世界上"。不能只看
    /// `pointer_over_ui`：模态面板周围的遮罩是**画上去的**，不是控件，
    /// egui 不会把它算进 `pointer_over_ui`，于是点空白处会顺手在世界里涂一笔。
    pub modal: bool,
}

impl Default for Output {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            repaint: true,
            central: egui::Rect::NOTHING,
            modal: false,
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
        let mut modal = false;
        let full = self.ctx.run_ui(raw_input, |ui| {
            central = panels::draw(ui, state, snap);
            // 工坊是**模态**：它画在所有面板之上，并且吃掉这一帧的鼠标。
            // 放在 `panels::draw` 之后是因为它要盖住面板；用 `Area` 的
            // `Foreground` 层，所以不受面板布局约束。
            modal = workshop::draw(ui, state, snap);
        });

        let commands = state.take_commands();
        (
            Output {
                commands,
                // 每帧都重绘：tick 和 fps 一直在变，省这一帧只会让数字看起来卡住。
                repaint: true,
                central,
                modal,
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
    use crate::state::{CellInfo, HoverOrganism};
    use vivarium_core::{CellType, MaterialKind, ViewMode};

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

    /// 工坊的画布与细胞表**往返一致**：画几格、转成细胞表、再载回来，
    /// 格子还在原处。
    ///
    /// 这条守的是"锚点对齐"——`to_cells` 把包围盒中心当锚点，
    /// `load` 又要把它还原到画布中心。两边差一格的话，症状是
    /// "存进去的身体再载出来会往右下角挪一点"，而画面上很难发现，
    /// 因为每一次挪动都很小。
    #[test]
    fn workshop_round_trips_through_cell_table() {
        let mut w = state::Workshop::default();
        w.set(4, 5, Some(CellType::Mover));
        w.set(5, 5, Some(CellType::Mouth));
        w.set(4, 4, Some(CellType::Producer));

        let cells = w.to_cells();
        assert_eq!(cells.len(), 3);
        // 锚点是包围盒中心，向下取整：(4,4)-(5,5) → (4,4)。
        // 于是 (4,4) 生产者是 (0,0)、(5,5) 嘴是 (1,1)、(4,5) 移动是 (0,1)。
        assert!(cells.contains(&(0, 0, CellType::Producer.as_u8())));
        assert!(cells.contains(&(1, 1, CellType::Mouth.as_u8())));
        assert!(cells.contains(&(0, 1, CellType::Mover.as_u8())));

        let mut again = state::Workshop::default();
        again.load(&cells);
        assert_eq!(again.count(), 3, "载回来少了几格");
        let mut a = again.to_cells();
        let mut b = cells.clone();
        a.sort_unstable();
        b.sort_unstable();
        assert_eq!(a, b, "载回来形状变了");
    }

    /// 打开工坊会**多画一堆东西**——遮罩、面板、网格线、色块。
    ///
    /// 数的是 `shapes` 的条数，不是 tessellate 之后的顶点数：`Context`
    /// 会按 shape 的哈希缓存三角化结果，两次调用之间拿到的顶点数不可靠
    /// （实测画了 97 个新 shape，顶点数却只涨了 8）。
    /// 而"多画了 97 个图元"本身就是"面板真的显示了"的直接证据。
    #[test]
    fn opening_the_workshop_draws_a_lot_more() {
        let mut ui = Ui::new();
        let mut state = UiState::new();
        let snap = Snapshot::default();

        let (_, mut full) = ui.show(&mut state, &snap, screen());
        let before = full.shapes.len();
        full.textures_delta.clear();

        state.workshop.open = true;
        state.workshop.set(3, 3, Some(CellType::Producer));
        let (out, mut full) = ui.show(&mut state, &snap, screen());
        let after = full.shapes.len();
        full.textures_delta.clear();

        assert!(out.modal, "工坊打开时必须报 modal，否则点击会漏到世界上");
        assert!(
            after > before + 50,
            "工坊应当画出网格、色块和按钮：{before} → {after}"
        );

        // 图元也不能被裁没。`clip_rect` 宽度为负是"布局把它推到屏幕外"的
        // 典型症状，而那种情况下画了等于没画。
        assert!(
            full.shapes.iter().all(|s| s.clip_rect.width() >= 0.0),
            "有图元的裁剪矩形宽度为负——说明它被布局挤出了屏幕"
        );
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

    fn hover_cell() -> CellInfo {
        CellInfo {
            x: 214,
            y: 137,
            material: MaterialKind::Water,
            organism: Some(HoverOrganism {
                id: 312,
                cells: 12,
                generation: 7,
                energy: (412, 480),
                producers: 6,
                cell: CellType::Producer,
            }),
            light: 0.71,
            temp: 20.3,
            depth: 42,
        }
    }

    /// 悬停读数**只在有内容时画**，而且不能画到屏幕外。
    ///
    /// 两个方向都要守：没有 `hover` 时一个图元都不该多（否则它会在
    /// 世界里一直挂着一个空框）；有 `hover` 时必须真的多出框和文字。
    #[test]
    fn the_hover_readout_draws_only_when_there_is_something_under_the_pointer() {
        let mut ui = Ui::new();
        let mut state = UiState::new();
        let mut snap = Snapshot::default();
        let pointer = egui::pos2(400.0, 300.0);
        let input = || egui::RawInput {
            events: vec![egui::Event::PointerMoved(pointer)],
            ..screen()
        };

        let (_, mut full) = ui.show(&mut state, &snap, input());
        let plain = full.shapes.len();
        full.textures_delta.clear();

        snap.hover = Some(hover_cell());
        let (_, mut full) = ui.show(&mut state, &snap, input());
        let hovered = full.shapes.len();
        let widest = full
            .shapes
            .iter()
            .map(|s| s.shape.visual_bounding_rect().max.x)
            .fold(0.0_f32, f32::max);
        full.textures_delta.clear();

        assert!(
            hovered > plain,
            "悬停时应当多画一个框和五行字：{plain} → {hovered}"
        );
        assert!(widest <= 1100.5, "读数被画到了屏幕外：右边界 {widest}");
    }

    /// 读数框贴边时**翻到另一边**，不是被夹住。
    ///
    /// 玩家连着报过两次"切视图时提示挡住 UI"：第一次是 tooltip，
    /// 第二次就是这个框——它靠上边界时被 `max(central.top())` 夹住，
    /// 正好按在指针正上方，看着像顶栏下面挂了一条提示。
    ///
    /// 判据用"多出来的那个矩形"：同一个快照渲两帧，差集里就是读数框。
    #[test]
    fn the_readout_flips_instead_of_being_clamped() {
        let mut ui = Ui::new();
        let mut state = UiState::new();
        let mut snap = Snapshot::default();

        let render = |ui: &mut Ui, state: &mut UiState, snap: &Snapshot, at: egui::Pos2| {
            let input = egui::RawInput {
                events: vec![egui::Event::PointerMoved(at)],
                ..screen()
            };
            let (_, mut full) = ui.show(state, snap, input);
            let rects: Vec<egui::Rect> = full
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Rect(r) => Some(r.rect),
                    _ => None,
                })
                .collect();
            full.textures_delta.clear();
            rects
        };

        // 指针贴着世界顶部（顶栏下面一点）。
        let high = egui::pos2(520.0, 60.0);
        let plain = render(&mut ui, &mut state, &snap, high);
        snap.hover = Some(hover_cell());
        let with = render(&mut ui, &mut state, &snap, high);
        let added: Vec<egui::Rect> = with.into_iter().filter(|r| !plain.contains(r)).collect();
        let boxed = added
            .iter()
            .find(|r| r.width() > 40.0 && r.height() > 30.0)
            .copied()
            .expect("悬停时应当多画一个读数框");
        assert!(
            boxed.top() > high.y,
            "上面放不下时应当翻到指针下面，实测框顶 {} / 指针 {}",
            boxed.top(),
            high.y
        );

        // 指针在世界中间：框还是应该在指针**上方**（不要为了修这个把它一律翻下去）。
        let mid = egui::pos2(520.0, 420.0);
        snap.hover = None; // 先把框关掉，否则"差集"是空的
        let plain = render(&mut ui, &mut state, &snap, mid);
        snap.hover = Some(hover_cell());
        let with = render(&mut ui, &mut state, &snap, mid);
        let added: Vec<egui::Rect> = with.into_iter().filter(|r| !plain.contains(r)).collect();
        let boxed = added
            .iter()
            .find(|r| r.width() > 40.0 && r.height() > 30.0)
            .copied()
            .expect("悬停时应当多画一个读数框");
        assert!(
            boxed.bottom() < mid.y,
            "世界中间时框应当仍在指针上方，实测框底 {} / 指针 {}",
            boxed.bottom(),
            mid.y
        );
    }

    /// 换滤镜**不许动世界的矩形**。
    ///
    /// 滤镜只改 shader 里的分支，不该参与布局。要是哪天某个滤镜按钮
    /// 换成了更宽的标签、把顶栏撑高一行，世界就会在切换滤镜时跳一下——
    /// 而那种跳动看起来像"渲染抖动"，没人会想到是布局。
    #[test]
    fn switching_the_filter_does_not_move_the_world() {
        let mut ui = Ui::new();
        let mut state = UiState::new();
        let snap = Snapshot::default();

        let mut rects = Vec::new();
        for mode in ViewMode::ALL {
            state.view = mode;
            let (out, mut full) = ui.show(&mut state, &snap, screen());
            full.textures_delta.clear();
            rects.push((mode.label(), out.central));
        }

        let first = rects[0].1;
        for (label, rect) in &rects {
            assert_eq!(*rect, first, "切到「{label}」之后世界矩形变了");
        }
    }
}
