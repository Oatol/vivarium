//! 「世界」窗口：新建、保存、读取。
//!
//! ## 它解决的问题
//!
//! 在这之前，"换一个世界"只有两条路：改命令行参数重启，或者按 R 换个种子。
//! 两条都不保留任何东西——关掉窗口，跑了一万 tick 的生态就没了。
//!
//! ## 为什么所有"会丢东西"的按钮都要点两次
//!
//! 新建和读取都会**无声地**毁掉当前世界。这里没有"另存为"可以兜底，
//! 也没有撤销：世界不是文档，它是一段跑过的历史。所以这两个动作都要
//! 二次确认，而且第一次点击必须**变成红字**——不是弹一个对话框，
//! 而是让同一个按钮原地变成"确认丢弃当前世界"。
//!
//! 为什么不弹对话框：对话框把"我点的是哪个按钮"这个上下文挡住了。
//! 原地变红的话，眼睛一直看着那个位置，看到的是"这个按钮现在会毁掉世界"。
//!
//! ## 界面不做的事
//!
//! 它不知道文件在哪、也不知道存档里是什么。读列表是主循环填好的
//! [`SaveEntry`]（连"1.0 MB""10-22 22:41"这种字符串都格式化好了），
//! 路径原样带回去。所以这一整块能在没有磁盘的机器上测。

use egui::{Align, Color32, CornerRadius, FontId, Layout, Stroke, Ui, Vec2};

use crate::state::{
    SaveEntry, Snapshot, UiState, WorldAction, MAX_HEIGHT, MAX_WIDTH, MIN_SIDE, PRESETS,
};
use crate::theme;
use crate::widgets::{self, BtnState};
use crate::Command;

/// 窗口内容宽度（逻辑像素）。
const CONTENT_W: f32 = 720.0;

/// 读档列表最多占多高。超出就滚动。
///
/// 固定高度而不是"随内容长高"：存档会有几十个，让窗口跟着长会把
/// 标题行顶到屏幕外，而"保存"和"读取"两个分区的位置每次打开都在变。
const LIST_H: f32 = 168.0;

/// 画一次「世界」窗口。返回 `true` 表示它吃掉了这一帧的鼠标。
pub fn draw(ui: &mut Ui, state: &mut UiState, snap: &Snapshot) -> bool {
    if !state.world.open {
        return false;
    }

    let now = ui.input(|i| i.time);
    state.world.expire_confirm(now);

    // 整窗遮罩：压暗背后的世界，也吞掉点击。
    //
    // 和生物工坊同一个理由：遮罩是**画上去的**，egui 不会把它算进
    // `pointer_over_ui`，所以主循环要靠 `Output::modal` 才知道"别往世界里落笔"。
    let full = ui.ctx().content_rect();
    ui.painter()
        .rect_filled(full, CornerRadius::ZERO, Color32::from_black_alpha(200));

    let mut close = false;
    // 用 `Window` 而不是 `Area`，并且**必须**给 `default_width`：
    // `Area` 的尺寸由内容决定，配 `available_width()` 会互相等待算出 0 宽，
    // 整块面板一个图元都不画；`Window` 不给初始宽度则第一列一直撑，
    // 把后面的东西推到屏幕外，`clip_rect` 宽度变负数。两条都是实测踩过的。
    egui::Window::new("vivarium-world")
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
            summary(ui, snap);
            ui.add_space(6.0);
            new_section(ui, state, now);
            ui.add_space(8.0);
            save_section(ui, state);
            ui.add_space(8.0);
            load_section(ui, state, now);
            if let Some(hint) = &state.world.hint {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(hint.as_str())
                        .size(9.5)
                        .color(theme::TEXT_DIM),
                );
            }
        });

    if close {
        state.world.open = false;
    }
    true
}

/// 标题行。返回 `true` 表示点了关闭。
fn header(ui: &mut Ui) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("世界")
                .size(theme::TITLE_SIZE + 2.0)
                .color(theme::TEXT),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("新建 · 保存 · 读取")
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

