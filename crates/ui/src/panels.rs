//! 五个面板的骨架。
//!
//! ## 布局
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │ 顶栏：标识 · tick · 状态 · 速度 · 帧率                     │
//! ├────┬────────────────────────────────────┬────────────────┤
//! │ 工 │                                    │                │
//! │ 具 │        世界（wgpu 直接画，           │   检查器        │
//! │ 条 │        egui 不参与）                │                │
//! ├────┴────────────────────────────────────┴────────────────┤
//! │ 调色板：材质 · 细胞 · 笔刷                                 │
//! ├──────────────────────────────────────────────────────────┤
//! │ 控制台                                                    │
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! ## 为什么是这个形状
//!
//! **结构照 Alien**：左边工具、中间视口、右边检查器、下面控制台。
//! Alien 是同类项目里把"看一个进化中的世界"这件事做得最完整的，
//! 它的分区不是随便定的——左手的工具、右手的数据、下面的日志，
//! 恰好对应"操作 / 观察 / 追溯"三种动作。
//!
//! **顺序照 TPT**：调色板在视口正下方、紧挨着画布，而不是塞进右侧面板。
//! TPT 用二十年证明了"手在下面、眼在中间"是画布类程序最顺的姿势：
//! 选材质的时候视线不离开画面。
//!
//! ## 中央区域是空的
//!
//! 世界由 `vivarium-render` 直接 blit 到交换链上，egui 只在它周围画框。
//! 中央那块的空白是**故意的**：没有 `CentralPanel`，就没有任何东西
//! 盖住世界。`egui` 只在面板矩形内接收输入，所以世界仍然可以点击。

use egui::{Align, Color32, CornerRadius, Frame, Layout, Margin, Panel, ScrollArea, Stroke, Ui};

use vivarium_core::{palette, CellType, MaterialKind, TimeScale, ViewMode};

use crate::state::{CellInfo, Command, Level, SelectedOrganism, Snapshot, Tool, UiState};
use crate::theme;
use crate::widgets::{self, BtnState, Icon};

/// 画一整套界面。
///
/// 参数是**根 `Ui`**（egui 0.37 的 `Context::run_ui` 给的那个），
/// 不是 `Context`：0.37 把面板 API 从 `ctx` 挪到了 `ui` 上，
/// 面板要挂在一个 `Ui` 下面，而不是直接挂在上下文上。
/// 返回值是**剩下的中央矩形**——世界该画在哪一块。
///
/// 这个矩形必须由界面算出来，不能让主循环自己猜：面板尺寸一旦变了，
/// 猜的那个值就错了，而错了的症状是"世界的边缘被面板压住一条"，
/// 很难一眼看出是布局问题。
pub fn draw(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) -> egui::Rect {
    top_bar(ui, state, snap);
    // 顺序有意义：先加控制台（贴最底），再加调色板（贴在控制台上方）。
    // 反过来的话调色板会跑到屏幕最下边。
    if state.show_console {
        console(ui, state, snap);
    }
    palette(ui, state, snap);
    tool_strip(ui, state, snap);
    if state.show_inspector {
        inspector(ui, state, snap);
    }
    let central = ui.available_rect_before_wrap();
    brush_cursor(ui, state, snap, central);
    hover_readout(ui, state, snap, central);
    central
}

/// 指针下面那一格的属性：材质、物种、温度、光照、坐标。
///
/// ## 为什么是浮在指针旁边的框，而不是检查器里的一行
///
/// 因为"看一格"是一个**移动中的**动作：鼠标划过一片海，你想知道
/// 这一格和上一格差多少。把读数放在右侧面板里，视线要来回跳四千像素，
/// 跳一次就忘了上一次的数。
///
/// ## 为什么贴右上角而不是正右下
///
/// 正右下方会盖住"接下来要看的下一格"——鼠标往右下走的时候，
/// 框永远压着它。贴右上，指针前进的方向始终是空的。
fn hover_readout(ui: &mut Ui, state: &UiState, snap: &Snapshot, central: egui::Rect) {
    let Some(info) = snap.hover.as_ref() else {
        return;
    };
    let Some(pos) = ui.ctx().pointer_latest_pos() else {
        return;
    };
    if !central.contains(pos) {
        return;
    }
    // 缩放工具在"点哪儿是哪格"上已经够忙了，读数只在没在落笔时出现，
    // 免得画一笔的时候旁边一直有个框跟着抖。
    if state.tool.paints() && ui.ctx().input(|i| i.pointer.primary_down()) {
        return;
    }

    let lines = cell_lines(info);
    let font = egui::FontId::new(9.5, egui::FontFamily::Proportional);
    let mut width: f32 = 96.0;
    let mut galleys = Vec::with_capacity(lines.len());
    for (text, color) in &lines {
        let g = ui
            .painter()
            .layout_no_wrap(text.clone(), font.clone(), *color);
        width = width.max(g.size().x + 14.0);
        galleys.push(g);
    }
    let line_h = 13.0;
    let height = line_h * galleys.len() as f32 + 8.0;

    // 贴指针的右上角；放不下就翻到另一边。
    //
    // **两条边都要"翻"，不能"夹"。** 第一版只处理了右边，上下用
    // `max(central.top())` 夹住——指针贴着顶栏时，框被按在指针正上方，
    // 正好盖住你刚切完滤镜要看的那一片（玩家报的"切视图时提示挡住界面"
    // 就是这个：那个框浮在顶栏下面，看着就像一条提示）。
    let gap = 18.0;
    let mut x = pos.x + gap;
    let mut y = pos.y - height - 8.0;
    if x + width > central.right() {
        x = pos.x - gap - width; // 右边放不下 → 翻到左边
    }
    if y < central.top() {
        y = pos.y + gap; // 上面放不下 → 翻到下面
    }
    // 翻完之后如果两边都放不下（世界很矮），才允许贴边——这时已经无解了，
    // 但至少不要画出世界。
    let max_x = (central.right() - width).max(central.left());
    let max_y = (central.bottom() - height).max(central.top());
    let anchor = egui::pos2(x.clamp(central.left(), max_x), y.clamp(central.top(), max_y));
    let rect = egui::Rect::from_min_size(anchor, egui::vec2(width, height));

    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, theme::PANEL);
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::LINE_STRONG),
        egui::StrokeKind::Inside,
    );
    for (i, g) in galleys.into_iter().enumerate() {
        p.galley(
            egui::pos2(rect.left() + 6.0, rect.top() + 4.0 + i as f32 * line_h),
            g,
            theme::TEXT,
        );
    }
}

