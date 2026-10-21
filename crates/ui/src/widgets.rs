//! 自绘控件。
//!
//! ## 为什么不用 egui 自带控件
//!
//! egui 的 `Button` / `Slider` 是"现代桌面"风格：圆角、内阴影、悬停有渐变。
//! 这个项目要的是 TPT 那种**机械**的样子——1px 实线边框、按下整体反色、
//! 没有一个像素是用来装饰的。用 egui 默认控件再改 `Visuals` 也达不到：
//! 圆角半径、内边距、文字基线都散在 `Style` 的十几个字段里，
//! 改到最后是"在别人的设计里打补丁"。
//!
//! 自绘的代价是要自己处理交互（悬停、按下、拖拽），但收益是**每一个像素
//! 都在自己手里**，而且判据可以写成一句话：
//!
//! > 默认 = 1px 边框 + 透明底；悬停 = 边框变亮；按下 = 整体反色。
//!
//! ## 为什么全部返回 `Response`
//!
//! 这样调用方可以用 `if btn.clicked() { ... }`，和 egui 原生控件的手感一致。
//! 自绘的是**外观**，不是**交互模型**——交互模型照搬 egui 是对的，
//! 它已经处理好了触摸、键盘焦点、无障碍这些没人想重写的东西。

use egui::{
    Align2, Color32, CornerRadius, FontId, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use crate::theme;

/// 按钮的三种视觉状态。分开成一个枚举，是因为"按下"和"选中"长得不一样：
///
/// - **按下**是瞬时反馈，整体反色（白底黑字），和 TPT 一样；
/// - **选中**是持续状态，只有边框变成强调色，底色是强调色的暗部。
///
/// 混成一种的话，"这个按钮现在是不是开着"和"我正按着它"就分不清了。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtnState {
    /// 普通可点。
    Normal,
    /// 持续选中（开关打开、工具被选中）。
    Selected,
    /// 禁用。
    Disabled,
}

/// 扁平按钮。**这是整套界面的基础件**，其他控件都是它的变体。
///
/// 判据：
/// - 默认：1px `LINE_STRONG` 边框，`RAISED` 底，`TEXT` 字；
/// - 悬停：边框 `TEXT`，底 `HOVER`，字纯白；
/// - 按下：底 `TEXT`、字 `PANEL`（反色）；
/// - 选中：边框 `ACCENT`、底 `ACCENT_DIM`、字 `ACCENT`；
/// - 禁用：边框 `LINE`、字 `TEXT_FAINT`，不响应。
pub fn flat_button(ui: &mut Ui, label: &str, state: BtnState) -> Response {
    let font = FontId::new(theme::TEXT_SIZE, egui::FontFamily::Proportional);
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        font,
        match state {
            BtnState::Selected => theme::ACCENT,
            BtnState::Disabled => theme::TEXT_FAINT,
            BtnState::Normal => theme::TEXT,
        },
    );

    let pad = Vec2::new(7.0, 4.0);
    let size = Vec2::new(galley.size().x + pad.x * 2.0, 18.0);
    let sense = if state == BtnState::Disabled {
        Sense::hover()
    } else {
        Sense::click()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);

    // 反色的时机是 `is_pointer_button_down_on`，不是 `clicked`——
    // 按下就要变色，等松手才变会让人怀疑自己点没点上。
    let pressed = resp.is_pointer_button_down_on() && state != BtnState::Disabled;
    let hovered = resp.hovered() && state != BtnState::Disabled;

    let (fill, border, ink) = if pressed {
        (theme::TEXT, Color32::WHITE, theme::PANEL)
    } else if state == BtnState::Selected {
        (theme::ACCENT_DIM, theme::ACCENT, theme::ACCENT)
    } else if state == BtnState::Disabled {
        (theme::PANEL, theme::LINE, theme::TEXT_FAINT)
    } else if hovered {
        (theme::HOVER, theme::TEXT, Color32::WHITE)
    } else {
        (theme::RAISED, theme::LINE_STRONG, theme::TEXT)
    };

    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, fill);
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
    p.galley(rect.center() - galley.size() / 2.0, galley, ink);

    resp
}