/// 顶上那行"当前世界"。
///
/// 一行之内把"我现在在玩什么"说完：尺寸、tick、活着的个体数、物种数。
/// 单独一条凹槽（`SUNKEN` 底）而不是普通文字：它是**读数**不是**控件**，
/// 而面板里全是可点控件的时候，一块不带边框的底能让人立刻分出两类东西。
fn summary(ui: &mut Ui, snap: &Snapshot) {
    let h = 26.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), egui::Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::ZERO, theme::SUNKEN);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::LINE),
        egui::StrokeKind::Inside,
    );

    let text = format!(
        "当前世界   {} × {}  ·  tick {}  ·  {} 只  ·  物种 {}",
        snap.world_size.0, snap.world_size.1, snap.tick, snap.organisms, snap.species
    );
    ui.painter().text(
        egui::pos2(rect.left() + 8.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::new(theme::MONO_SIZE, egui::FontFamily::Monospace),
        theme::TEXT,
    );
}

/// 新建分区。
fn new_section(ui: &mut Ui, state: &mut UiState, now: f64) {
    widgets::section(ui, "新建");
    ui.add_space(2.0);

    // 两条滑块各占一行，而不是并排：`value_slider` 用 `available_width()`，
    // 并排就得手工分配宽度，而那两个宽度会和标签宽度（固定 62px）打架。
    let mut w = state.world.width as f32;
    if widgets::value_slider(
        ui,
        "宽",
        &mut w,
        MIN_SIDE as f32..=MAX_WIDTH as f32,
        " 格",
    )
    .changed()
    {
        state.world.width = (w.round() as u32).clamp(MIN_SIDE, MAX_WIDTH);
    }
    let mut h = state.world.height as f32;
    if widgets::value_slider(
        ui,
        "高",
        &mut h,
        MIN_SIDE as f32..=MAX_HEIGHT as f32,
        " 格",
    )
    .changed()
    {
        state.world.height = (h.round() as u32).clamp(MIN_SIDE, MAX_HEIGHT);
    }

    // 预设。
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("预设")
                .size(theme::TITLE_SIZE)
                .color(theme::TEXT_FAINT),
        );
        for (label, pw, ph) in PRESETS {
            let on = state.world.width == pw && state.world.height == ph;
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
                // **预设也要夹。** 它和滑块是两条独立的赋值路径，而
                // "尺寸合法"不能只靠其中一条守住——早先"大 768×728"就是
                // 从这个口子把 728 塞进去的，后果是点一下闪退。
                state.world.width = pw.clamp(MIN_SIDE, MAX_WIDTH);
                state.world.height = ph.clamp(MIN_SIDE, MAX_HEIGHT);
            }
        }
    });
    ui.add_space(3.0);

    // 种子。
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("种子")
                .size(theme::TITLE_SIZE)
                .color(theme::TEXT_FAINT),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.world.seed)
                .hint_text("留空 = 随机")
                .desired_width(140.0)
                .font(FontId::new(11.0, egui::FontFamily::Monospace)),
        );
        ui.label(
            egui::RichText::new("同一个种子必然长出同一张图")
                .size(9.0)
                .color(theme::TEXT_FAINT),
        );
    });
    ui.add_space(3.0);

    // 两个勾选。
    ui.horizontal(|ui| {
        ui.checkbox(&mut state.world.terrain, "生成地形");
        ui.add_space(10.0);
        ui.checkbox(&mut state.world.life, "随机投放初始生命（8 只）");
    });
    ui.add_space(6.0);

    // 红字常驻提醒。
    //
    // **常驻**，不是点了才出现：它要在人动手之前就在那儿。
    warning(ui, "新建 / 读取会丢弃当前世界，未保存的进度无法恢复。");
    ui.add_space(4.0);

    let action = WorldAction::New;
    let confirming = state.world.is_confirming(&action, now);
    let label = if confirming {
        "确认丢弃当前世界"
    } else {
        "新建世界"
    };
    let btn = if confirming {
        BtnState::Danger
    } else {
        BtnState::Normal
    };
    if widgets::flat_button(ui, label, btn).clicked() && state.world.confirm_click(action, now) {
        state.emit(Command::NewWorld {
            width: state.world.width,
            height: state.world.height,
            seed: state.world.parsed_seed(),
            terrain: state.world.terrain,
            life: state.world.life,
        });
    }
}