/// 读数的内容。颜色分开给，因为"这是材质"和"这是生物"得一眼分得开。
fn cell_lines(info: &CellInfo) -> Vec<(String, Color32)> {
    let mut out = Vec::with_capacity(8);
    out.push((
        format!("({}, {})  ·  深 {} 格", info.x, info.y, info.depth),
        theme::TEXT_FAINT,
    ));
    match &info.organism {
        Some(o) => out.push((
            format!(
                "#{} {} · {} 格 · {} 代",
                o.id,
                cell_label(o.cell),
                o.cells,
                o.generation
            ),
            theme::ACCENT,
        )),
        None => out.push((material_label(info.material).to_owned(), theme::TEXT)),
    }
    if let Some(o) = &info.organism {
        out.push((
            format!("能量 {}/{} · 生产者 {}", o.energy.0, o.energy.1, o.producers),
            theme::TEXT_DIM,
        ));
    } else {
        out.push((info.material.describe().to_owned(), theme::TEXT_DIM));
    }
    out.push((
        format!("光照 {:.2}", info.light),
        light_color(info.light),
    ));
    out.push((format!("温度 {:.0}°C", info.temp), temp_color(info.temp)));
    out
}

/// 光照值 → 一个小色块用的颜色。和滤镜的色带同一个意图，只是简化了。
fn light_color(light: f32) -> Color32 {
    let t = light.clamp(0.0, 1.0);
    let v = (40.0 + 215.0 * t) as u8;
    Color32::from_rgb(v / 3, v, (v as f32 * 0.9) as u8)
}

/// 温度 → 颜色。蓝冷红热，20°C 附近是中性灰蓝。
fn temp_color(temp: f32) -> Color32 {
    let t = (temp / 600.0).clamp(0.0, 1.0);
    if t < 0.5 {
        let k = t / 0.5;
        Color32::from_rgb(
            (60.0 + 100.0 * k) as u8,
            (140.0 + 100.0 * k) as u8,
            (230.0 - 40.0 * k) as u8,
        )
    } else {
        let k = (t - 0.5) / 0.5;
        Color32::from_rgb(
            (160.0 + 95.0 * k) as u8,
            (240.0 - 200.0 * k) as u8,
            (190.0 - 190.0 * k) as u8,
        )
    }
}

/// 画笔预览：在指针位置画一个圈，半径就是笔刷半径。
///
/// 没有它，落笔之前你不知道自己会画多大一片——而"多涂了三格水"
/// 和"少涂了三格"在画面上都不明显，只有能量账知道。
///
/// 圈画在**中央矩形之内**才显示：指针在面板上时，它下面根本没有世界。
fn brush_cursor(ui: &mut Ui, state: &UiState, snap: &Snapshot, central: egui::Rect) {
    if !state.tool.paints() {
        return;
    }
    let Some(pos) = ui.ctx().pointer_latest_pos() else {
        return;
    };
    if !central.contains(pos) {
        return;
    }
    let r = state.brush as f32 * snap.pixels_per_cell.max(0.5);
    let p = ui.painter();
    // 两层：外圈用亮色（在任何材质上都看得见），内圈用暗色衬底，
    // 免得落在浅色的沙子上时整个圈消失。
    p.circle_stroke(pos, r, Stroke::new(1.0, Color32::from_black_alpha(160)));
    p.circle_stroke(pos, r.max(1.0), Stroke::new(1.0, Color32::WHITE));
    // 中心一个小十字，标出"真正落笔的那一格"。
    let c = 3.0;
    p.line_segment(
        [egui::pos2(pos.x - c, pos.y), egui::pos2(pos.x + c, pos.y)],
        Stroke::new(1.0, Color32::WHITE),
    );
    p.line_segment(
        [egui::pos2(pos.x, pos.y - c), egui::pos2(pos.x, pos.y + c)],
        Stroke::new(1.0, Color32::WHITE),
    );
}

// ---------------------------------------------------------------------------
// 顶栏
// ---------------------------------------------------------------------------

/// 面板统一的外框。
///
/// 刻意**不带描边**：`Frame` 的 `stroke` 是四条边全画，两个面板贴在一起时
/// 会得到 2px 的缝，看起来像漏光。分界线一律用 `hairline_*` 手工画一条，
/// 这样每条边界只被画一次，而且位置精确到 0.5px（对齐物理像素）。
fn panel_frame(fill: Color32, pad: Margin) -> Frame {
    Frame::NONE.fill(fill).inner_margin(pad)
}

/// 在 `ui` 的当前矩形**底边**画一条 1px 线。顶栏与调色板用它分界。
fn hairline_bottom(ui: &mut Ui) {
    let r = ui.max_rect();
    ui.painter().line_segment(
        [
            egui::pos2(r.left(), r.bottom() - 0.5),
            egui::pos2(r.right(), r.bottom() - 0.5),
        ],
        Stroke::new(1.0, theme::LINE),
    );
}

/// 在 `ui` 的当前矩形**顶边**画一条 1px 线。底部面板用它分界。
fn hairline_top(ui: &mut Ui) {
    let r = ui.max_rect();
    ui.painter().line_segment(
        [
            egui::pos2(r.left(), r.top() + 0.5),
            egui::pos2(r.right(), r.top() + 0.5),
        ],
        Stroke::new(1.0, theme::LINE),
    );
}

