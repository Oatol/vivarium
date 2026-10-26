//! 「全局参数」窗口：世界温度 + 生态开关 + 物理模型 + 性能。
//!
//! ## 它解决的问题
//!
//! 到 v0.53 为止，这个项目里已经攒了三个"改了它世界就换了一种跑法"的开关
//! （藻席生长、水循环、陆地繁殖）和一个温度场。它们全都只能从代码或命令行
//! 参数打开——也就是说，**只有写代码的人能用**。而它们恰恰是"这个世界会
//! 变成什么样"最直接的几个旋钮。
//!
//! ## 为什么这些开关不该藏在命令行里
//!
//! 开关的价值在于**对照**：同一份地形、同一个种子，开着跑 8000 tick 和
//! 关着跑 8000 tick，差别才说明问题。命令行参数做不到这件事——改一次要
//! 重启，重启就把世界丢了。所以它们必须能在**世界正在跑的时候**翻。
//!
//! ## 界面不知道"这个开关会改变什么"
//!
//! 这里没有一条规则是"打开水循环之后世界会怎样"。界面只做三件事：
//! 把当前值显示出来、把点击翻译成一条 [`Command`]、把 main 算好的派生量
//! （比如光合效率）排版成字。所以这一整块能在没有世界的机器上测。

use egui::{Align, Color32, CornerRadius, Layout, Stroke, Ui, Vec2};

use crate::state::{Snapshot, UiState};
use crate::theme;
use crate::widgets::{self, BtnState};
use crate::Command;

/// 窗口内容宽度（逻辑像素）。
///
/// 比「世界」窗口（720）窄：那一块要排一张四列的存档表，这里最长的内容
/// 是"蒸发 → 水汽 → 降水"这种说明文字，560 已经宽到会留白。
const CONTENT_W: f32 = 560.0;

/// 环境温度滑块的显示范围。
///
/// 两端**必须**和 core 的 `AMBIENT_MIN` / `AMBIENT_MAX` 一致，所以下面有
/// 一条编译期断言把它们绑在一起。不一致的表现是"拖到头了数值还在涨"
/// （或反过来）——而 `set_ambient_temp` 自己会夹，所以那是**安全但不准**，
/// 比坏档更难发现。
const TEMP_MIN: f32 = -20.0;
const TEMP_MAX: f32 = 80.0;
const TEMP_RANGE: std::ops::RangeInclusive<f32> = TEMP_MIN..=TEMP_MAX;

const _: () = assert!(TEMP_MIN == vivarium_core::field::AMBIENT_MIN);
const _: () = assert!(TEMP_MAX == vivarium_core::field::AMBIENT_MAX);

/// 工人数滑块的范围。
const THREAD_RANGE: std::ops::RangeInclusive<f32> = 1.0..=16.0;

/// 画一次「全局参数」窗口。返回 `true` 表示它吃掉了这一帧的鼠标。
pub fn draw(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) -> bool {
    if !state.params.open {
        return false;
    }

    // 整窗遮罩：和生物工坊 / 「世界」同一个理由。遮罩是**画上去的**，
    // egui 不会把它算进 `pointer_over_ui`，所以主循环要靠 `Output::modal`
    // 才知道"别往世界里落笔"。
    let full = ui.ctx().content_rect();
    ui.painter()
        .rect_filled(full, CornerRadius::ZERO, Color32::from_black_alpha(200));

    let mut close = false;
    // `default_width` 是必须的，理由写在 `world_window` 里：不给初始宽度
    // 则第一列一直撑，后面的东西被推到屏幕外，`clip_rect` 宽度变负数。
    egui::Window::new("vivarium-params")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .default_width(CONTENT_W + 20.0)
        .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
        .frame(
            egui::Frame::NONE
                .fill(theme::PANEL)
                .stroke(Stroke::new(1.0, theme::LINE_STRONG))
                .inner_margin(egui::Margin {
                    left: 10,
                    right: 10,
                    top: 8,
                    bottom: 8,
                }),
        )
        .show(ui.ctx(), |ui| {
            ui.set_width(CONTENT_W);
            ui.set_max_width(CONTENT_W);
            if header(ui) {
                close = true;
            }
            ui.add_space(6.0);
            world_section(ui, state, snap);
            ui.add_space(8.0);
            ecology_section(ui, state, snap);
            ui.add_space(8.0);
            physics_section(ui, state, snap);
            ui.add_space(8.0);
            perf_section(ui, state, snap);
            ui.add_space(6.0);
            footer(ui);
        });

    if close {
        state.params.open = false;
    }
    true
}

