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

use vivarium_core::organelle::{Organelles, MITO};
use vivarium_core::{palette, CellType, MaterialKind, TimeScale, ViewMode};

use crate::state::{
    Command, Hover, Level, MorphView, SelectedOrganism, Snapshot, Tool, UiState,
};
use vivarium_core::probe::Tint;
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
    let Some(hover) = snap.hover.as_ref() else {
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

    let rows = probe_rows(hover);
    if rows.is_empty() {
        return;
    }
    let font = egui::FontId::new(9.5, egui::FontFamily::Proportional);
    let head_font = egui::FontId::new(8.5, egui::FontFamily::Proportional);

    // 先排版，再量尺寸：**宽度由内容决定**，写死一个宽度会让"细胞器清单"
    // 那一行（"叶绿体 ×2 + 气囊 + 根"）被截掉半截。
    let mut galleys: Vec<LaidOutRow> = Vec::new();
    let mut width: f32 = 104.0;
    for row in &rows {
        match row {
            Row::Header(text) => {
                let g = ui.painter().layout_no_wrap(
                    text.clone(),
                    head_font.clone(),
                    theme::TEXT_FAINT,
                );
                width = width.max(g.size().x + 16.0);
                galleys.push((Some(g), None));
            }
            Row::Pair {
                label,
                value,
                tint,
                num,
            } => {
                let lg = ui.painter().layout_no_wrap(
                    label.clone(),
                    font.clone(),
                    theme::TEXT_DIM,
                );
                let vg = ui.painter().layout_no_wrap(
                    value.clone(),
                    font.clone(),
                    tint_color(*tint, *num),
                );
                width = width.max(lg.size().x + vg.size().x + 26.0);
                galleys.push((Some(lg), Some(vg)));
            }
        }
    }
    let line_h = 12.5;
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
    for (i, (lg, vg)) in galleys.into_iter().enumerate() {
        let top = rect.top() + 4.0 + i as f32 * line_h;
        if let Some(lg) = lg {
            p.galley(egui::pos2(rect.left() + 6.0, top), lg, theme::TEXT);
        }
        // 值**右对齐**：左对齐的话，"光照 0.71" 和 "能量 412/480" 的
        // 数字会各起一行，扫一眼对不上。
        if let Some(vg) = vg {
            let w = vg.size().x;
            p.galley(egui::pos2(rect.right() - 6.0 - w, top), vg, theme::TEXT);
        }
    }
}

/// 排好版的一行：`(左边, 右边)`。标题行右边是 `None`。
type LaidOutRow = (Option<std::sync::Arc<egui::Galley>>, Option<std::sync::Arc<egui::Galley>>);

/// 读数的一行。
enum Row {
    /// 组标题（"格" / "场" / "生物"）。
    Header(String),
    /// 标签 + 值。
    Pair {
        label: String,
        value: String,
        tint: Tint,
        /// 数值本身（`0..1` 的归一化量）。`Tint` 里那三条色带要用它取色，
        /// 其余语气忽略它。
        num: Option<f32>,
    },
}

/// 把探针表摊成"一行一条"的列表。
///
/// **这里是界面唯一认识探针的地方**：它不知道"湿度"是什么，
/// 只知道"有一个标签、一个值、一个语气"。以后 `core` 里多一个探针，
/// 这个函数一个字都不用改。
fn probe_rows(hover: &Hover) -> Vec<Row> {
    let mut out = Vec::new();
    for group in hover {
        let mut any = false;
        let mut rows = Vec::new();
        for probe in &group.probes {
            let Some(text) = format_value(&probe.value) else {
                continue; // `Missing`：这一项不适用于这一格
            };
            rows.push(Row::Pair {
                label: probe.label.to_owned(),
                value: text,
                tint: probe.tint,
                num: ramp_input(&probe.value),
            });
            any = true;
        }
        // 整组都 `Missing` 时不画标题——空的"生物"小标题只会让人以为
        // 界面漏了东西。
        if any {
            out.push(Row::Header(group.title.to_owned()));
            out.extend(rows);
        }
    }
    out
}