fn top_bar(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) {
    Panel::top("vivarium-top")
        .exact_size(theme::TOP_BAR_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(panel_frame(
            theme::PANEL,
            Margin {
                left: 6,
                right: 6,
                top: 4,
                bottom: 4,
            },
        ))
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                logo(ui);
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new("VIVARIUM")
                        .size(12.0)
                        .color(theme::TEXT)
                        .strong(),
                );
                ui.add_space(8.0);
                sep(ui);
                ui.add_space(4.0);

                // tick：等宽，且**不分组千位**——分组会让数字宽度变化，
                // 每 tick 都在抖。
                ui.label(
                    egui::RichText::new(format!("tick {:>7}", snap.tick))
                        .monospace()
                        .color(theme::TEXT_DIM),
                );
                ui.add_space(8.0);

                // 状态灯。
                let (dot, word) = if snap.scale.is_paused() {
                    (theme::WARN, "暂停")
                } else {
                    (theme::ACCENT, "运行")
                };
                let (r, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                ui.painter().circle_filled(r.center(), 3.0, dot);
                ui.label(egui::RichText::new(word).color(dot));

                ui.add_space(8.0);
                sep(ui);
                ui.add_space(4.0);

                // 速度档。四个都是开关，选中的那个高亮——
                // 这比一个下拉框好：速度是高频操作，多一次点击都是浪费。
                let mut want: Option<TimeScale> = None;
                for (label, scale) in [
                    ("1×", TimeScale::X1),
                    ("5×", TimeScale::X5),
                    ("50×", TimeScale::X50),
                    ("MAX", TimeScale::Max),
                ] {
                    let on = !snap.scale.is_paused() && snap.scale == scale;
                    if widgets::flat_button(
                        ui,
                        label,
                        if on {
                            BtnState::Selected
                        } else {
                            BtnState::Normal
                        },
                    )
                    .clicked()
                    {
                        want = Some(scale);
                    }
                }
                if let Some(scale) = want {
                    state.emit(Command::SetScale(scale));
                }

                // 单步：只在暂停时有意义，所以暂停时才可点。
                let step_state = if snap.scale.is_paused() {
                    BtnState::Normal
                } else {
                    BtnState::Disabled
                };
                if widgets::flat_button(ui, "单步", step_state).clicked() {
                    state.emit(Command::Step);
                }

                ui.add_space(8.0);
                sep(ui);
                ui.add_space(4.0);

                // 显示滤镜。照着 TPT 的 `DisplayMode` 那一排开关：
                // 同一份世界数据，换一种"看的方式"。
                //
                // 做成**单选**而不是复选：材质 / 温度 / 光照 / 物种 是四张
                // 互斥的图，叠在一起谁也看不清。TPT 的复选是因为它有
                // 气压、速度、涡度这些**可以叠加**的空气通道，而我们没有。
                ui.label(
                    egui::RichText::new("视图 V")
                        .size(theme::TITLE_SIZE)
                        .color(theme::TEXT_FAINT),
                );
                let mut want_view: Option<ViewMode> = None;
                for mode in ViewMode::ALL {
                    let on = state.view == mode;
                    let resp = widgets::flat_button(
                        ui,
                        mode.label(),
                        if on { BtnState::Selected } else { BtnState::Normal },
                    );
                    if resp.clicked() {
                        want_view = Some(mode);
                    }
                    // **不给 tooltip，也不往控制台写一行。**
                    //
                    // 顶栏在最上面，tooltip 只能往下弹，正好压在世界上；
                    // 而切滤镜恰恰是最需要看世界的时刻。玩家连着报过两次
                    // "切换视图的提示挡住 UI"，所以这次一个字都不弹：
                    // 热键直接写进标题（"视图 V"），名字自己就说明了看什么。
                    //
                    // 通则：**说明不该出现在你正要看的那个地方。**
                }
                if let Some(mode) = want_view {
                    state.view = mode;
                    state.emit(Command::SetViewMode(mode));
                }

                // 右侧：帧率与成本。用 `right_to_left` 让它们贴右。
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("{:.0} fps", snap.fps))
                            .monospace()
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(6.0);
                    sep(ui);
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!("{:.1} ms/tick", snap.ms_per_tick))
                            .monospace()
                            .color(theme::TEXT_DIM),
                    );
                    ui.add_space(6.0);
                    sep(ui);
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "{}² · {:.1} px/格",
                            snap.world_size.0, snap.pixels_per_cell
                        ))
                        .monospace()
                        .color(theme::TEXT_DIM),
                    );
                });
            });
            hairline_bottom(ui);
        });
}

/// 标识：一个 3×3 的小格子阵，颜色取自细胞调色板。
///
/// 用**真实的细胞配色**画，而不是设计一个 logo——它顺便成了一个
/// "调色板有没有被改坏"的肉眼检查点：绿橙青黄紫，顺序永远是这五个。
fn logo(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
    let p = ui.painter();
    let cell = 4.0;
    let kinds = [
        CellType::Producer,
        CellType::Mouth,
        CellType::Mover,
        CellType::Eye,
        CellType::Killer,
    ];
    for (i, kind) in kinds.iter().enumerate() {
        let (cx, cy) = (i % 3, i / 3);
        let [r, g, b] = palette::cell_rgb(*kind);
        p.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(
                    rect.left() + cx as f32 * (cell + 1.0),
                    rect.top() + cy as f32 * (cell + 1.0),
                ),
                egui::vec2(cell, cell),
            ),
            CornerRadius::ZERO,
            Color32::from_rgb(r, g, b),
        );
    }
}

/// 竖直分隔线（顶栏里的）。
fn sep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(egui::vec2(1.0, 12.0), egui::Sense::hover());
    ui.painter().line_segment(
        [r.center_top(), r.center_bottom()],
        Stroke::new(1.0, theme::LINE),
    );
}

// ---------------------------------------------------------------------------
// 左栏：工具
// ---------------------------------------------------------------------------

