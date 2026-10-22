//! 配色与全局样式。
//!
//! ## 参照了什么
//!
//! **The Powder Toy** 的界面几乎全是"纯黑 + 1px 亮灰边框 + 反色高亮"：
//! 面板底色 `#000000`，边框 `#C8C8C8`，悬停底色 `#141414`，
//! 按下时**整个按钮反色**（白底黑字）。没有圆角、没有阴影、没有渐变。
//! 它看起来很"工程"，但有一个实际好处：**世界里的颜色是主角**，
//! 界面只提供骨架，不和像素抢注意力。
//!
//! **Alien** 是另一套取向：深灰面板、分栏、右侧检查器里塞满实时数值。
//! 我们从它那里拿的是**布局**（左工具条 / 中视口 / 右检查器 / 下控制台），
//! 不是配色。
//!
//! 所以这里：**结构照 Alien，皮肤照 TPT，但底色再压暗一点并偏蓝**。
//! 偏蓝是因为世界的主体是水（蓝绿色），纯黑面板贴在旁边会显得脏；
//! 一点点蓝让"界面"和"水"分开，又不会变成另一种颜色。
//!
//! ## 为什么不用 egui 默认主题
//!
//! egui 默认是圆角、有阴影、灰蓝渐变的"现代桌面"风格。和这个项目的气质
//! 完全不搭：这里的每一格像素都是有意义的，界面不该有装饰性的柔和边缘。
//! 而且自绘控件的判据（见 `widgets.rs`）依赖"边框就是 1px 实线"这个前提。

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

// ---------------------------------------------------------------------------
// 调色板
// ---------------------------------------------------------------------------

/// 世界视口之外的空。比面板更黑，用来让"面板浮在世界上"这件事看得出来。
pub const VOID: Color32 = Color32::from_rgb(0x05, 0x07, 0x0A);

/// 面板底色。
pub const PANEL: Color32 = Color32::from_rgb(0x0B, 0x0F, 0x14);

/// 面板里"凸起"的块（按钮底、色块底、表格行）。
pub const RAISED: Color32 = Color32::from_rgb(0x12, 0x17, 0x1E);

/// 面板里"凹陷"的块（输入框、控制台、滑杆轨道）。
pub const SUNKEN: Color32 = Color32::from_rgb(0x06, 0x09, 0x0C);

/// 悬停底色。取自 TPT 的 `BackgroundHover = #141414`，只是跟着底色偏了蓝。
pub const HOVER: Color32 = Color32::from_rgb(0x18, 0x1F, 0x27);

/// 1px 分隔线。
pub const LINE: Color32 = Color32::from_rgb(0x22, 0x2A, 0x34);

/// 悬停 / 选中的边框。TPT 的 `BorderInactive = #C8C8C8` 在纯黑上很好，
/// 但在这个偏蓝的底上太扎眼，压到 0x3A4653。
pub const LINE_STRONG: Color32 = Color32::from_rgb(0x3A, 0x46, 0x53);

/// 正文。
pub const TEXT: Color32 = Color32::from_rgb(0xC6, 0xD2, 0xDE);

/// 次要文字（单位、说明）。
pub const TEXT_DIM: Color32 = Color32::from_rgb(0x6B, 0x78, 0x85);

/// 最弱的文字（占位符、禁用）。
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x45, 0x50, 0x5B);

/// 主强调色：生命绿。和藻席、生产者细胞的颜色是同一族，
/// 于是"界面在说生命的事"这件事不需要解释。
pub const ACCENT: Color32 = Color32::from_rgb(0x6F, 0xD3, 0xA8);

/// 强调色的暗部，用于选中态的底色。
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x1B, 0x3D, 0x31);

/// 警告（TPT 的 `WarningTitle`，原样保留）。
pub const WARN: Color32 = Color32::from_rgb(0xFF, 0xD8, 0x20);

/// 错误（TPT 的 `ErrorTitle`，原样保留）。
pub const DANGER: Color32 = Color32::from_rgb(0xFF, 0x40, 0x20);

/// 信息（TPT 的 `InformationTitle`，原样保留）。
pub const INFO: Color32 = Color32::from_rgb(0x8C, 0x8C, 0xFF);

