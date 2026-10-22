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

use vivarium_core::{palette, CellType};

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
    /// **危险**：点下去会丢掉东西（当前世界、存档）。
    ///
    /// 单独一个状态而不是"Normal + 红字"：红色在这套皮肤里只有两个用途
    /// ——出错，和"要毁掉东西"。两者共用一种颜色是可以的，但**不能靠
    /// 调用方自己记得染红**：那样第一个忘记染的地方就是一个静默的陷阱。
    Danger,
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
            BtnState::Danger => theme::DANGER,
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
    } else if state == BtnState::Danger {
        // 悬停时把底稍微提亮，但不换成 `HOVER`——危险按钮在悬停时
        // 变得更"亮"会读成"更安全"，而它恰恰是唯一需要人犹豫一下的那个。
        let fill = if hovered {
            Color32::from_rgb(0x3A, 0x12, 0x0C)
        } else {
            Color32::from_rgb(0x24, 0x0B, 0x07)
        };
        (fill, theme::DANGER, theme::DANGER)
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
    /// 生物橡皮：一个圆（细胞）加斜杠。
    EraseLife,
    /// 点光源：一个圆心加四道射线。
    Lamp,
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
    /// 生物工坊：一个 2×2 的方格（"画身体"这件事最直白的图标）。
    Workshop,
    /// 世界：一个地球（圆 + 赤道 + 两条经线）。
    Globe,
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
        Icon::EraseLife => {
            // 一个圆（细胞）+ 一道斜杠。和物质橡皮的区别只在"擦的是什么"，
            // 所以图标也只换那一个形状：方块 → 圆。
            p.circle_stroke(egui::pos2(cx, cy + 1.0), (rt - l) * 0.30, s);
            p.line_segment([egui::pos2(l + 1.0, b), egui::pos2(rt - 1.0, t)], s);
        }
        Icon::Lamp => {
            // 圆心 + 四道短射线。不放满八道：工具条只有 20px，
            // 八道射线在那个尺寸下会糊成一个太阳。
            p.circle_filled(egui::pos2(cx, cy), 2.0, ink);
            for (dx, dy) in [(0.0f32, -1.0f32), (0.0, 1.0), (-1.0, 0.0), (1.0, 0.0)] {
                p.line_segment(
                    [
                        egui::pos2(cx + dx * 4.0, cy + dy * 4.0),
                        egui::pos2(cx + dx * 7.0, cy + dy * 7.0),
                    ],
                    s,
                );
            }
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
        Icon::Workshop => {
            // 2×2 的四个小方格，左上那个填实——"在一张网格上摆东西"。
            let w = (rt - l) * 0.30;
            let h = (b - t) * 0.30;
            let q = |i: f32, j: f32| {
                Rect::from_min_size(
                    egui::pos2(
                        cx + (i - 0.5) * (w + 2.0) - w * 0.5,
                        cy + (j - 0.5) * (h + 2.0) - h * 0.5,
                    ),
                    Vec2::new(w, h),
                )
            };
            for i in 0..2 {
                for j in 0..2 {
                    let r = q(i as f32, j as f32);
                    if i == 0 && j == 0 {
                        p.rect_filled(r, CornerRadius::ZERO, ink);
                    } else {
                        p.rect_stroke(r, CornerRadius::ZERO, s, StrokeKind::Inside);
                    }
                }
            }
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
        Icon::Globe => {
            // 圆 + 赤道 + 两条经线。经线用折线近似一个竖椭圆——
            // 在 14px 的尺寸上，四条折线已经读得出"球"了。
            let r = (rt - l) * 0.5;
            p.circle_stroke(egui::pos2(cx, cy), r, s);
            p.line_segment([egui::pos2(l, cy), egui::pos2(rt, cy)], s);
            for dir in [-1.0f32, 1.0] {
                let pts: Vec<egui::Pos2> = (0..=4)
                    .map(|i| {
                        let a = std::f32::consts::PI * i as f32 / 4.0;
                        egui::pos2(
                            cx + dir * r * 0.5 * a.sin(),
                            cy - r * a.cos(),
                        )
                    })
                    .collect();
                p.add(egui::Shape::line(pts, s));
            }
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
    chip(ui, color, label, hint, selected, true)
}

/// 图例色块：长得像色块，但**不能点**，悬停也不变色。
///
/// 这个区分是必须的。第一版把底部那排细胞做成了 `swatch`
/// （`Sense::click()` + 悬停白边），于是它和上面那排材质按钮长得一模一样，
/// 点下去却什么也不发生——玩家的第一个问题就是"这一排是干什么用的"。
///
/// **一个控件如果长得可以点，就必须能点。** 做不到，就把它画成不可点的样子。
pub fn legend_chip(ui: &mut Ui, color: Color32, label: &str) -> Response {
    chip(ui, color, label, None, false, false)
}

fn chip(
    ui: &mut Ui,
    color: Color32,
    label: &str,
    hint: Option<&str>,
    selected: bool,
    interactive: bool,
) -> Response {
    let size = Vec2::new(theme::SWATCH + 10.0, theme::SWATCH + 16.0);
    let sense = if interactive {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    // 只有可点的东西才在悬停时发亮。图例跟着发亮，等于在骗人去点它。
    let lit = interactive && resp.hovered();

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
    } else if lit {
        Color32::WHITE
    } else if interactive {
        theme::LINE_STRONG
    } else {
        // 图例用更暗的框：它只是"这个颜色是什么"，不是"点这里"。
        theme::LINE
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
    } else if lit {
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
        Some(h) if interactive => resp.on_hover_text(h),
        _ => resp,
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

// ---------------------------------------------------------------------------
// 检查器专用：缩略图、条形、权重矩阵
// ---------------------------------------------------------------------------

/// 身体缩略图：把 `(dx, dy, 细胞类型)` 画成一片小方块。
///
/// ## 为什么不用纹理
///
/// 世界里的鱼是着色器画的（圆角、膜、按格心打光）。缩略图**故意不复制那套**：
/// 这里要回答的是"它的形状是什么"，不是"它长什么样"。方块阵列在小尺寸下
/// 更清楚——一颗 3px 的圆看不出是圆的，一个 3px 的方块一眼就是个格子。
///
/// 坐标系与世界一致：`+x` 向右、`+y` 向下，锚点用一圈亮框标出来
/// （锚点不是装饰：感知深度、拥挤判定都用它）。
pub fn thumbnail(ui: &mut Ui, height: f32, shape: &[(i16, i16, u8)]) -> Response {
    let (rect, resp) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, theme::SUNKEN);
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::LINE),
        StrokeKind::Inside,
    );

    if shape.is_empty() {
        return resp;
    }

    // 包围盒。
    let (mut min_x, mut max_x, mut min_y, mut max_y) = (i16::MAX, i16::MIN, i16::MAX, i16::MIN);
    for &(x, y, _) in shape {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    let (bw, bh) = ((max_x - min_x + 1) as f32, (max_y - min_y + 1) as f32);

    // 每格边长：留 6px 边距，且不超过 14px——再大就变成"一张图"而不是"一个图标"。
    let cell = ((rect.height() - 12.0) / bh)
        .min((rect.width() - 12.0) / bw)
        .clamp(1.0, 14.0);
    let grid = Vec2::new(bw * cell, bh * cell);
    let origin = rect.center() - grid * 0.5;

    for &(x, y, kind) in shape {
        // `x` 已经由主循环按朝向镜像过，这里不再翻——镜像只有一份实现。
        let px = origin.x + (x - min_x) as f32 * cell;
        let py = origin.y + (y - min_y) as f32 * cell;
        let c = CellType::from_u8(kind);
        let [r, g, b] = palette::cell_rgb(c);
        let body = Rect::from_min_size(egui::pos2(px, py), Vec2::splat((cell - 1.0).max(1.0)));
        p.rect_filled(body, CornerRadius::ZERO, Color32::from_rgb(r, g, b));
        // 锚点：(0,0) 那一格描一圈白边。
        if x == 0 && y == 0 {
            p.rect_stroke(
                body,
                CornerRadius::ZERO,
                Stroke::new(1.0, Color32::WHITE),
                StrokeKind::Outside,
            );
        }
    }

    resp
}

/// 一行 "标签 · 条形 · 数值"。
///
/// 输入 / 输出那两栏用它。条形的**零点在中间**（`signed`），因为神经网络的
/// 输入有正有负（方向分量 −1..+1），而从左边长的条看不出负号。
pub fn bar_row(ui: &mut Ui, label: &str, value: f32, signed: bool, ink: Color32) {
    let h = 13.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let p = ui.painter();

    let label_w = 52.0;
    let value_w = 34.0;
    p.text(
        egui::pos2(rect.left(), rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::new(10.0, egui::FontFamily::Proportional),
        theme::TEXT_DIM,
    );

    let track = Rect::from_min_max(
        egui::pos2(rect.left() + label_w, rect.center().y - 2.0),
        egui::pos2(rect.right() - value_w, rect.center().y + 2.0),
    );
    p.rect_filled(track, CornerRadius::ZERO, theme::SUNKEN);

    let v = value.clamp(-1.0, 1.0);
    if signed {
        // 从中间往两边长。
        let mid = track.center().x;
        let half = track.width() * 0.5;
        let bar = Rect::from_min_max(
            egui::pos2(mid.min(mid + v * half), track.top()),
            egui::pos2(mid.max(mid + v * half), track.bottom()),
        );
        p.rect_filled(bar, CornerRadius::ZERO, ink);
        p.line_segment(
            [
                egui::pos2(mid, track.top()),
                egui::pos2(mid, track.bottom()),
            ],
            Stroke::new(1.0, theme::LINE),
        );
    } else {
        let bar = Rect::from_min_max(
            track.min,
            egui::pos2(track.left() + track.width() * v.max(0.0), track.bottom()),
        );
        p.rect_filled(bar, CornerRadius::ZERO, ink);
    }

    p.text(
        egui::pos2(rect.right(), rect.center().y),
        Align2::RIGHT_CENTER,
        format!("{value:+.2}"),
        FontId::new(10.0, egui::FontFamily::Monospace),
        if v.abs() < 0.02 {
            theme::TEXT_FAINT
        } else {
            ink
        },
    );
}

/// 一路输入的竖直条带：17 路并排，零点在中间。
///
/// ## 为什么是竖条，不是 17 行横条
///
/// 横条每行要占 13px，17 行就是 221px——检查器一屏只有六百多像素，
/// 光输入就吃掉三分之一。而"感知"这个信息的关键是**哪几路在动**，
/// 那是一个整体形状，不是 17 个独立读数。
///
/// 竖着排之后它变成一张 46px 高的"频谱"：一眼扫过去就知道
/// 这个身体此刻被哪几路输入占满了。要看具体数值就悬停。
///
/// 每根条自己带 tooltip，所以"第 13 根是什么"不需要另找图例。
pub fn input_strip(ui: &mut Ui, values: &[f32], names: &[&str]) -> Response {
    let h = 46.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::ZERO, theme::SUNKEN);

    let n = values.len().max(1);
    let slot = rect.width() / n as f32;
    let bar_w = (slot - 2.0).max(1.0);
    let mid = rect.center().y;
    let half = rect.height() * 0.5 - 2.0;

    for (i, &v) in values.iter().enumerate() {
        let v = v.clamp(-1.0, 1.0);
        let x = rect.left() + i as f32 * slot + 1.0;
        let top = if v >= 0.0 { mid - v * half } else { mid };
        let bot = if v >= 0.0 { mid } else { mid - v * half };
        let color = if v >= 0.0 { theme::INFO } else { theme::DANGER };
        let bar = Rect::from_min_max(
            egui::pos2(x, top),
            egui::pos2(x + bar_w, bot.max(top + 0.5)),
        );
        // 零值的条只画一条 1px 的底线，免得 17 根全黑看不出"这里有 17 路"。
        if v.abs() < 0.02 {
            p.rect_filled(
                Rect::from_min_max(egui::pos2(x, mid - 0.5), egui::pos2(x + bar_w, mid + 0.5)),
                CornerRadius::ZERO,
                theme::LINE,
            );
        } else {
            p.rect_filled(bar, CornerRadius::ZERO, color);
        }
    }

    // 中线：正负的分界。
    p.line_segment(
        [egui::pos2(rect.left(), mid), egui::pos2(rect.right(), mid)],
        Stroke::new(1.0, theme::LINE),
    );
    p.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::LINE),
        StrokeKind::Inside,
    );

    // 逐段悬停提示。分开分配 17 个 `Sense::hover()` 的小矩形，
    // 因为 egui 的 tooltip 是挂在具体矩形上的——整块一个 tooltip
    // 只能显示"这是输入"，那等于没有。
    let mut hovered: Option<usize> = None;
    for (i, _) in values.iter().enumerate() {
        let x = rect.left() + i as f32 * slot;
        let r = Rect::from_min_size(egui::pos2(x, rect.top()), Vec2::new(slot, rect.height()));
        if ui.rect_contains_pointer(r) {
            hovered = Some(i);
        }
    }
    let resp = match hovered {
        Some(i) if i < names.len() => resp.on_hover_text(format!(
            "{}：{:+.2}",
            names[i],
            values.get(i).copied().unwrap_or(0.0)
        )),
        _ => resp,
    };
    resp
}

/// 权重矩阵热力图：行是输入、列是输出。
///
/// 为什么是矩阵而不是"画一张网络图"：17×6 的连线画出来是一团毛线，
/// 而且连线长度、弯曲、交叉全都在骗人。矩阵把每个权重放在**固定位置**，
/// 扫一眼就能看出"哪一行全亮"（这一路输入接了好几个输出）。
///
/// 颜色只用两种：正权重绿、负权重红，亮度按 `|w| / WEIGHT_LIMIT`。
/// 不用彩虹色是因为"正负"才是权重的语义，大小只是程度。
pub fn weight_matrix(ui: &mut Ui, rows: usize, cols: usize, weights: &[f32]) -> Response {
    let cell = 7.0;
    let gap = 1.0;
    let size = Vec2::new(
        cols as f32 * (cell + gap) - gap,
        rows as f32 * (cell + gap) - gap,
    );
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), size.y + 2.0),
        Sense::hover(),
    );
    let p = ui.painter();
    let origin = egui::pos2(rect.left(), rect.top() + 1.0);

    for r in 0..rows {
        for c in 0..cols {
            let w = weights.get(r * cols + c).copied().unwrap_or(0.0);
            let t = (w.abs() / vivarium_core::life::WEIGHT_LIMIT).clamp(0.0, 1.0);
            let color = if w >= 0.0 {
                // 正权重：从底色渐亮到强调绿。
                lerp_color(theme::SUNKEN, theme::ACCENT, t)
            } else {
                lerp_color(theme::SUNKEN, theme::DANGER, t)
            };
            let q = Rect::from_min_size(
                egui::pos2(
                    origin.x + c as f32 * (cell + gap),
                    origin.y + r as f32 * (cell + gap),
                ),
                Vec2::splat(cell),
            );
            p.rect_filled(q, CornerRadius::ZERO, color);
        }
    }
    resp
}

/// 两个颜色之间线性插值。`t = 0` 取 `a`，`t = 1` 取 `b`。
fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgb(f(a.r(), b.r()), f(a.g(), b.g()), f(a.b(), b.b()))
}