/// 保存分区。
fn save_section(ui: &mut Ui, state: &mut UiState) {
    widgets::section(ui, "保存");
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("文件名")
                .size(theme::TITLE_SIZE)
                .color(theme::TEXT_FAINT),
        );
        ui.add(
            egui::TextEdit::singleline(&mut state.world.save_name)
                .hint_text("world-20261022")
                .desired_width(180.0)
                .font(FontId::new(11.0, egui::FontFamily::Monospace)),
        );
        ui.label(
            egui::RichText::new(".vivarium")
                .size(9.5)
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(6.0);
        // 保存不丢东西，所以**不需要**二次确认。多一道确认会让"存一下"
        // 变成一件麻烦事，而存档正是最该被鼓励的动作。
        let ready = !state.world.save_name.trim().is_empty();
        let btn = if ready {
            BtnState::Normal
        } else {
            BtnState::Disabled
        };
        if widgets::flat_button(ui, "保存", btn).clicked() && ready {
            let name = state.world.save_name.trim().to_owned();
            state.emit(Command::SaveWorld(name));
        }
    });
    ui.add_space(2.0);
    // 存档目录**写出来**：1.x 的惯例是"导出落程序目录"，而玩家找不到文件
    // 时唯一的线索就是这一行。空的时候说明主循环还没填，不显示半句话。
    if !state.world.saves_dir.is_empty() {
        ui.label(
            egui::RichText::new(format!("目录  {}", state.world.saves_dir))
                .size(9.0)
                .color(theme::TEXT_FAINT),
        );
    }
}

/// 读取分区。
fn load_section(ui: &mut Ui, state: &mut UiState, now: f64) {
    widgets::section(ui, "读取");
    ui.add_space(2.0);

    if state.world.saves.is_empty() {
        ui.label(
            egui::RichText::new("saves/ 里还没有存档——先保存一个世界")
                .size(9.5)
                .color(theme::TEXT_FAINT),
        );
        return;
    }

    // 表头。列宽写死：这是**一张表**，每行的列必须对齐，
    // 而中文标签的宽度随字体变化，靠 `horizontal` 自动分配永远对不齐。
    // 第一个数是左边距，后面四个是"从上一列往右挪多少"。
    let cols = [0.0f32, 240.0, 100.0, 84.0, 80.0];
    header_row(ui, &cols);

    egui::ScrollArea::vertical()
        .max_height(LIST_H)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut want: Option<String> = None;
            // 列表先整体借出去：`row` 既要读这一行、又要改 `state.world.confirm`
            // （同一个结构体的两个字段），同时借会打架。`mem::take` 是 O(1)，
            // 借完再放回去——比 clone 一整张表干净，也不用把 `WorldPanel`
            // 拆成两个类型去迁就借用检查器。
            let saves = std::mem::take(&mut state.world.saves);
            for entry in &saves {
                row(ui, entry, &cols, state, now, &mut want);
            }
            state.world.saves = saves;
            if let Some(path) = want {
                state.emit(Command::LoadWorld(path));
            }
        });
}

/// 表头那一行。
fn header_row(ui: &mut Ui, cols: &[f32; 5]) {
    let h = 14.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), egui::Sense::hover());
    let p = ui.painter();
    let names = ["名字", "尺寸", "tick", "大小", "修改时间"];
    let mut x = rect.left() + cols[0];
    for (i, name) in names.iter().enumerate() {
        p.text(
            egui::pos2(x, rect.center().y),
            egui::Align2::LEFT_CENTER,
            *name,
            FontId::new(9.0, egui::FontFamily::Proportional),
            theme::TEXT_FAINT,
        );
        // 最后一列之后不用再挪（`cols` 只有 5 个，够挪 4 次）。
        if let Some(step) = cols.get(i + 1) {
            x += step;
        }
    }
    p.line_segment(
        [
            egui::pos2(rect.left(), rect.bottom()),
            egui::pos2(rect.right(), rect.bottom()),
        ],
        Stroke::new(1.0, theme::LINE),
    );
}