fn tool_strip(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) {
    Panel::left("vivarium-tools")
        .exact_size(theme::TOOL_STRIP_W)
        .resizable(false)
        .show_separator_line(false)
        .frame(panel_frame(
            theme::PANEL,
            Margin {
                left: 4,
                right: 4,
                top: 6,
                bottom: 6,
            },
        ))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 3.0;
            for tool in Tool::ALL {
                let icon = match tool {
                    Tool::Brush => Icon::Brush,
                    Tool::EraseLife => Icon::EraseLife,
                    Tool::EraseMatter => Icon::Eraser,
                    Tool::Fill => Icon::Fill,
                    Tool::Pick => Icon::Pick,
                    Tool::Inspect => Icon::Inspect,
                    Tool::Lamp => Icon::Lamp,
                };
                let tip = format!("{}（{}）— {}", tool.label(), tool.hotkey(), tool.describe());
                if widgets::icon_button(ui, icon, state.tool == tool, &tip).clicked() {
                    state.tool = tool;
                    state.emit(Command::SetTool(tool));
                }
            }

            ui.add_space(6.0);
            if widgets::icon_button(
                ui,
                Icon::Workshop,
                state.workshop.open,
                "生物工坊（W）——在网格上画一副身体",
            )
            .clicked()
            {
                state.workshop.open = !state.workshop.open;
            }

            ui.add_space(6.0);
            if widgets::icon_button(
                ui,
                Icon::Globe,
                state.world.open,
                "世界（G）——新建 / 保存 / 读取",
            )
            .clicked()
            {
                state.world.open = !state.world.open;
                // 打开时立刻要一份存档列表：那个目录可能刚被用户在外面改过，
                // 而"打开面板看到的是上一次的列表"会让人以为存档丢了。
                if state.world.open {
                    state.emit(Command::RefreshSaves);
                }
            }

            ui.add_space(6.0);
            let (r, _) =
                ui.allocate_exact_size(egui::vec2(ui.available_width(), 1.0), egui::Sense::hover());
            ui.painter().line_segment(
                [r.left_center(), r.right_center()],
                Stroke::new(1.0, theme::LINE),
            );
            ui.add_space(6.0);

            // 暂停 / 继续 + 单步，放在工具条底部——大拇指的位置。
            // 画布类程序的"停一下"是最高频的操作之一。
            let pause_icon = if snap.scale.is_paused() {
                Icon::Step
            } else {
                Icon::Pause
            };
            if widgets::icon_button(
                ui,
                pause_icon,
                snap.scale.is_paused(),
                if snap.scale.is_paused() {
                    "继续（空格）"
                } else {
                    "暂停（空格）"
                },
            )
            .clicked()
            {
                state.emit(Command::TogglePause);
            }
        });
    // 右边缘的分界线（和中央视口之间）。
    let r = ui.max_rect();
    ui.painter().line_segment(
        [
            egui::pos2(r.left() + 0.5, r.top()),
            egui::pos2(r.left() + 0.5, r.bottom()),
        ],
        Stroke::new(1.0, theme::LINE),
    );
}

// ---------------------------------------------------------------------------
// 底部：材质与细胞调色板
// ---------------------------------------------------------------------------

fn palette(ui: &mut Ui, state: &mut UiState, _snap: &Snapshot) {
    Panel::bottom("vivarium-palette")
        .exact_size(theme::PALETTE_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(panel_frame(
            theme::PANEL,
            Margin {
                left: 6,
                right: 6,
                top: 3,
                bottom: 3,
            },
        ))
        .show(ui, |ui| {
            // 悬停在哪一个细胞图例上。下面那行描述优先说它——
            // 图例如果连说明都没有，就只是一排好看的色块。
            let mut peek: Option<CellType> = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 1.0;

                // --- 材质 ---
                caption(ui, "材质");
                for kind in [
                    MaterialKind::Stone,
                    MaterialKind::Sand,
                    MaterialKind::Water,
                    MaterialKind::Fire,
                    MaterialKind::Gas,
                    MaterialKind::Remains,
                    MaterialKind::Corpse,
                ] {
                    let [r, g, b] = palette::rgb(kind);
                    if widgets::swatch(
                        ui,
                        Color32::from_rgb(r, g, b),
                        material_label(kind),
                        Some(&format!(
                            "{}（{} 格）",
                            kind.name(),
                            _snap.census[kind.as_u8() as usize]
                        )),
                        state.material == kind,
                    )
                    .clicked()
                    {
                        state.material = kind;
                        state.emit(Command::SetMaterial(kind));
                    }
                }

                ui.add_space(6.0);
                vsep(ui);
                ui.add_space(6.0);

                // --- 细胞：只读图例 ---
                //
                // 这一排**点不动**，是图例不是按钮。细胞的颜色是一套语言
                // （世界里哪一格是什么颜色就是什么细胞），界面必须把它写出来，
                // 否则"右边那个橙红色的是什么"永远没人知道。
                //
                // 但"往世界里放细胞"不该做成画笔：细胞属于某个有机体，
                // 单独丢一格进去是一个没有身体的怪东西。投放请用「放鱼」。
                //
                // 它得**看起来**不可点（`legend_chip`：无点击、悬停不发亮、
                // 边框更暗），否则玩家一定会去点它，然后发现什么都没发生。
                // 悬停时把它的说明写到下面那行描述里——图例不能只是好看。
                caption(ui, "细胞图例");
                for kind in CellType::ALL {
                    let [r, g, b] = palette::cell_rgb(kind);
                    let resp =
                        widgets::legend_chip(ui, Color32::from_rgb(r, g, b), cell_label(kind));
                    if resp.hovered() {
                        peek = Some(kind);
                    }
                }

                // --- 右侧：笔刷 ---
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    let mut size = state.brush as f32;
                    let resp = widgets::value_slider(ui, "笔刷", &mut size, 1.0..=16.0, "格");
                    if resp.changed() {
                        state.brush = size.round() as u32;
                        state.emit(Command::SetBrush(state.brush));
                    }
                });
            });

            // --- 描述 ---
            //
            // 色块只回答"是什么颜色"，这一行回答"**它会做什么**"——
            // 而后者才是新手要学的部分。放在色块下面单独一行，
            // 因为描述有三四十个字，挤在色块之间会把布局撑破。
            ui.add_space(1.0);
            let desc = ui.horizontal(|ui| {
                let (name, text) = match peek {
                    Some(kind) => (cell_label(kind), kind.describe()),
                    None => (material_label(state.material), state.material.describe()),
                };
                ui.label(egui::RichText::new(name).size(9.5).color(theme::TEXT));
                ui.label(
                    egui::RichText::new(text)
                        .size(9.5)
                        .color(theme::TEXT_DIM),
                );
            });
            hairline_top(ui);

            // 内容比面板高会怎样？**不会报错，会被下面的控制台盖住。**
            // 面板是固定高度的，溢出的那半行正好落在控制台的背景上——
            // 看起来像"控制台压住了描述"，而真正的原因是这里多了一行。
            // 所以让它在测试里立刻炸，而不是等人用眼睛发现。
            //
            // 判据只能取**描述行自己的矩形**：
            // - `ui.min_rect()` 不行——它会被 `max_rect` 夹住（实测内容 67、
            //   面板 66，两者高度都报 66），永远测不出溢出；
            // - `ui.cursor()` 也不行——它含 `item_spacing.y`，正常布局下就已经
            //   越过面板底 1px，判据会永远红。
            // 而 `desc.response.rect` 就是那一行实际占的地方，没有夹、没有间距。
            debug_assert!(
                desc.response.rect.bottom() <= ui.max_rect().bottom() + 0.5,
                "描述行底部 {:.1} 越过面板底部 {:.1}：它会被下面的控制台盖住",
                desc.response.rect.bottom(),
                ui.max_rect().bottom()
            );
            // 调高度时的量尺：`VIVARIUM_LAYOUT=1` 打印"描述行还剩多少余量"。
            // 和 `VIVARIUM_HOVER` 一个思路——布局类问题只看代码猜不准，
            // 但也不该为此改一次代码量一次。
            if std::env::var("VIVARIUM_LAYOUT").is_ok() {
                println!(
                    "调色板 描述底 {:.1} / 面板底 {:.1} / 余量 {:.1}",
                    desc.response.rect.bottom(),
                    ui.max_rect().bottom(),
                    ui.max_rect().bottom() - desc.response.rect.bottom()
                );
            }
        });
}