/// 标题行。返回 `true` 表示点了关闭。
fn header(ui: &mut Ui) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("全局参数")
                .size(theme::TITLE_SIZE + 2.0)
                .color(theme::TEXT),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("温度 · 生态 · 物理 · 性能")
                .size(9.5)
                .color(theme::TEXT_FAINT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::flat_button(ui, "关闭", BtnState::Normal).clicked() {
                close = true;
            }
        });
    });
    close
}

/// 世界：环境温度 + 它的读数。
fn world_section(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) {
    widgets::section(ui, "世界");
    ui.add_space(2.0);

    // 拖滑块时**每帧发一条命令**。这是有意的：温度是"目标值"，
    // 连续变化正是想要的手感。不用本地副本、不用松手才生效——
    // 那样滑块会拖得动、世界却没反应，而玩家分不清是哪种。
    let mut t = snap.ambient_temp;
    if widgets::value_slider(ui, "环境温度", &mut t, TEMP_RANGE, " °C").changed() {
        state.emit(Command::SetAmbientTemp(t));
    }

    ui.add_space(2.0);
    // 派生读数。**光合效率**是这一块唯一"看得见后果"的数字：
    // 温度在这份代码里除了冷却以外只有一条出路（`photosynthesis`），
    // 不把它显示出来，这个滑块看起来就只是让滤镜变色而已。
    //
    // 标签里的"（环境）"不是凑字数：这个数是按**环境温度**算的，不是当场
    // 实测——世界可能正被一团火烧到 800 °C，而那一格的光合早就归零了。
    // 两者并排放着才是诚实的：上面是"这个旋钮会把它推到多少"，
    // 下面"实测平均温度"是"现在实际是多少"，中间的差就是还没走完的松弛。
    widgets::stat_row(
        ui,
        "光合效率（环境）",
        &format!("{:.0}%", snap.warmth * 100.0),
        if snap.warmth > 0.6 {
            theme::ACCENT
        } else if snap.warmth > 0.05 {
            theme::WARN
        } else {
            theme::DANGER
        },
    );
    widgets::stat_row(
        ui,
        "实测平均温度",
        &format!("{:.0} °C", snap.field.avg_temp),
        theme::TEXT,
    );
    widgets::stat_row(
        ui,
        "实测温度范围",
        &format!(
            "{:.0} … {:.0} °C",
            snap.field.temp_range.0, snap.field.temp_range.1
        ),
        theme::TEXT_DIM,
    );

    ui.add_space(3.0);
    ui.horizontal(|ui| {
        if widgets::flat_button(ui, "温度场拉回环境温度", BtnState::Normal).clicked() {
            state.emit(Command::ResetTemperature);
        }
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("温度是目标值，现有热量会自己慢慢走过去（约 2000 tick）")
                .size(9.0)
                .color(theme::TEXT_FAINT),
        );
    });
}