/// 方形图标按钮（左侧工具条用）。
///
/// 图标是**画出来的几何形状**，不是字体图标：项目没有图标字体资源，
/// 引一个图标 crate 又会带进几十个用不上的字形。而工具图标本来就简单
/// （笔、橡皮、矩形、填充、取样、放大镜），`Painter` 几笔就够。
pub fn icon_button(ui: &mut Ui, icon: Icon, selected: bool, tip: &str) -> Response {
    let size = Vec2::splat(theme::TOOL_BTN);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());

    let hovered = resp.hovered();
    let pressed = resp.is_pointer_button_down_on();

    let (fill, border, ink) = if pressed {
        (theme::TEXT, Color32::WHITE, theme::PANEL)
    } else if selected {
        (theme::ACCENT_DIM, theme::ACCENT, theme::ACCENT)
    } else if hovered {
        (theme::HOVER, theme::TEXT, Color32::WHITE)
    } else {
        (theme::RAISED, theme::LINE_STRONG, theme::TEXT_DIM)
    };

    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, fill);
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, border),
        StrokeKind::Inside,
    );
    draw_icon(p, icon, rect.shrink(7.0), ink, Stroke::new(1.4, ink));

    resp.on_hover_text(tip)
}

/// 工具图标。每种都只由几根线组成——放大到 40px 也认得出。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// 笔刷：一支斜着的笔。
    Brush,
    /// 橡皮：一个方块加斜杠。
    Eraser,
    /// 矩形：空心方框。
    Rect,
    /// 直线：对角线。
    Line,
    /// 填充：一个斗加一滴。
    Fill,
    /// 取样：吸管。
    Pick,
    /// 检查器：放大镜。
    Inspect,
    /// 暂停 / 继续。
    Pause,
    /// 单步。
    Step,
}

fn draw_icon(p: &egui::Painter, icon: Icon, r: Rect, ink: Color32, s: Stroke) {
    let (l, t, rt, b) = (r.left(), r.top(), r.right(), r.bottom());
    let (cx, cy) = (r.center().x, r.center().y);
    match icon {
        Icon::Brush => {
            // 笔杆 + 笔尖三角。
            p.line_segment([egui::pos2(l + 2.0, b), egui::pos2(rt - 1.0, t + 1.0)], s);
            p.line_segment([egui::pos2(l, b - 3.0), egui::pos2(l + 4.0, b)], s);
            p.line_segment([egui::pos2(l, b - 3.0), egui::pos2(l + 4.0, b)], s);
            p.line_segment([egui::pos2(l, b), egui::pos2(l + 1.0, b - 1.0)], s);
        }
        Icon::Eraser => {
            p.rect_stroke(
                Rect::from_min_max(egui::pos2(l, cy - 3.0), egui::pos2(rt, b)),
                CornerRadius::ZERO,
                s,
                StrokeKind::Inside,
            );
            p.line_segment([egui::pos2(l + 2.0, b), egui::pos2(rt - 2.0, cy - 3.0)], s);
        }
        Icon::Rect => {
            p.rect_stroke(
                Rect::from_min_max(egui::pos2(l, t), egui::pos2(rt, b)),
                CornerRadius::ZERO,
                s,
                StrokeKind::Inside,
            );
        }
        Icon::Line => {
            p.line_segment([egui::pos2(l, b), egui::pos2(rt, t)], s);
        }
        Icon::Fill => {
            // 斗身是三条边（上边开口），右边挂一滴。
            p.line_segment([egui::pos2(l, t + 2.0), egui::pos2(cx, b)], s);
            p.line_segment([egui::pos2(cx, b), egui::pos2(rt - 1.0, t + 2.0)], s);
            p.line_segment([egui::pos2(l, t + 2.0), egui::pos2(rt - 1.0, t + 2.0)], s);
            p.circle_filled(egui::pos2(rt, b - 1.0), 1.6, ink);
        }
        Icon::Pick => {
            // 吸管：一根斜管 + 顶上的球。
            p.line_segment([egui::pos2(l, b), egui::pos2(rt - 2.0, t + 2.0)], s);
            p.circle_stroke(egui::pos2(rt - 1.5, t + 1.5), 2.2, s);
        }
        Icon::Inspect => {
            p.circle_stroke(egui::pos2(cx - 1.0, cy - 1.0), 4.2, s);
            p.line_segment([egui::pos2(cx + 2.0, cy + 2.0), egui::pos2(rt, b)], s);
        }
        Icon::Pause => {
            p.line_segment(
                [egui::pos2(cx - 2.0, t), egui::pos2(cx - 2.0, b)],
                Stroke::new(2.0, ink),
            );
            p.line_segment(
                [egui::pos2(cx + 2.0, t), egui::pos2(cx + 2.0, b)],
                Stroke::new(2.0, ink),
            );
        }
        Icon::Step => {
            // 播放三角 + 一道竖线。
            p.add(egui::Shape::convex_polygon(
                vec![egui::pos2(l, t), egui::pos2(cx + 2.0, cy), egui::pos2(l, b)],
                ink,
                Stroke::NONE,
            ));
            p.line_segment(
                [egui::pos2(rt, t), egui::pos2(rt, b)],
                Stroke::new(2.0, ink),
            );
        }
    }
}