/// 调色板里的小标题。
///
/// **高度必须和色块一样**（`SWATCH + 16`），而且要自己画、自己垂直居中。
/// 用 `ui.label` 会有个很隐蔽的副作用：`ui.horizontal` 的垂直居中按
/// **当前行高**算，而行高会被同一行里最高的元素撑高——于是分隔线之后的
/// "细胞图例"会沉到行的底部，和色块下面的标签挤在一起。
/// 所以 `caption` 和 `vsep` 都得钉死在 `SWATCH + 16`。
fn caption(ui: &mut Ui, text: &str) {
    let font = egui::FontId::new(theme::TITLE_SIZE, egui::FontFamily::Proportional);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font, theme::TEXT_FAINT);
    let size = egui::vec2(galley.size().x + 8.0, theme::SWATCH + 16.0);
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().galley(
        egui::pos2(rect.left() + 4.0, rect.center().y - galley.size().y * 0.5),
        galley,
        theme::TEXT_FAINT,
    );
}

/// 调色板里的竖直分隔线。
///
/// 高度**必须等于这一行**（`SWATCH + 16`，和 `caption` 同高），不能写
/// `PALETTE_H - 16`。写面板高度会把这一个元素撑到几乎整个面板，
/// `ui.horizontal` 的行高跟着涨到 72，于是**下一行**（描述）被推出面板，
/// 落在下面控制台的背景上——看起来像"控制台压住了描述"。
fn vsep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(
        egui::vec2(1.0, theme::SWATCH + 16.0),
        egui::Sense::hover(),
    );
    ui.painter().line_segment(
        [r.center_top(), r.center_bottom()],
        Stroke::new(1.0, theme::LINE),
    );
}

/// 材质的中文名。
///
/// 单独一张表，而不是往 `MaterialKind` 上加方法：**名字是给人看的，
/// 是界面的事**。核心里那个 `name()` 是给日志和存档用的稳定标识
/// （`"algae"`），两者用途不同，混在一起会让"改个显示名"变成
/// "改了存档格式"。
fn material_label(kind: MaterialKind) -> &'static str {
    match kind {
        MaterialKind::Empty => "空",
        MaterialKind::Stone => "石",
        MaterialKind::Sand => "沙",
        MaterialKind::Water => "水",
        MaterialKind::Fire => "火",
        MaterialKind::Gas => "气",
        MaterialKind::Remains => "碎",
        MaterialKind::Corpse => "尸",
    }
}

fn cell_label(kind: CellType) -> &'static str {
    match kind {
        CellType::Mouth => "嘴",
        CellType::Producer => "产",
        CellType::Mover => "移",
        CellType::Eye => "眼",
        CellType::Killer => "杀",
        CellType::Armor => "装",
        CellType::Builder => "造",
    }
}

// 细胞的说明只有一份：`CellType::describe()`（住在核心里）。
// 这里曾经复制过一份，然后 Builder 在 v0.23 真的起作用了，而副本还写着
// "未实现"——**两份文案必然漂移**，所以副本已经删掉。

// ---------------------------------------------------------------------------
// 右栏：检查器
// ---------------------------------------------------------------------------