/// 一个值怎么念。返回 `None` 表示"这一行不画"。
fn format_value(v: &vivarium_core::probe::Value) -> Option<String> {
    use vivarium_core::probe::Value;
    Some(match v {
        Value::Int(n) => n.to_string(),
        Value::Float { v, digits, unit } => format!("{v:.*}{unit}", *digits),
        Value::Ratio { a, b } => format!("{a}/{b}"),
        Value::Text(s) => s.clone(),
        Value::Flag(true) => "是".to_owned(),
        Value::Flag(false) => "否".to_owned(),
        Value::Missing => return None,
    })
}

/// 从探针的值里取出"给色带用的那个数"。
///
/// 三条色带各自归一化：
/// - 光照本来就是 `0..1`；
/// - 温度除以 600（和温度滤镜同一个量纲，这样读数和滤镜的颜色能对上）；
/// - 湿度是 `a/b`，取 `a/b`。
fn ramp_input(v: &vivarium_core::probe::Value) -> Option<f32> {
    use vivarium_core::probe::Value;
    match v {
        Value::Float { v, .. } => Some(*v),
        Value::Ratio { a, b } if *b > 0 => Some(*a as f32 / *b as f32),
        _ => None,
    }
}

/// 语气 → 颜色。
///
/// **颜色是读数的第二语言**：光照越亮那一行越绿、温度越高越红、
/// 湿度越高越蓝。扫一眼就知道"这格在暗处"，不用去读 `0.03` 那个数字。
fn tint_color(tint: Tint, num: Option<f32>) -> Color32 {
    match tint {
        Tint::Faint => theme::TEXT_FAINT,
        Tint::Normal => theme::TEXT,
        Tint::Dim => theme::TEXT_DIM,
        Tint::Accent => theme::ACCENT,
        Tint::Danger => theme::DANGER,
        Tint::Light => light_color(num.unwrap_or(0.5)),
        // 温度那一路的 `num` 是**摄氏度**，而 `temp_color` 期望的就是摄氏度。
        Tint::Temp => temp_color(num.unwrap_or(20.0)),
        Tint::Moisture => moisture_color(num.unwrap_or(0.5)),
    }
}

/// 光照值 → 颜色。和光照滤镜同一个意图，只是简化了。
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