/// 调色板色块（底部材质 / 细胞条用）。
///
/// 结构照 TPT 的元素按钮：一个**纯色方块**代表材质本身，下面一行小字。
/// 色块的颜色**直接用渲染层的调色板**，不另配一套——界面上看到的绿
/// 必须就是世界里那个绿，否则"点这个会画出什么颜色"要重新学一遍。
///
/// 选中态用**黄色边框**（TPT 的 `BorderFavorite`）：黄色在世界里不出现，
/// 所以它不会和任何一种材质混淆。
pub fn swatch(
    ui: &mut Ui,
    color: Color32,
    label: &str,
    hint: Option<&str>,
    selected: bool,
) -> Response {
    let size = Vec2::new(theme::SWATCH + 10.0, theme::SWATCH + 16.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());

    let chip = Rect::from_min_size(
        egui::pos2(rect.left() + 5.0, rect.top()),
        Vec2::splat(theme::SWATCH),
    );

    let p = ui.painter();
    // 底色：色块本身。空材质（透明）画成棋盘格，否则"这里什么都没有"
    // 和"这里是黑色"分不清。
    if color == theme::VOID {
        p.rect_filled(chip, CornerRadius::ZERO, theme::SUNKEN);
        for i in 0..3 {
            for j in 0..3 {
                if (i + j) % 2 == 0 {
                    let cell = Rect::from_min_size(
                        egui::pos2(chip.left() + i as f32 * 7.5, chip.top() + j as f32 * 7.5),
                        Vec2::splat(7.5),
                    );
                    p.rect_filled(cell, CornerRadius::ZERO, theme::RAISED);
                }
            }
        }
    } else {
        p.rect_filled(chip, CornerRadius::ZERO, color);
    }

    let border = if selected {
        theme::WARN
    } else if resp.hovered() {
        Color32::WHITE
    } else {
        theme::LINE_STRONG
    };
    p.rect_stroke(
        chip,
        CornerRadius::ZERO,
        Stroke::new(if selected { 2.0 } else { 1.0 }, border),
        StrokeKind::Outside,
    );

    // 标签：色块下面一行，居中。用比例字体而不是等宽——中文标签用等宽
    // 会显得松松垮垮。
    let ink = if selected {
        theme::WARN
    } else if resp.hovered() {
        Color32::WHITE
    } else {
        theme::TEXT_DIM
    };
    p.text(
        egui::pos2(rect.center().x, chip.bottom() + 1.0),
        Align2::CENTER_TOP,
        label,
        FontId::new(theme::TITLE_SIZE, egui::FontFamily::Proportional),
        ink,
    );

    match hint {
        Some(h) => resp.on_hover_text(h),
        None => resp,
    }
}

/// 自绘滑杆：`标签 ····· 轨道 ····· 数值`。
///
/// 比 egui 的 `Slider` 多了两件这里必须要的东西：
///
/// 1. **数值右对齐、等宽**。一列滑杆的数值必须竖直对齐，否则拖动时
///    整行字在抖。
/// 2. **轨道是 1px 线 + 一个方块把手**，不是圆头胶囊。这是 TPT 的语言。
pub fn value_slider(
    ui: &mut Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    unit: &str,
) -> Response {
    let h = 18.0;
    let (rect, mut resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::click_and_drag());

    let label_w = 62.0;
    let value_w = 42.0;
    let track = Rect::from_min_max(
        egui::pos2(rect.left() + label_w, rect.center().y - 1.0),
        egui::pos2(rect.right() - value_w, rect.center().y + 1.0),
    );

    // 拖动：把指针的 x 映射回数值。用 `interact_pointer_pos` 而不是
    // `hover_pos`——拖出控件范围时指针还在，但 hover 已经变成 false，
    // 那时松手会让数值卡在半路。
    if resp.dragged() || resp.clicked() {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            let t = ((pos.x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0);
            let (lo, hi) = (*range.start(), *range.end());
            *value = lo + (hi - lo) * t;
            resp.mark_changed();
        }
    }

    let t = ((*value - range.start()) / (range.end() - range.start()).max(f32::EPSILON))
        .clamp(0.0, 1.0);

    let p = ui.painter();
    p.text(
        egui::pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::new(theme::TEXT_SIZE, egui::FontFamily::Proportional),
        theme::TEXT_DIM,
    );

    // 轨道底 + 已填充部分。
    p.rect_filled(track, CornerRadius::ZERO, theme::SUNKEN);
    p.rect_stroke(
        track,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::LINE),
        StrokeKind::Inside,
    );
    let fill = Rect::from_min_max(
        track.min,
        egui::pos2(track.left() + track.width() * t, track.bottom()),
    );
    p.rect_filled(fill, CornerRadius::ZERO, theme::ACCENT_DIM);

    // 把手：一个 6×12 的方块，比圆点好瞄。
    let hx = track.left() + track.width() * t;
    let handle = Rect::from_center_size(egui::pos2(hx, track.center().y), Vec2::new(6.0, 12.0));
    let hf = if resp.hovered() || resp.dragged() {
        theme::TEXT
    } else {
        theme::ACCENT
    };
    p.rect_filled(handle, CornerRadius::ZERO, hf);

    p.text(
        egui::pos2(rect.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{value:.0}{unit}"),
        FontId::new(theme::MONO_SIZE, egui::FontFamily::Monospace),
        theme::TEXT,
    );

    resp
}