/// 一行存档。
fn row(
    ui: &mut Ui,
    entry: &SaveEntry,
    cols: &[f32; 5],
    state: &mut UiState,
    now: f64,
    want: &mut Option<String>,
) {
    let h = 20.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), egui::Sense::hover());
    let p = ui.painter();
    let mono = FontId::new(10.0, egui::FontFamily::Monospace);
    let prop = FontId::new(10.0, egui::FontFamily::Proportional);

    let mut x = rect.left() + cols[0];
    p.text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &entry.name,
        prop.clone(),
        theme::TEXT,
    );
    x += cols[1];
    p.text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{}×{}", entry.width, entry.height),
        mono.clone(),
        theme::TEXT_DIM,
    );
    x += cols[2];
    p.text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        format!("{}", entry.tick),
        mono.clone(),
        theme::TEXT_DIM,
    );
    x += cols[3];
    p.text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &entry.size,
        mono.clone(),
        theme::TEXT_DIM,
    );
    x += cols[4];
    p.text(
        egui::pos2(x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        &entry.modified,
        mono,
        theme::TEXT_FAINT,
    );

    // 读取按钮：右对齐，每行一个。第二次点击变红。
    let action = WorldAction::Load(entry.path.clone());
    let confirming = state.world.is_confirming(&action, now);
    let label = if confirming { "确认丢弃" } else { "读取" };
    let btn = if confirming {
        BtnState::Danger
    } else {
        BtnState::Normal
    };
    let btn_w = 76.0;
    let btn_rect = egui::Rect::from_min_size(
        egui::pos2(rect.right() - btn_w, rect.top()),
        Vec2::new(btn_w, h),
    );
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(btn_rect));
    if widgets::flat_button(&mut child, label, btn).clicked()
        && state.world.confirm_click(action, now)
    {
        *want = Some(entry.path.clone());
    }
}

/// 一行红字。
fn warning(ui: &mut Ui, text: &str) {
    ui.label(egui::RichText::new(text).size(9.5).color(theme::DANGER));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::CONFIRM_SECONDS;

    /// 二次确认的三段语义。
    ///
    /// 这条守的是**最危险的那个交互**：点一次不该丢世界，点两次才丢，
    /// 超时之后又回到"点一次不丢"。
    #[test]
    fn the_discard_confirm_needs_two_clicks_and_expires() {
        let mut panel = crate::state::WorldPanel::default();
        let new = WorldAction::New;

        // 第一下：不执行，进入待确认。
        assert!(!panel.confirm_click(new.clone(), 100.0));
        assert!(panel.is_confirming(&new, 100.0));

        // 3 秒内第二下：执行。
        assert!(panel.confirm_click(new.clone(), 101.5));
        // 执行完状态清空，不会连点两下再触发一次。
        assert!(!panel.is_confirming(&new, 101.5));
        assert!(!panel.confirm_click(new.clone(), 101.6));
    }

    #[test]
    fn an_expired_confirm_does_not_fire() {
        let mut panel = crate::state::WorldPanel::default();
        let new = WorldAction::New;
        assert!(!panel.confirm_click(new.clone(), 0.0));
        // 超过 3 秒。
        let late = CONFIRM_SECONDS + 0.1;
        assert!(!panel.is_confirming(&new, late));
        panel.expire_confirm(late);
        assert!(panel.confirm.is_none());
        // 再点一次只是重新开始计时，不执行。
        assert!(!panel.confirm_click(new, late));
    }

    /// 先点"新建"再点"读取"，不能被当成"确认新建"。
    ///
    /// 两个不同的动作共用一个确认位，是这类交互最容易写错的地方——
    /// 而写错的后果是**点错按钮丢世界**。
    #[test]
    fn switching_targets_restarts_the_countdown() {
        let mut panel = crate::state::WorldPanel::default();
        assert!(!panel.confirm_click(WorldAction::New, 10.0));
        // 1 秒后改点"读取"：不是确认，而是换成等读取的确认。
        assert!(!panel.confirm_click(WorldAction::Load("a.vivarium".into()), 11.0));
        assert!(!panel.is_confirming(&WorldAction::New, 11.0));
        assert!(panel.is_confirming(&WorldAction::Load("a.vivarium".into()), 11.0));
        // 再点读取：执行。
        assert!(panel.confirm_click(WorldAction::Load("a.vivarium".into()), 12.0));
    }

    /// 种子输入框：空 = 随机，数字 = 那个数，乱打 = 随机。
    #[test]
    fn the_seed_box_is_optional() {
        let mut panel = crate::state::WorldPanel::default();
        assert_eq!(panel.parsed_seed(), None);
        panel.seed = "   ".into();
        assert_eq!(panel.parsed_seed(), None);
        panel.seed = " 20261022 ".into();
        assert_eq!(panel.parsed_seed(), Some(20261022));
        panel.seed = "不是数字".into();
        assert_eq!(panel.parsed_seed(), None, "打错的种子应当退回随机，而不是让新建失败");
    }
}