// ---------------------------------------------------------------------------
// 尺寸
// ---------------------------------------------------------------------------

/// 顶栏高度。放得下一行 12px 字 + 上下各 5px 留白。
pub const TOP_BAR_H: f32 = 30.0;

/// 左侧工具条宽度。28px 的按钮 + 两侧 4px 留白。
pub const TOOL_STRIP_W: f32 = 36.0;

/// 工具按钮边长。
pub const TOOL_BTN: f32 = 28.0;

/// 右侧检查器宽度。
pub const INSPECTOR_W: f32 = 268.0;

/// 底部调色板高度（色块 22px + 标签 + 留白）。
/// 底部调色板的高度。
///
/// 从 54 涨到 72 是为了腾出一行**描述文字**：选中材质 / 细胞时，
/// 那一行会说明"它会做什么"。色块本身只回答"是什么颜色"，
/// 而新手最需要知道的是"点了它会发生什么"。
pub const PALETTE_H: f32 = 72.0;

/// 底部控制台高度。
pub const CONSOLE_H: f32 = 104.0;

/// 调色板色块边长。
pub const SWATCH: f32 = 22.0;

// ---------------------------------------------------------------------------
// 字体
// ---------------------------------------------------------------------------

/// 等宽族名。数字全靠它对齐——`312` 和 `3120` 差一位，
/// 用比例字体时整列数字会左右跳，读起来像在闪。
pub const MONO: &str = "mono";

/// 界面字号。
pub const TEXT_SIZE: f32 = 12.0;

/// 数字与代码字号。
pub const MONO_SIZE: f32 = 12.0;

/// 标题字号（面板分组标题）。
pub const TITLE_SIZE: f32 = 11.0;

/// 把主题装进 `egui::Context`。
///
/// 只在启动时调用一次。之后所有控件都用 [`egui::Ui::visuals`] 里的这套。
pub fn install(ctx: &egui::Context) {
    // 0.37 把 `style()` / `set_style()` 换成了按主题分开的一套。
    // 这里强制暗色，然后同时改两份（明/暗）——万一系统主题是亮的，
    // 也不会突然冒出一套浅色界面。
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(apply);
}

/// 把主题写进一份 `Style`。
fn apply(style: &mut egui::Style) {
    let mut v = egui::Visuals::dark();

    // 全局底色。面板自己会盖住需要盖的地方，这个只是兜底。
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = SUNKEN;
    v.faint_bg_color = RAISED;
    v.code_bg_color = SUNKEN;

    // **没有圆角、没有阴影。** 这是 TPT 的皮肤，也是自绘控件的判据前提。
    v.window_corner_radius = CornerRadius::ZERO;
    v.menu_corner_radius = CornerRadius::ZERO;
    v.window_shadow = egui::epaint::Shadow::NONE;
    v.popup_shadow = egui::epaint::Shadow::NONE;
    v.window_stroke = Stroke::new(1.0, LINE_STRONG);

    v.override_text_color = Some(TEXT);
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARN;
    v.error_fg_color = DANGER;

    v.selection.bg_fill = ACCENT_DIM;
    v.selection.stroke = Stroke::new(1.0, ACCENT);

    // 控件三态：默认只有 1px 边框，悬停才出底色，按下反色。
    // 反色的判据在 `widgets::flat_button` 里手写，这里只把"默认态"定死。
    let w = &mut v.widgets;
    w.noninteractive.bg_fill = PANEL;
    w.noninteractive.weak_bg_fill = PANEL;
    w.noninteractive.bg_stroke = Stroke::new(1.0, LINE);
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT_DIM);
    w.noninteractive.corner_radius = CornerRadius::ZERO;

    w.inactive.bg_fill = RAISED;
    w.inactive.weak_bg_fill = RAISED;
    w.inactive.bg_stroke = Stroke::new(1.0, LINE_STRONG);
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = CornerRadius::ZERO;

    w.hovered.bg_fill = HOVER;
    w.hovered.weak_bg_fill = HOVER;
    w.hovered.bg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.hovered.corner_radius = CornerRadius::ZERO;

    w.active.bg_fill = TEXT;
    w.active.weak_bg_fill = TEXT;
    w.active.bg_stroke = Stroke::new(1.0, Color32::WHITE);
    w.active.fg_stroke = Stroke::new(1.0, PANEL);
    w.active.corner_radius = CornerRadius::ZERO;

    w.open.bg_fill = HOVER;
    w.open.weak_bg_fill = HOVER;
    w.open.bg_stroke = Stroke::new(1.0, LINE_STRONG);
    w.open.fg_stroke = Stroke::new(1.0, TEXT);
    w.open.corner_radius = CornerRadius::ZERO;

    // 间距压紧。TPT 的信息密度很高，这里也是：一屏要能同时看到
    // 世界、工具、调色板、控制台、检查器，留白多了就什么都放不下。
    style.spacing.item_spacing = egui::vec2(4.0, 4.0);
    style.spacing.button_padding = egui::vec2(6.0, 3.0);
    style.spacing.window_margin = egui::Margin::same(6);
    style.spacing.menu_margin = egui::Margin::same(4);
    style.spacing.indent = 10.0;
    style.spacing.slider_width = 120.0;
    style.spacing.interact_size = egui::vec2(28.0, 20.0);

    // 字号：所有文字都显式指定，不依赖 egui 的默认比例。
    style.text_styles = [
        (
            TextStyle::Small,
            FontId::new(TITLE_SIZE, FontFamily::Proportional),
        ),
        (
            TextStyle::Body,
            FontId::new(TEXT_SIZE, FontFamily::Proportional),
        ),
        (
            TextStyle::Button,
            FontId::new(TEXT_SIZE, FontFamily::Proportional),
        ),
        (
            TextStyle::Heading,
            FontId::new(13.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Monospace,
            FontId::new(MONO_SIZE, FontFamily::Monospace),
        ),
    ]
    .into();

    style.visuals = v;
}