fn inspector(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) {
    Panel::right("vivarium-inspector")
        .exact_size(theme::INSPECTOR_W)
        .resizable(false)
        .show_separator_line(false)
        .frame(panel_frame(
            theme::PANEL,
            Margin {
                left: 8,
                right: 8,
                top: 6,
                bottom: 6,
            },
        ))
        .show(ui, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;

                    // **选中排在最上面。** 它是最"交互"的那一块：点了鱼，
                    // 眼睛就在那一处。放到最下面的话，点完得先滚一段才看得到，
                    // 而"点了没反应"是所有界面里最劝退的一种。
                    widgets::section(ui, "选中");
                    match &snap.selected {
                        Some(sel) => selected_body(ui, state, sel),
                        None => {
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new("用「检查」工具点世界里的生物")
                                    .size(10.0)
                                    .color(theme::TEXT_FAINT),
                            );
                            ui.add_space(2.0);
                        }
                    }

                    ui.add_space(6.0);
                    widgets::section(ui, "生态");
                    widgets::stat_row(ui, "生物", &snap.organisms.to_string(), theme::TEXT);
                    widgets::stat_row(ui, "细胞", &snap.cells.to_string(), theme::TEXT);
                    widgets::stat_row(
                        ui,
                        "杀手",
                        &snap.killers.to_string(),
                        // 归零就是捕食者灭绝了——这一条必须显眼，
                        // 因为其他所有指标都会是健康的。
                        if snap.killers == 0 {
                            theme::TEXT_FAINT
                        } else {
                            palette_to_color(palette::cell_rgb(CellType::Killer))
                        },
                    );
                    widgets::stat_row(
                        ui,
                        "体型",
                        &format!("{}–{}", snap.body_range.0, snap.body_range.1),
                        theme::TEXT,
                    );
                    widgets::stat_row(ui, "代数", &snap.generation.to_string(), theme::TEXT);
                    widgets::stat_row(ui, "食物", &format!("{} 格", snap.food), theme::ACCENT);
                    widgets::stat_row(
                        ui,
                        "飘着",
                        &snap.stranded.to_string(),
                        if snap.stranded * 2 > snap.organisms.max(1) {
                            theme::WARN
                        } else {
                            theme::TEXT_DIM
                        },
                    );
                    widgets::stat_row(ui, "能量", &snap.energy.to_string(), theme::TEXT);

                    ui.add_space(3.0);
                    widgets::sparkline(ui, 34.0, &snap.history, theme::ACCENT);
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new("种群（归一化，右端为当前）")
                            .size(9.0)
                            .color(theme::TEXT_FAINT),
                    );

                    ui.add_space(6.0);
                    widgets::section(ui, "性能");
                    let (active, total) = snap.active_chunks;
                    widgets::stat_row(
                        ui,
                        "活跃块",
                        &format!("{active}/{total}"),
                        // 活跃块长期满格 = 有东西在永远抖动，跳块优化等于没写。
                        if total > 0 && active * 4 > total * 3 {
                            theme::WARN
                        } else {
                            theme::TEXT
                        },
                    );

                    ui.add_space(6.0);
                    widgets::section(ui, "材质");
                    for kind in MaterialKind::ALL {
                        if kind == MaterialKind::Empty {
                            continue;
                        }
                        census_row(ui, kind, snap.census[kind.as_u8() as usize]);
                    }
                });
        });
    let r = ui.max_rect();
    ui.painter().line_segment(
        [
            egui::pos2(r.right() - 0.5, r.top()),
            egui::pos2(r.right() - 0.5, r.bottom()),
        ],
        Stroke::new(1.0, theme::LINE),
    );
}

fn palette_to_color([r, g, b]: [u8; 3]) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// 材质分布：一个 8×8 的色块 + 名字 + 格数 + 占比条。
///
/// 占比条是必要的：512² 里水和石头差三个数量级，光看数字读不出"谁占多少"。
fn census_row(ui: &mut Ui, kind: MaterialKind, count: usize) {
    let h = 15.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    let p = ui.painter();

    let chip = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 4.0, rect.center().y),
        egui::vec2(8.0, 8.0),
    );
    p.rect_filled(
        chip,
        CornerRadius::ZERO,
        palette_to_color(palette::rgb(kind)),
    );
    p.rect_stroke(
        chip,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::LINE),
        egui::StrokeKind::Inside,
    );

    p.text(
        egui::pos2(chip.right() + 5.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        material_label(kind),
        egui::FontId::new(theme::TEXT_SIZE, egui::FontFamily::Proportional),
        theme::TEXT_DIM,
    );
    p.text(
        egui::pos2(rect.right(), rect.center().y),
        egui::Align2::RIGHT_CENTER,
        count.to_string(),
        egui::FontId::new(theme::MONO_SIZE, egui::FontFamily::Monospace),
        theme::TEXT,
    );
}