/// 生态：三个开关。
///
/// 三行的高度、标签列宽、状态牌位置完全一致——它们是**同一类东西**，
/// 而一列对齐的开关扫一眼就能读出全部状态。这就是不用三个各自为政的
/// `checkbox` 的理由。
fn ecology_section(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) {
    widgets::section(ui, "生态开关");
    ui.add_space(2.0);

    if widgets::toggle_row(
        ui,
        "藻席生长",
        snap.food_growth,
        "海底的天然碎屑上长出藻席（食物）。关掉它是给对照实验用的。",
    )
    .changed()
    {
        state.emit(Command::SetFoodGrowth(!snap.food_growth));
    }

    if widgets::toggle_row(
        ui,
        "水循环",
        snap.water_cycle,
        "蒸发 → 水汽 → 降水。**会改水位**：开久了海会变浅、陆地会变湿。",
    )
    .changed()
    {
        state.emit(Command::SetWaterCycle(!snap.water_cycle));
    }

    if widgets::toggle_row(
        ui,
        "陆地繁殖",
        snap.land_reproduction,
        "子代可以站在实体上出生（不要求泡在水里）。**会改水位**：门后的碎屑还没有出口。",
    )
    .changed()
    {
        state.emit(Command::SetLandReproduction(!snap.land_reproduction));
    }

    ui.add_space(3.0);
    ui.label(
        egui::RichText::new("开关会立刻生效，但不会重算已经跑过的世界。")
            .size(9.0)
            .color(theme::TEXT_FAINT),
    );
}

/// 物理：换掉"身体和水的关系"的那一个开关。
///
/// 单独一节，而不是塞进「生态开关」：那三个开关各自开关一条规则，
/// 这一个换的是**规则之间的关系**——身体还借不借水、移动还顶不顶水。
/// 把它们排成一列会让人以为它们是同一层级的东西。
fn physics_section(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) {
    widgets::section(ui, "物理");
    ui.add_space(2.0);

    if widgets::toggle_row(
        ui,
        "水与生物共格",
        snap.share_water,
        "水留在原地，身体叠在它上面。关着时身体会把那格水借走、移动时把它顶开。\
         **改的是物理规则本身**，已经跑过的世界不会被重算。",
    )
    .changed()
    {
        state.emit(Command::SetShareWater(!snap.share_water));
    }

    ui.label(
        egui::RichText::new("开着时身体不借水、也不顶水；关着时两者都发生（旧模型）。")
            .size(9.0)
            .color(theme::TEXT_FAINT),
    );

    ui.add_space(6.0);

    if widgets::toggle_row(
        ui,
        "碎屑压实成地形",
        snap.compact_remains,
        "被埋住的碎屑会慢慢压成沙：**两格碎屑挤成一格沙**，体积减半。\
         关着时碎屑只进不出，堆在海里排开海水，水位会一路涨。",
    )
    .changed()
    {
        state.emit(Command::SetCompactRemains(!snap.compact_remains));
    }

    ui.label(
        egui::RichText::new(if snap.compact_remains {
            "开着：碎屑 → 沙，总量会停下来，水位不再涨。"
        } else {
            "关着：碎屑是吸收态，堆多少留多少（水位会跟着涨）。"
        })
        .size(9.0)
        .color(theme::TEXT_FAINT),
    );
}

/// 性能：并行工人数。
fn perf_section(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) {
    widgets::section(ui, "性能");
    ui.add_space(2.0);

    let mut n = snap.threads as f32;
    if widgets::value_slider(ui, "工人数", &mut n, THREAD_RANGE, "").changed() {
        state.emit(Command::SetThreads(n.round().max(1.0) as usize));
    }
    // "上限"和"实际用到"是两个数：世界静止时 `step` 一格都不扫，
    // 那时显示 1 个工人是**正确**的，不是"设置没生效"。
    widgets::stat_row(
        ui,
        "上次实际并行",
        &format!("{} / {}", snap.workers, snap.threads),
        theme::TEXT_DIM,
    );
    ui.label(
        egui::RichText::new("调低它可以让满载的机器凉下来；世界静止时用不到工人。")
            .size(9.0)
            .color(theme::TEXT_FAINT),
    );
}

/// 底部：热键提示。
fn footer(ui: &mut Ui) {
    ui.horizontal(|ui| {
        widgets::keycap(ui, "K");
        ui.label(
            egui::RichText::new("开/关这个窗口")
                .size(9.0)
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(10.0);
        widgets::keycap(ui, "G");
        ui.label(
            egui::RichText::new("世界（新建 / 保存 / 读取）")
                .size(9.0)
                .color(theme::TEXT_FAINT),
        );
    });
}