// ---------------------------------------------------------------------------
// 字体装载
// ---------------------------------------------------------------------------

/// 装中文字体。
///
/// ## 为什么要手动装
///
/// egui 内置字体（Ubuntu-Light / NotoEmoji / Hack）**不含中日韩字形**。
/// 不装的话，这个项目里所有中文都会变成一排方块——而界面文案全是中文，
/// 那不是"显示不好看"，那是整个界面不可读。
///
/// ## 为什么不把字体打进二进制
///
/// 一个中文字体 10~20 MB，而 `dist/vivarium.exe` 现在只有 6.8 MB。
/// 打进去体积翻三倍，为了一个"界面上有汉字"的需求不值得。
/// 改成**启动时从系统里找**：找不到就退回英文标签，程序照常能跑。
///
/// ## 候选顺序
///
/// 雅黑 → 黑体 → 宋体。雅黑是 Vista 之后 Windows 的默认界面字体，
/// 字形最接近界面该有的样子；黑体是备胎；宋体只在很老的系统上才有。
pub fn install_cjk_font(ctx: &egui::Context) -> Option<String> {
    const CANDIDATES: &[&str] = &[
        r"C:\Windows\Fonts\msyh.ttc",   // 微软雅黑
        r"C:\Windows\Fonts\msyh.ttf",   // 旧版雅黑
        r"C:\Windows\Fonts\simhei.ttf", // 黑体
        r"C:\Windows\Fonts\simsun.ttc", // 宋体
        r"/System/Library/Fonts/PingFang.ttc",
        r"/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ];

    for path in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        // `.ttc` 是字体集合，`index = 0` 取第一个面。雅黑的第一个面就是
        // "Microsoft YaHei"，不是它的粗体或 UI 变体。
        let data = egui::FontData::from_blob(std::sync::Arc::new(bytes), 0);
        fonts.font_data.insert("cjk".to_owned(), data.into());

        // **插在最前面**：中文字形优先用雅黑，拉丁字母也一起用它——
        // 混排时如果正文用 Ubuntu、汉字用雅黑，基线高度不一样，
        // 一行里中英文会上下跳。
        for family in [FontFamily::Proportional, FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, "cjk".to_owned());
        }
        ctx.set_fonts(fonts);
        return Some((*path).to_owned());
    }
    None
}