/// 选中个体的详情：形态、收支、感知、决策、权重。
///
/// 五段是照着"看一个个体"的顺序排的：
///
/// 1. **它是什么**（编号、坐标、缩略图）——先认出来是谁；
/// 2. **它过得怎么样**（能量、年龄、收支、走势）——再看状态；
/// 3. **它长什么样**（细胞构成）——形状决定能力；
/// 4. **它此刻在想什么**（17 路输入、6 路输出）——这一帧的决策；
/// 5. **它为什么这么想**（权重矩阵）——把"想"落到数字上。
fn selected_body(ui: &mut Ui, state: &mut UiState, sel: &SelectedOrganism) {
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(format!("#{}", sel.id))
                .monospace()
                .color(theme::TEXT),
        );
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("({}, {})", sel.anchor.0, sel.anchor.1))
                .monospace()
                .size(10.0)
                .color(theme::TEXT_FAINT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::flat_button(ui, "×", BtnState::Normal).clicked() {
                state.selected = None;
                state.emit(Command::Select(None));
            }
        });
    });

    // --- 形态 -------------------------------------------------------------
    //
    // 缩略图放在最上面，因为它回答的是"这是谁"。一行数字回答不了这个问题：
    // 10 格的银鱼和 10 格的掠食者，从数字上看只差"嘴 2 / 杀手 2"。
    widgets::thumbnail(ui, 62.0, &sel.shape);
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(format!(
            "{} 格 · 质量 {} · 第 {} 代{}",
            sel.shape.len(),
            sel.mass,
            sel.generation,
            match sel.parent {
                Some(p) => format!(" · 亲代 #{p}"),
                None => " · 初代".to_owned(),
            }
        ))
        .size(9.5)
        .color(theme::TEXT_FAINT),
    );

    // --- 收支 -------------------------------------------------------------
    ui.add_space(4.0);
    widgets::section(ui, "收支");
    let energy_frac = sel.energy as f32 / sel.max_energy.max(1) as f32;
    widgets::stat_row(
        ui,
        "能量",
        &format!("{} / {}", sel.energy, sel.max_energy),
        if energy_frac < 0.25 {
            theme::WARN
        } else {
            theme::ACCENT
        },
    );
    widgets::stat_row(
        ui,
        "年龄",
        &format!("{}/{}", sel.age.0, sel.age.1),
        theme::TEXT,
    );
    widgets::stat_row(ui, "维持", &format!("{}/tick", sel.upkeep), theme::TEXT_DIM);
    widgets::stat_row(
        ui,
        "移动",
        &format!("{}/格 ×{}", sel.move_cost, sel.speed),
        theme::TEXT_DIM,
    );
    widgets::stat_row(ui, "速度", &format!("{} 格/tick", sel.speed), theme::TEXT);
    widgets::stat_row(ui, "嗅觉", &format!("{} 格", sel.smell), theme::TEXT);

    // 能量走势：和"当前能量"是两件事——一个说现在多少，一个说在涨还是在跌。
    ui.add_space(3.0);
    widgets::sparkline(ui, 26.0, &sel.energy_trace, theme::ACCENT);
    ui.add_space(1.0);
    ui.label(
        egui::RichText::new("能量走势（右端为当前）")
            .size(9.0)
            .color(theme::TEXT_FAINT),
    );

    // --- 细胞构成 ---------------------------------------------------------
    ui.add_space(4.0);
    widgets::section(ui, "细胞构成");
    let total: usize = sel.cells.iter().sum();
    if total > 0 {
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::ZERO, theme::SUNKEN);
        let mut x = rect.left();
        for kind in CellType::ALL {
            let n = sel.cells[kind.as_u8() as usize];
            if n == 0 {
                continue;
            }
            let w = rect.width() * n as f32 / total as f32;
            p.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x, rect.top()),
                    egui::vec2(w.max(1.0), rect.height()),
                ),
                CornerRadius::ZERO,
                palette_to_color(palette::cell_rgb(kind)),
            );
            x += w;
        }
        p.rect_stroke(
            rect,
            CornerRadius::ZERO,
            Stroke::new(1.0, theme::LINE),
            egui::StrokeKind::Inside,
        );
    }
    // 逐类列出格数，只列有的。一行一条，右边是数量。
    for kind in CellType::ALL {
        let n = sel.cells[kind.as_u8() as usize];
        if n == 0 {
            continue;
        }
        let h = 13.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        let p = ui.painter();
        p.text(
            egui::pos2(rect.left() + 4.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            cell_label(kind),
            egui::FontId::new(theme::TEXT_SIZE, egui::FontFamily::Proportional),
            palette_to_color(palette::cell_rgb(kind)),
        );
        p.text(
            egui::pos2(rect.right(), rect.center().y),
            egui::Align2::RIGHT_CENTER,
            n.to_string(),
            egui::FontId::new(theme::MONO_SIZE, egui::FontFamily::Monospace),
            theme::TEXT_DIM,
        );
    }

    // --- 感知 -------------------------------------------------------------
    //
    // 17 路画成一条竖带，不是 17 行横条：横条要吃掉 221px，
    // 而这里真正要看的是"哪几路在动"——那是一个整体形状。
    ui.add_space(4.0);
    widgets::section(ui, "感知（17 路输入）");
    widgets::input_strip(ui, &sel.inputs, &INPUT_NAMES);
    // 把非零的那几路写成一行。竖带看得见"有没有"，这一行说得出"是什么"。
    let mut active: Vec<&str> = Vec::new();
    for (i, name) in INPUT_NAMES.iter().enumerate() {
        if sel.inputs[i].abs() >= 0.05 {
            active.push(name);
        }
    }
    ui.label(
        egui::RichText::new(if active.is_empty() {
            "（一片安静）".to_owned()
        } else {
            active.join(" · ")
        })
        .size(9.5)
        .color(theme::TEXT_FAINT),
    );

    // --- 决策 -------------------------------------------------------------
    ui.add_space(4.0);
    widgets::section(ui, "决策（输出）");
    for (i, name) in OUTPUT_NAMES.iter().enumerate() {
        let v = sel.outputs[i];
        let ink = if i < 4 {
            theme::ACCENT
        } else if i == 4 {
            theme::WARN
        } else {
            theme::INFO
        };
        widgets::bar_row(ui, name, v, true, ink);
    }
    ui.add_space(2.0);
    ui.label(
        egui::RichText::new(format!("本 tick：{}", action_of(&sel.outputs)))
            .size(10.0)
            .color(theme::TEXT),
    );

    // --- 权重 -------------------------------------------------------------
    ui.add_space(4.0);
    widgets::section(ui, "行为网络（直连权重）");
    ui.label(
        egui::RichText::new("行 = 输入，列 = 输出；绿正红负")
            .size(9.0)
            .color(theme::TEXT_FAINT),
    );
    ui.add_space(2.0);
    widgets::weight_matrix(ui, INPUT_NAMES.len(), OUTPUT_NAMES.len(), &sel.direct);
}

/// 17 路输入的中文名，顺序与 `life::IN_*` 一致。
///
/// 单独一张表而不是给每个常量加名字：这些名字是**给人看的**，
/// 而且必须能在 52px 里放下——核心里那些常量名（`IN_FOOD_DIR_X`）
/// 是给代码看的，两者用途不同。
const INPUT_NAMES: [&str; 17] = [
    "前有食物",
    "前被挡",
    "前有同类",
    "前有异类",
    "左被挡",
    "右被挡",
    "上被挡",
    "下方支撑",
    "能量",
    "年龄",
    "朝向 x",
    "朝向 y",
    "光照",
    "食物 x",
    "食物 y",
    "生物 x",
    "生物 y",
];