/// 湿度 → 颜色。和湿度滤镜的色带同一个意图（干=土色，湿=蓝）。
fn moisture_color(m: f32) -> Color32 {
    let t = m.clamp(0.0, 1.0);
    if t < 0.35 {
        let k = t / 0.35;
        Color32::from_rgb(
            (154.0 - 107.0 * k) as u8,
            (127.0 - 16.0 * k) as u8,
            (94.0 + 49.0 * k) as u8,
        )
    } else {
        let k = ((t - 0.35) / 0.65).min(1.0);
        Color32::from_rgb(
            (47.0 + 120.0 * k) as u8,
            (111.0 + 136.0 * k) as u8,
            (143.0 + 112.0 * k) as u8,
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
                // 单独丢一格进去是一个没有身体的怪东西。投放请在生物工坊里
                // 画一副，或者在「世界」窗口新建时勾上初始生命。
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

/// 细胞器的中文名。**下标必须和 `palette::ORGANELLE_RGB` 一致。**
///
/// 和 `cell_label` 分开而不是合并：一个是"预设细胞的名字"（给工坊用），
/// 一个是"细胞器清单的名字"（给检视用）。两者的集合不一样——
/// 气囊和线粒体没有对应的预设细胞，却有颜色。
fn organelle_name(idx: usize) -> &'static str {
    match idx {
        0 => "叶绿体",
        1 => "鞭毛",
        2 => "刺",
        3 => "嘴",
        4 => "装甲",
        5 => "气囊",
        6 => "眼",
        7 => "线粒体",
        8 => "建造",
        9 => "根",
        _ => "?",
    }
}

/// 一颗"细胞器胶囊"：色点 + 名字（+ 等级）。
fn organelle_chip(ui: &mut Ui, idx: usize, level: u32) {
    let [r, g, b] = palette::ORGANELLE_RGB[idx];
    let text = if level > 1 {
        format!("{} {level} 级", organelle_name(idx))
    } else {
        organelle_name(idx).to_string()
    };
    // 用 `legend_chip` 而不是按钮：这里点不动，画成能点的样子会骗人。
    widgets::legend_chip(ui, Color32::from_rgb(r, g, b), &text);
}

/// 一组细胞器的短文本，给"段 / 梯度带"那种地方用。
fn organelle_set_text(bits: u16) -> String {
    let o = Organelles(bits);
    let mut parts: Vec<String> = Vec::new();
    for (idx, mask, shift) in vivarium_core::palette::ORGANELLE_SLOTS {
        let level = match shift {
            Some(sh) => u32::from(o.level(sh)),
            None => u32::from(o.has(mask)),
        };
        if level == 0 {
            continue;
        }
        parts.push(if level > 1 {
            format!("{}×{level}", organelle_name(idx))
        } else {
            organelle_name(idx).to_string()
        });
    }
    if parts.is_empty() {
        "空".to_string()
    } else {
        parts.join("+")
    }
}

/// 形态发生：段树、逐层展开、梯度。
fn morph_section(ui: &mut Ui, m: &MorphView) {
    if m.segments.is_empty() {
        ui.label(
            egui::RichText::new("没有基因组数据")
                .size(9.0)
                .color(theme::TEXT_FAINT),
        );
        return;
    }

    // 第一行是总账：几段、几层、顶没顶到上限。
    let expanded: usize = m.layers.iter().sum();
    let cap = if m.capped {
        format!(" · **顶到上限 {}**", m.max_cells)
    } else {
        format!(" / 上限 {}", m.max_cells)
    };
    ui.label(
        egui::RichText::new(format!(
            "{} 段 · 展开 {expanded} 格{cap}",
            m.segments.len()
        ))
        .size(9.5)
        .color(if m.capped { theme::WARN } else { theme::TEXT_DIM }),
    );

    // 段树：按深度缩进。`depth` 来自真展开的递归深度，不是另算的。
    for seg in &m.segments {
        let h = 12.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        let p = ui.painter();
        let x = rect.left() + 2.0 + seg.depth as f32 * 10.0;
        let mut tail = format!("{} 格", seg.cells);
        if seg.repeat > 1 {
            tail.push_str(&format!(" ×{}", seg.repeat));
        }
        if seg.mirror {
            tail.push_str(" 镜像");
        }
        p.text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            if seg.depth == 0 { "主干" } else { "└ 段" },
            egui::FontId::new(theme::TEXT_SIZE, egui::FontFamily::Proportional),
            if seg.depth == 0 {
                theme::TEXT
            } else {
                theme::TEXT_DIM
            },
        );
        p.text(
            egui::pos2(x + 30.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            format!("{tail}  {}", organelle_set_text(seg.base)),
            egui::FontId::new(9.0, egui::FontFamily::Monospace),
            theme::TEXT_FAINT,
        );
    }

    // 逐层展开：这是"身体是靠重复长出来的，还是靠子段长出来的"的直接证据。
    if !m.layers.is_empty() {
        let chain: Vec<String> = m.layers.iter().map(|n| n.to_string()).collect();
        ui.label(
            egui::RichText::new(format!("逐层：{} 格", chain.join(" → ")))
                .size(9.0)
                .color(theme::TEXT_FAINT),
        );
    }

    // 梯度。
    if m.bands.is_empty() {
        ui.label(
            egui::RichText::new("无梯度：整具身体是同一种细胞")
                .size(9.0)
                .color(theme::TEXT_FAINT),
        );
    } else {
        for (i, b) in m.bands.iter().enumerate() {
            let mut txt = format!("带{} 沿 {:.2}–{:.2}", i + 1, b.along.0, b.along.1);
            if b.from_edge != (0.0, 0.0) {
                txt.push_str(&format!(" 贴边 {:.0}–{:.0}", b.from_edge.0, b.from_edge.1));
            }
            if b.set != 0 {
                txt.push_str(&format!(" +{}", organelle_set_text(b.set)));
            }
            if b.clear != 0 {
                txt.push_str(&format!(" −{}", organelle_set_text(b.clear)));
            }
            ui.label(
                egui::RichText::new(txt)
                    .size(9.0)
                    .color(theme::TEXT_DIM)
                    .monospace(),
            );
        }
    }

    // 最后一句必须写死：它是这个面板最容易被误读的地方。
    ui.label(
        egui::RichText::new("身体在出生那一刻就长完了——之后只死不长，没有发育")
            .size(9.0)
            .color(theme::TEXT_FAINT),
    );
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
        CellType::Root => "根",
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
                    // 水汽：蒸发让它涨、降水让它落。**它是唯一一个能看见
                    // 水循环在跑的数**——总水量几乎不变（水只是从海里搬到
                    // 天上再搬回去），所以看总水量看不出循环。
                    widgets::stat_row(
                        ui,
                        "水汽",
                        &format!("{} 格", snap.vapor),
                        if snap.vapor == 0 {
                            theme::TEXT_FAINT
                        } else {
                            theme::TEXT_DIM
                        },
                    );
                    // "飘着"改成"离水"：v0.31 之后这个数不再只是"飘在天上"，
                    // 它也包含搁在干地上的个体——两者在数值上是同一件事
                    // （一格水都够不到），而"离水"把两件事都说到了。
                    widgets::stat_row(
                        ui,
                        "离水",
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
/// 4. **它此刻在想什么**（`INPUTS` 路输入、`OUTPUTS` 路输出）——这一帧的决策；
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
    //
    // v0.41 起它还能**点**：细胞是细胞器组合之后，"这一格到底是
    // 叶绿体 + 气囊还是纯生产者"从外面看不出来，只能点开。
    let picked = state
        .picked_cell
        .filter(|(id, _)| *id == sel.id)
        .map(|(_, i)| i)
        .filter(|i| *i < sel.shape.len());
    let (_, clicked) = widgets::thumbnail(ui, 62.0, &sel.shape, picked);
    if let Some(i) = clicked {
        // 再点一次同一格 = 取消选中。手势比"清除"按钮便宜。
        state.picked_cell = if picked == Some(i) {
            None
        } else {
            Some((sel.id, i))
        };
    }
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

    // --- 选中的那一格 -----------------------------------------------------
    //
    // 这一块是"缩略图能点"的全部意义：一格里有哪几个细胞器、各几级。
    // 没有它，玩家能点但看不到任何东西，等于没做。
    match picked.and_then(|i| sel.shape.get(i).map(|c| (i, *c))) {
        Some((i, (dx, dy, bits))) => {
            let o = Organelles(bits);
            ui.add_space(4.0);
            widgets::section(ui, &format!("第 {i} 格 · 偏移 ({dx:+}, {dy:+})"));
            ui.add_space(1.0);
            // 一行细胞器清单。**按混色顺序排**（叶绿体 → 鞭毛 → 刺 → 开关），
            // 和右边那颗色块的颜色构成一致，读起来才对得上。
            let mut row_open = false;
            let mut any = false;
            for (idx, mask, shift) in vivarium_core::palette::ORGANELLE_SLOTS {
                let level = match shift {
                    Some(sh) => u32::from(o.level(sh)),
                    None => u32::from(o.has(mask)),
                };
                if level == 0 {
                    continue;
                }
                if !row_open {
                    ui.horizontal_wrapped(|ui| {
                        organelle_chip(ui, idx, level);
                        row_open = true;
                    });
                } else {
                    // `horizontal_wrapped` 不能跨调用续写，所以这里重开一行。
                    ui.horizontal_wrapped(|ui| {
                        organelle_chip(ui, idx, level);
                    });
                }
                any = true;
            }
            if o.has(MITO) {
                ui.horizontal_wrapped(|ui| {
                    organelle_chip(ui, 7, 1);
                });
            }
            if !any && !o.has(MITO) {
                ui.label(
                    egui::RichText::new("这一格什么细胞器都没有——它只是一团细胞质")
                        .size(9.0)
                        .color(theme::TEXT_FAINT),
                );
            }
        }
        None => {
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("点缩略图里的格子，看它带哪些细胞器")
                    .size(9.0)
                    .color(theme::TEXT_FAINT),
            );
        }
    }

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

    // --- 细胞器构成 -------------------------------------------------------
    //
    // **按细胞器统计，不按"细胞类型"。** 一旦一格可以是"叶绿体 + 气囊"，
    // 按类型分桶就不再可加——那 13 格里到底几格是生产者？取决于你数哪一列。
    // 按细胞器数则永远可加：叶绿体 9、气囊 2、线粒体 13，加起来就是"身体里
    // 有多少份功能"。
    ui.add_space(4.0);
    widgets::section(ui, "细胞器构成");
    let total: usize = sel.organelles.iter().sum();
    if total > 0 {
        // 一条按比例分段的带子：每段一个细胞器，颜色就是它在世界里的颜色。
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), 10.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, CornerRadius::ZERO, theme::SUNKEN);
        let mut x = rect.left();
        for (idx, n) in sel.organelles.iter().enumerate() {
            if *n == 0 {
                continue;
            }
            let w = rect.width() * *n as f32 / total as f32;
            p.rect_filled(
                egui::Rect::from_min_size(
                    egui::pos2(x, rect.top()),
                    egui::vec2(w.max(1.0), rect.height()),
                ),
                CornerRadius::ZERO,
                palette_to_color(palette::ORGANELLE_RGB[idx]),
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
    // 逐项列出格数，只列有的。一行一条，右边是数量。
    for (idx, n) in sel.organelles.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        let h = 13.0;
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
        let p = ui.painter();
        p.text(
            egui::pos2(rect.left() + 4.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            organelle_name(idx),
            egui::FontId::new(theme::TEXT_SIZE, egui::FontFamily::Proportional),
            palette_to_color(palette::ORGANELLE_RGB[idx]),
        );
        p.text(
            egui::pos2(rect.right(), rect.center().y),
            egui::Align2::RIGHT_CENTER,
            n.to_string(),
            egui::FontId::new(theme::MONO_SIZE, egui::FontFamily::Monospace),
            theme::TEXT_DIM,
        );
    }

    // --- 形态发生 ---------------------------------------------------------
    //
    // **这不是"发育"。** 身体在出生那一刻就长完了，之后只死不长；
    // 这里画的是"基因组 → 身体"那一次计算的过程。
    ui.add_space(4.0);
    widgets::section(ui, "形态发生");
    morph_section(ui, &sel.morph);

    // --- 感知 -------------------------------------------------------------
    //
    // 17 路画成一条竖带，不是 17 行横条：横条要吃掉 221px，
    // 而这里真正要看的是"哪几路在动"——那是一个整体形状。
    ui.add_space(4.0);
    widgets::section(ui, "感知（输入）");
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

/// 输入的中文名，顺序与 `life::IN_*` 一致。
///
/// 单独一张表而不是给每个常量加名字：这些名字是**给人看的**，
/// 而且必须能在 52px 里放下——核心里那些常量名（`IN_FOOD_DIR_X`）
/// 是给代码看的，两者用途不同。
const INPUT_NAMES: [&str; vivarium_core::life::INPUTS] = [
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
    // M2a 新增：递归状态。这五个和上面 17 个不是一类东西——
    // 上面是"我看到了什么"，下面是"我刚才在干什么"。
    "上次左",
    "上次右",
    "上次上",
    "上次下",
    "卡住",
    // v0.39：尸体独立成一路。上面 17 路是"我看到了什么"，
    // 中间 5 路是"我刚才在干什么"，这两路是"**哪儿有得捡**"。
    "尸体 x",
    "尸体 y",
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
    use vivarium_core::probe::{Group, Probe, Value};

    /// 一份和 `core::probe::cell_probes` 同构的读数。
    ///
    /// **刻意手写而不是调 `cell_probes`**：这一层测的是"界面能不能把一张
    /// 探针表排版出来"，不是"core 给的探针对不对"。掺进一个真世界之后，
    /// 断言就会跟着生态漂——而那和排版没有关系。
    fn hover() -> Hover {
        vec![
            Group {
                title: "格",
                probes: vec![
                    Probe {
                        label: "坐标",
                        value: Value::Text("214, 137".to_owned()),
                        tint: Tint::Normal,
                    },
                    Probe {
                        label: "深度",
                        value: Value::Text("+42 格".to_owned()),
                        tint: Tint::Dim,
                    },
                    Probe {
                        label: "材质",
                        value: Value::Text("水".to_owned()),
                        tint: Tint::Normal,
                    },
                ],
            },
            Group {
                title: "场",
                probes: vec![
                    Probe {
                        label: "光照",
                        value: Value::Float {
                            v: 0.71,
                            digits: 2,
                            unit: "",
                        },
                        tint: Tint::Light,
                    },
                    Probe {
                        label: "温度",
                        value: Value::Float {
                            v: 20.0,
                            digits: 0,
                            unit: " °C",
                        },
                        tint: Tint::Temp,
                    },
                    Probe {
                        label: "湿度",
                        value: Value::Ratio { a: 200, b: 255 },
                        tint: Tint::Moisture,
                    },
                ],
            },
            Group {
                title: "生物",
                probes: vec![
                    Probe {
                        label: "个体",
                        value: Value::Text("#312 · 12 格 · 代 7".to_owned()),
                        tint: Tint::Accent,
                    },
                    Probe {
                        label: "能量",
                        value: Value::Ratio { a: 412, b: 480 },
                        tint: Tint::Normal,
                    },
                ],
            },
        ]
    }

    /// 读数必须真的说出"这里是哪儿、是什么、多亮、多热、多湿"。
    ///
    /// 这条守的是**每一类信息都有自己的来源**：坐标来自拾取、材质来自
    /// 材质表、光照/温度/湿度来自各自的场。少了任何一条，读数就退化成
    /// "这里有个东西"。
    #[test]
    fn the_readout_names_the_cell() {
        let rows = probe_rows(&hover());
        assert_eq!(rows.len(), 11, "三条标题 + 八条读数");
        let headers: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Header(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["格", "场", "生物"]);

        let pairs: Vec<(&str, &str)> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Pair { label, value, .. } => Some((label.as_str(), value.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(pairs[0], ("坐标", "214, 137"));
        assert_eq!(pairs[2], ("材质", "水"));
        assert_eq!(pairs[3], ("光照", "0.71"), "光照两位小数");
        assert_eq!(pairs[4], ("温度", "20 °C"), "温度带单位、零位小数");
        assert_eq!(pairs[5], ("湿度", "200/255"));
        assert_eq!(pairs[7], ("能量", "412/480"));
    }

    /// **空组不画标题。**
    ///
    /// 一整组都是 `Missing` 时（比如"这一格不是生物"），留下一个空标题
    /// 会让人以为界面漏了东西——而那正是"读数不可信"的起点。
    #[test]
    fn a_group_with_nothing_to_say_draws_no_header() {
        let mut h = hover();
        h.push(Group {
            title: "空组",
            probes: vec![Probe {
                label: "年龄",
                value: Value::Missing,
                tint: Tint::Dim,
            }],
        });
        let rows = probe_rows(&h);
        let headers: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Header(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(headers, ["格", "场", "生物"], "空组不该多出一个标题");
    }

    /// 光照越亮那一行越亮；温度越高越红；湿度越高越蓝。
    ///
    /// 颜色是读数的第二语言：扫一眼就知道"这格在暗处"，
    /// 不用去读 `0.03` 那个数字。
    #[test]
    fn the_readout_colours_track_the_field() {
        let dark = tint_color(Tint::Light, Some(0.0));
        let bright = tint_color(Tint::Light, Some(1.0));
        assert!(bright.r() > dark.r() && bright.g() > dark.g());

        let cold = tint_color(Tint::Temp, Some(20.0));
        let hot = tint_color(Tint::Temp, Some(900.0));
        assert!(hot.r() > cold.r(), "高温必须更红");
        assert!(cold.b() > hot.b(), "低温必须更蓝");

        let dry = tint_color(Tint::Moisture, Some(0.0));
        let wet = tint_color(Tint::Moisture, Some(1.0));
        // 判据是**冷暖**（r−b），不是"谁的红分量更高"：饱和那一端是近白，
        // 红分量也很高——用 `r` 比会得出"湿比干更暖"这种反直觉的结论。
        let warmth = |c: Color32| i32::from(c.r()) - i32::from(c.b());
        assert!(warmth(dry) > warmth(wet), "干土该偏暖：{dry:?} vs {wet:?}");
        assert!(wet.b() > dry.b(), "湿土该更蓝：{dry:?} vs {wet:?}");
    }

    /// 三条色带的输入是从 `Value` 里取的，不是从格式化后的字符串里解析的。
    ///
    /// 这一条守的是"先格式化再解析"那条弯路——它一旦出现，
    /// 改一下单位（`"20 °C"` 改成 `"20°C"`）就会让颜色悄悄失效。
    #[test]
    fn ramp_input_comes_from_the_value_not_the_text() {
        assert_eq!(
            ramp_input(&Value::Float {
                v: 0.71,
                digits: 2,
                unit: "",
            }),
            Some(0.71)
        );
        assert_eq!(ramp_input(&Value::Ratio { a: 51, b: 255 }), Some(0.2));
        assert_eq!(ramp_input(&Value::Text("200/255".to_owned())), None);
        assert_eq!(
            ramp_input(&Value::Ratio { a: 1, b: 0 }),
            None,
            "除以零不能崩"
        );
    }
}