/// 一行 "标签 ······ 数值"。
///
/// 标签左、数值右，中间不画引导线：TPT 不画，而在这个信息密度下
/// 引导线会变成一堵墙。靠"左对齐的标签列 + 右对齐的数值列"就够了。
pub fn stat_row(ui: &mut Ui, label: &str, value: &str, ink: Color32) {
    let h = 15.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let p = ui.painter();
    p.text(
        egui::pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::new(theme::TEXT_SIZE, egui::FontFamily::Proportional),
        theme::TEXT_DIM,
    );
    p.text(
        egui::pos2(rect.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        value,
        FontId::new(theme::MONO_SIZE, egui::FontFamily::Monospace),
        ink,
    );
}

/// 分组标题：一行小字 + 一条贯穿的 1px 线。
///
/// 线是必要的：面板里全是"标签 + 数值"的行，没有分隔就分不清
/// 哪几行属于哪一组。
pub fn section(ui: &mut Ui, title: &str) {
    let h = 18.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let galley = ui.painter().layout_no_wrap(
        title.to_owned(),
        FontId::new(theme::TITLE_SIZE, egui::FontFamily::Proportional),
        theme::TEXT_FAINT,
    );
    let p = ui.painter();
    let text_w = galley.size().x;
    p.galley(
        egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
        galley,
        theme::TEXT_FAINT,
    );
    // 线从文字右侧 6px 开始，一直画到右边界。
    let y = rect.center().y;
    p.line_segment(
        [
            egui::pos2(rect.left() + text_w + 6.0, y),
            egui::pos2(rect.right(), y),
        ],
        Stroke::new(1.0, theme::LINE),
    );
}

/// 折线图（种群 / 能量曲线）。
///
/// 自己画而不是引 `egui_plot`：这里只需要"一条折线 + 一个底框"，
/// 引一个绘图库要多背几千行代码和它自己的主题体系。
///
/// `samples` 是 0..1 归一化后的值，最新的一帧在最右边。
pub fn sparkline(ui: &mut Ui, height: f32, samples: &[f32], ink: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, theme::SUNKEN);
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::LINE),
        StrokeKind::Inside,
    );

    if samples.len() < 2 {
        return;
    }

    // 中线。有它才能看出"曲线是在涨还是在跌"，否则只能看出形状。
    let mid = rect.center().y;
    p.line_segment(
        [egui::pos2(rect.left(), mid), egui::pos2(rect.right(), mid)],
        Stroke::new(1.0, theme::LINE),
    );

    let step = rect.width() / (samples.len() - 1) as f32;
    let pts: Vec<egui::Pos2> = samples
        .iter()
        .enumerate()
        .map(|(i, v)| {
            egui::pos2(
                rect.left() + i as f32 * step,
                // 上下各留 2px，曲线贴边会被边框吃掉一半。
                rect.bottom() - 2.0 - v.clamp(0.0, 1.0) * (rect.height() - 4.0),
            )
        })
        .collect();
    p.add(egui::Shape::line(pts, Stroke::new(1.0, ink)));
}

/// 键位提示：一个小方框里放一个字母。放在按钮右边当快捷键说明。
pub fn keycap(ui: &mut Ui, key: &str) {
    let galley = ui.painter().layout_no_wrap(
        key.to_owned(),
        FontId::new(9.0, egui::FontFamily::Monospace),
        theme::TEXT_FAINT,
    );
    let size = galley.size() + Vec2::new(6.0, 2.0);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::LINE),
        StrokeKind::Inside,
    );
    p.galley(
        rect.center() - galley.size() / 2.0,
        galley,
        theme::TEXT_FAINT,
    );
}

/// 一根竖直分隔线，高度撑满可用空间。
///
/// 面板之间用它分界，而不是靠背景色差：色差在两个都是深色面板之间
/// 几乎看不出来，而 1px 亮线一眼就分得清。
pub fn vline(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, ui.available_height()), Sense::hover());
    ui.painter().line_segment(
        [rect.center_top(), rect.center_bottom()],
        Stroke::new(1.0, theme::LINE),
    );
}