/// 6 路输出的中文名，顺序与 `life::OUT_*` 一致。
const OUTPUT_NAMES: [&str; 6] = ["左", "右", "上", "下", "吃", "生"];

/// 从输出里读出"这一 tick 它打算做什么"。
///
/// 判据必须和 `life_sim::pick_direction` 一致（阈值 0.15、取最大的那个），
/// 否则检查器会显示"想吃"而它其实在往右游——那种不一致比不显示更糟。
fn action_of(out: &[f32; 6]) -> &'static str {
    const THRESHOLD: f32 = 0.15;
    let mut dir = "不动";
    let mut best = THRESHOLD;
    for (i, name) in ["左", "右", "上", "下"].iter().enumerate() {
        if out[i] > best {
            best = out[i];
            dir = name;
        }
    }
    let eating = out[4] > THRESHOLD;
    let breeding = out[5] > THRESHOLD;
    match (dir, eating, breeding) {
        ("不动", false, false) => "待着",
        ("不动", true, false) => "吃",
        ("不动", false, true) => "生",
        ("不动", true, true) => "边吃边生",
        (dir, false, false) => dir,
        (_, true, false) => "边游边吃",
        (_, false, true) => "边游边生",
        (_, true, true) => "边游边吃边生",
    }
}

// ---------------------------------------------------------------------------
// 底部：控制台
// ---------------------------------------------------------------------------

fn console(ui: &mut Ui, state: &mut UiState, _snap: &Snapshot) {
    Panel::bottom("vivarium-console")
        .exact_size(theme::CONSOLE_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(panel_frame(
            theme::SUNKEN,
            Margin {
                left: 6,
                right: 6,
                top: 3,
                bottom: 3,
            },
        ))
        .show(ui, |ui| {
            ScrollArea::vertical()
                .auto_shrink([false, false])
                // 粘底：新行进来时自动滚到最下面，除非用户往上翻了。
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in state.console.lines() {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            ui.label(
                                egui::RichText::new(format!("{:>6}", line.tick))
                                    .monospace()
                                    .size(10.0)
                                    .color(theme::TEXT_FAINT),
                            );
                            let (color, tag) = match line.level {
                                Level::Info => (theme::INFO, "···"),
                                Level::Warn => (theme::WARN, " ! "),
                                Level::Error => (theme::DANGER, "×"),
                            };
                            ui.label(egui::RichText::new(tag).monospace().size(10.0).color(color));
                            ui.label(
                                egui::RichText::new(&line.text)
                                    .monospace()
                                    .size(10.5)
                                    .color(if line.level == Level::Info {
                                        theme::TEXT_DIM
                                    } else {
                                        color
                                    }),
                            );
                        });
                    }
                });
            hairline_top(ui);
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::HoverOrganism;

    fn cell() -> CellInfo {
        CellInfo {
            x: 214,
            y: 137,
            material: MaterialKind::Water,
            organism: None,
            light: 0.71,
            temp: 20.3,
            depth: 42,
        }
    }

    /// 读数必须真的说出"这里是哪儿、是什么、多亮、多热"。
    ///
    /// 这条守的是**四类信息各自都有来源**：坐标来自拾取、材质来自材质表、
    /// 光照和温度来自空气场。少了任何一条，读数就退化成"这里有个东西"。
    #[test]
    fn the_readout_names_the_cell() {
        let lines = cell_lines(&cell());
        assert_eq!(lines.len(), 5, "五行：坐标 / 材质 / 描述 / 光照 / 温度");
        let text: Vec<&str> = lines.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(text[0], "(214, 137)  ·  深 42 格");
        assert_eq!(text[1], "水");
        assert!(text[3].starts_with("光照 0.71"), "{}", text[3]);
        assert_eq!(text[4], "温度 20°C");
        // 材质那一行用的是它自己的说明，不是"水"这个名字重复一遍。
        assert!(text[2].contains("往下流"), "{}", text[2]);
    }

    /// 指针下面是个生物时，第二、三行换成"个体"而不是"材质"。
    ///
    /// 这一条容易写错成"两个都显示"——但生物占住的那格**材质是水**
    /// （借的），同时说"水"和"#312 叶背鱼"会让人以为那一格既是水又是鱼。
    #[test]
    fn the_readout_prefers_the_organism_over_the_material() {
        let mut info = cell();
        info.organism = Some(HoverOrganism {
            id: 312,
            cells: 12,
            generation: 7,
            energy: (412, 480),
            producers: 6,
            cell: CellType::Mover,
        });
        let lines = cell_lines(&info);
        let text: Vec<&str> = lines.iter().map(|(s, _)| s.as_str()).collect();
        assert!(text[1].starts_with("#312"), "{}", text[1]);
        assert!(text[1].contains("12 格"), "{}", text[1]);
        assert!(text[1].contains("7 代"), "{}", text[1]);
        assert_eq!(text[2], "能量 412/480 · 生产者 6");
        assert!(!text[1].contains('水'), "不能同时说是水又是鱼");
    }

    /// 光照越亮，读数里那一行越亮；温度越高越偏红。
    ///
    /// 颜色是读数的第二语言：扫一眼就知道"这格在暗处"，
    /// 不用去读 `0.03` 那个数字。
    #[test]
    fn the_readout_colours_track_light_and_heat() {
        let dark = light_color(0.0);
        let bright = light_color(1.0);
        assert!(bright.r() > dark.r() && bright.g() > dark.g());

        let cold = temp_color(20.0);
        let hot = temp_color(900.0);
        assert!(hot.r() > cold.r(), "高温必须更红");
        assert!(cold.b() > hot.b(), "低温必须更蓝");
    }
}
