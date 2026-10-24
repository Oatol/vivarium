//! 生物工坊：在网格上画一副身体，然后放进世界或存进生物库。
//!
//! ## 为什么是"画"而不是"调参数"
//!
//! 手工体型是写死在 `body.rs` 里的五个常量。想加第六个就得改代码、重新编译——
//! 那不是"玩"，那是开发。工坊把这件事变成点击：选细胞、涂格子、放置。
//!
//! ## 为什么是模态
//!
//! 打开时它盖住整个窗口，世界暂停响应鼠标。理由很实际：画布本身就是一张网格，
//! 而窗口中央也是网格（世界）。两个网格叠在一起、鼠标同时作用于两边，
//! 结果是"想画身体却顺手在世界里涂了一道水"。
//!
//! ## 界面不做的事
//!
//! 它不知道世界长什么样，也不知道文件在哪。输出的是一条
//! [`crate::Command::PlaceBody`] 或 [`crate::Command::SaveBody`]，
//! 落点和磁盘都在主循环手里。所以这一整块能在没有 GPU、没有文件系统的
//! 机器上测。

use egui::{
    Align, Color32, CornerRadius, FontId, Layout, Rect, Sense, Stroke, StrokeKind, Ui, Vec2,
};

use vivarium_core::organelle::{
    Organelles, ARMOR, BUILDER, CHLORO_SHIFT, DIGEST, EYE, FLAGELLUM_SHIFT, GAS, MITO, STING_SHIFT,
};
use vivarium_core::{morph::MAX_BODY_CELLS, palette, CellType};

use crate::state::{Snapshot, UiState, DRAFT_SIZE};
use crate::theme;
use crate::widgets::{self, BtnState};
use crate::Command;

/// 每格画多大（逻辑像素）。
const CELL: f32 = 20.0;

/// 画布边长（像素）。
const BOARD: f32 = CELL * DRAFT_SIZE as f32;

/// 画一次工坊。返回 `true` 表示它吃掉了这一帧的鼠标。
pub fn draw(ui: &mut Ui, state: &mut UiState, _snap: &Snapshot) -> bool {
    if !state.workshop.open {
        return false;
    }

    // 整窗遮罩：既压暗背后的世界（提示"现在不是在世界里操作"），
    // 也吞掉点击——面板之外的地方点下去不该落到世界上。
    let full = ui.ctx().content_rect();
    let p = ui.painter();
    p.rect_filled(full, CornerRadius::ZERO, Color32::from_black_alpha(200));

    let mut close = false;
    // 用 `Window` 而不是 `Area`：`Area` 的尺寸由内容决定，而这里的内容
    // 既有 `ui.available_width()` 又有 `set_width` —— 两者互相等待，
    // 最后算出 0 宽，**整块面板一个图元都不画**（实测只多了遮罩的 8 个顶点，
    // 而且不报任何错）。`Window` 自带一个初始尺寸和布局约束，不会有这个死结。
    //
    // `title_bar(false)` + 自己的 `Frame`：标题栏的样式（圆角、投影、
    // 关闭按钮位置）和整套 TPT 皮肤冲突，所以只借它的布局，不借它的外观。
    egui::Window::new("vivarium-workshop")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        // **必须给初始宽度。** `Window` 默认按内容自动撑开，而内容里是
        // `horizontal_top` 三列——第一列在"宽度未知"时会一直撑，
        // 把后两列推到 x≈993 之外，clip_rect 宽度变成**负数**，
        // 于是整块右侧面板被裁得一个像素都不剩（实测图元数只多了 8）。
        .default_width(760.0)
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
            ui.set_width(720.0);
            ui.set_max_width(720.0);
            if header(ui, state) {
                close = true;
            }
            ui.add_space(6.0);
            ui.horizontal_top(|ui| {
                cell_picker(ui, state);
                ui.add_space(8.0);
                board(ui, state);
                ui.add_space(8.0);
                side_panel(ui, state);
            });
            ui.add_space(6.0);
            library_strip(ui, state);
        });

    if close {
        state.workshop.open = false;
    }
    true
}

/// 标题行。返回 `true` 表示点了关闭。
fn header(ui: &mut Ui, _state: &mut UiState) -> bool {
    let mut close = false;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("生物工坊")
                .size(theme::TITLE_SIZE + 2.0)
                .color(theme::TEXT),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new("选细胞 → 画身体 → 放置或保存")
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

/// 左侧：细胞类型选择。竖排色块，当前画笔高亮。
fn cell_picker(ui: &mut Ui, state: &mut UiState) {
    ui.vertical(|ui| {
        // 固定宽度：不固定的话这一列会按最长的那行标签撑开，
        // 而标签是中文、宽度随字体变化——右边两列就被挤出去了。
        ui.set_width(62.0);
        widgets::section(ui, "细胞");
        ui.add_space(2.0);
        for kind in CellType::ALL {
            let [r, g, b] = palette::cell_rgb(kind);
            let preset = Organelles::from_cell_type(kind);
            let selected = !state.workshop.erasing && state.workshop.brush == preset;
            if widgets::swatch(
                ui,
                Color32::from_rgb(r, g, b),
                cell_label(kind),
                Some(cell_hint(kind)),
                selected,
            )
            .clicked()
            {
                state.workshop.brush = preset;
                state.workshop.erasing = false;
                state.emit(Command::SetCell(kind));
            }
        }
        ui.add_space(4.0);
        if widgets::flat_button(
            ui,
            "擦除",
            if state.workshop.erasing {
                BtnState::Selected
            } else {
                BtnState::Normal
            },
        )
        .clicked()
        {
            state.workshop.erasing = true;
        }
    });
}

/// 中间：16×16 网格。
fn board(ui: &mut Ui, state: &mut UiState) {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(BOARD), Sense::click_and_drag());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::ZERO, theme::SUNKEN);

    // 网格线。
    for i in 0..=DRAFT_SIZE {
        let t = i as f32 * CELL;
        let line = Stroke::new(1.0, theme::LINE);
        painter.line_segment(
            [
                egui::pos2(rect.left() + t, rect.top()),
                egui::pos2(rect.left() + t, rect.bottom()),
            ],
            line,
        );
        painter.line_segment(
            [
                egui::pos2(rect.left(), rect.top() + t),
                egui::pos2(rect.right(), rect.top() + t),
            ],
            line,
        );
    }

    // 画布中心那一格描一圈：它是锚点。
    //
    // 锚点不是装饰——感知深度、拥挤判定、镜像都以它为准。
    // 画的时候得知道"我这一笔会让身体的中心偏到哪儿"。
    let mid = (DRAFT_SIZE / 2) as f32 * CELL;
    painter.rect_stroke(
        Rect::from_min_size(
            egui::pos2(rect.left() + mid, rect.top() + mid),
            Vec2::splat(CELL),
        ),
        CornerRadius::ZERO,
        Stroke::new(1.0, theme::TEXT_FAINT),
        StrokeKind::Inside,
    );

    // 细胞。
    for y in 0..DRAFT_SIZE {
        for x in 0..DRAFT_SIZE {
            let Some(o) = state.workshop.get(x, y) else {
                continue;
            };
            // 颜色按**代表类型**取：一格「叶绿体 + 气囊」画成绿色，
            // 因为它的主功能是产能。精确功能看右侧的细胞器清单。
            let [r, g, b] = palette::cell_rgb(o.primary_cell_type());
            let cell_rect = Rect::from_min_size(
                egui::pos2(
                    rect.left() + x as f32 * CELL + 1.0,
                    rect.top() + y as f32 * CELL + 1.0,
                ),
                Vec2::splat(CELL - 2.0),
            );
            painter.rect_filled(cell_rect, CornerRadius::ZERO, Color32::from_rgb(r, g, b));
        }
    }

    // 落笔：按住就连续画，所以拖动可以一次涂一片。
    if resp.is_pointer_button_down_on() {
        if let Some(pos) = ui.ctx().pointer_interact_pos() {
            if rect.contains(pos) {
                let x = ((pos.x - rect.left()) / CELL).floor().max(0.0) as usize;
                let y = ((pos.y - rect.top()) / CELL).floor().max(0.0) as usize;
                if x < DRAFT_SIZE && y < DRAFT_SIZE {
                    let v = if state.workshop.erasing {
                        None
                    } else {
                        Some(state.workshop.brush)
                    };
                    if state.workshop.get(x, y) != v {
                        state.workshop.set(x, y, v);
                    }
                }
            }
        }
    }
}

/// 右侧：名字、统计、按钮。
fn side_panel(ui: &mut Ui, state: &mut UiState) {
    ui.vertical(|ui| {
        ui.set_width(200.0);

        // --- 当前画笔的说明 ---
        //
        // 放在最上面而不是藏在 tooltip 里：画身体的时候要反复问
        // "这一格到底干什么用"，而悬停查询会打断手上的动作。
        let brush = state.workshop.brush;
        widgets::section(ui, &organelle_list(brush));
        ui.add_space(1.0);
        ui.label(
            egui::RichText::new(format!(
                "维持 {}/tick · 密度 {:.2} · 效率 {:.0}%",
                brush.upkeep(),
                brush.density(),
                brush.efficiency() * 100.0
            ))
            .size(9.0)
            .color(theme::TEXT_DIM),
        );
        ui.add_space(6.0);

        organelle_editor(ui, state);
        ui.add_space(6.0);

        widgets::section(ui, "名字");
        ui.add_space(2.0);
        ui.add(
            egui::TextEdit::singleline(&mut state.workshop.name)
                .hint_text("例如：三节虫")
                .desired_width(190.0)
                .font(FontId::new(11.0, egui::FontFamily::Proportional)),
        );

        ui.add_space(6.0);
        widgets::section(ui, "这副身体");
        let cells = state.workshop.to_cells();
        let n = cells.len();
        let (movers, eyes, mouths, killers, producers) = count_kinds(&cells);
        let speed = 1 + (movers / 4).min(2);
        widgets::stat_row(ui, "格数", &n.to_string(), theme::TEXT);
        widgets::stat_row(
            ui,
            "速度",
            &format!("{speed} 格/tick"),
            if speed > 1 {
                theme::ACCENT
            } else {
                theme::TEXT_DIM
            },
        );
        widgets::stat_row(
            ui,
            "嗅觉",
            &format!("{} 格", 20 + eyes * 4),
            theme::TEXT_DIM,
        );
        widgets::stat_row(ui, "嘴", &mouths.to_string(), theme::TEXT_DIM);
        widgets::stat_row(ui, "杀手", &killers.to_string(), theme::TEXT_DIM);

        // 合法性：**至少要有一种能量来源**，不是"必须有嘴"。
        //
        // 早先这条判据是 `mouths == 0` 直接拦掉，理由是"没有嘴就吃不了东西"。
        // 但那句话只对了一半：**生产者自己就能产能**，而纯生产者身体
        // （海草）不但能活，实测还是超模的——它省下全部移动开销，
        // 4 格净赚 56.8/tick，自动投放会把光吃光。**这条门挡住的是一个强策略。**
        ui.add_space(4.0);
        if n == 0 {
            hint_line(ui, "画布是空的", theme::TEXT_FAINT);
        } else if mouths == 0 && producers == 0 {
            // 仍然拦：既不能产能也不能吃，放下去只是慢慢饿死。
            // 但话要说准——它饿死的原因是没有能量来源，不是"没有嘴"。
            hint_line(ui, "没有能量来源——它只会慢慢饿死", theme::DANGER);
        } else if n > MAX_BODY_CELLS as usize {
            // 这里原来写的是"超过 16 格，水里推不动"——**那句是假的**。
            // 16 是 `passable()` 里的一条通行性判据（超过 16 格水变成固体），
            // 被 UI 当成了一句物理说明。v0.26 拆掉了那道墙，
            // 现在真正的上限是基因组能长多大：`grn::MAX_BODY_CELLS` = 24。
            hint_line(
                ui,
                &format!("超过 {MAX_BODY_CELLS} 格，超出体型上限"),
                theme::DANGER,
            );
        } else if !has_anchor_neighbour(&cells) {
            hint_line(ui, "身体不连通", theme::WARN);
        } else {
            hint_line(ui, "可以放置", theme::ACCENT);
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if widgets::flat_button(ui, "清空", BtnState::Normal).clicked() {
                state.workshop.clear();
            }
            if widgets::flat_button(ui, "居中", BtnState::Normal).clicked() {
                let cells = state.workshop.to_cells();
                state.workshop.load(&cells);
            }
        });
        ui.add_space(4.0);
        let ready = n > 0 && (mouths > 0 || producers > 0) && n <= MAX_BODY_CELLS as usize;
        if widgets::flat_button(ui, "放置到世界", BtnState::Normal).clicked() && ready {
            state.emit(Command::PlaceBody(cells.clone()));
            state.workshop.open = false;
            state.toast = Some(("点一下世界，把它放下去".into(), 0.0));
        }
        ui.add_space(4.0);
        if widgets::flat_button(ui, "存进生物库", BtnState::Normal).clicked() && ready {
            let name = state.workshop.name.trim().to_owned();
            if name.is_empty() {
                state.workshop.hint = Some(("先起个名字".into(), 0.0));
            } else {
                state.emit(Command::SaveBody {
                    name,
                    cells: cells.clone(),
                });
            }
        }

        if let Some((text, _)) = &state.workshop.hint {
            ui.add_space(4.0);
            hint_line(ui, text, theme::WARN);
        }
    });
}

/// 底部：生物库。
fn library_strip(ui: &mut Ui, state: &mut UiState) {
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new("生物库")
                .size(9.5)
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(4.0);
        if state.library.is_empty() {
            ui.label(
                egui::RichText::new("（空的——画一副身体存进来）")
                    .size(9.5)
                    .color(theme::TEXT_FAINT),
            );
            return;
        }
        let mut delete: Option<String> = None;
        let mut load: Option<Vec<(i16, i16, u16)>> = None;
        for entry in &state.library {
            if widgets::flat_button(
                ui,
                &format!("{}（{}）", entry.name, entry.cells.len()),
                BtnState::Normal,
            )
            .clicked()
            {
                load = Some(entry.cells.clone());
                state.workshop.name = entry.name.clone();
            }
            if widgets::flat_button(ui, "×", BtnState::Normal)
                .on_hover_text("从生物库删掉")
                .clicked()
            {
                delete = Some(entry.name.clone());
            }
            ui.add_space(4.0);
        }
        if let Some(cells) = load {
            state.workshop.load(&cells);
        }
        if let Some(name) = delete {
            state.emit(Command::DeleteBody(name));
        }
    });
}

/// 一行提示文字。
fn hint_line(ui: &mut Ui, text: &str, color: Color32) {
    ui.label(egui::RichText::new(text).size(9.5).color(color));
}

/// 数一数各类细胞。返回 `(移动, 眼, 嘴, 杀手)`。
/// 数一遍各功能有多少**级**（不是多少格）。
///
/// 鞭毛和叶绿体有等级：两格 1 级鞭毛和一格 2 级鞭毛在速度上是同一件事
/// （`speed()` 数的是 `level` 之和），所以这里也数级，统计面板才对得上。
fn count_kinds(cells: &[(i16, i16, u16)]) -> (usize, usize, usize, usize, usize) {
    let (mut movers, mut eyes, mut mouths, mut killers, mut producers) = (0, 0, 0, 0, 0);
    for &(_, _, bits) in cells {
        let o = Organelles(bits);
        movers += o.level(FLAGELLUM_SHIFT) as usize;
        producers += o.level(CHLORO_SHIFT) as usize;
        eyes += usize::from(o.has(EYE));
        mouths += usize::from(o.has(DIGEST));
        killers += usize::from(o.level(STING_SHIFT) > 0);
    }
    (movers, eyes, mouths, killers, producers)
}

/// 把细胞器位掩码翻译成「叶绿体 ×2 + 气囊」这种短标签。
fn organelle_list(o: Organelles) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (shift, name) in [
        (CHLORO_SHIFT, "叶绿体"),
        (FLAGELLUM_SHIFT, "鞭毛"),
        (STING_SHIFT, "刺"),
    ] {
        match o.level(shift) {
            0 => {}
            1 => parts.push(name.to_string()),
            n => parts.push(format!("{name} ×{n}")),
        }
    }
    for (flag, name) in [
        (DIGEST, "嘴"),
        (ARMOR, "装甲"),
        (GAS, "气囊"),
        (EYE, "眼"),
        (MITO, "线粒体"),
        (BUILDER, "建造"),
    ] {
        if o.has(flag) {
            parts.push(name.to_string());
        }
    }
    if parts.is_empty() {
        "空细胞".to_string()
    } else {
        parts.join(" + ")
    }
}

/// 细胞器编辑器：点一下加一级，循环 0→1→2→0。
///
/// 这是「新建一种细胞」的全部实现——`CellType` 是封闭枚举，而功能由
/// `organelles` 决定，所以**能改位掩码就等于能发明新细胞**。
/// 三个有等级的（叶绿体 / 鞭毛 / 刺）循环三档，其余五个是开关。
fn organelle_editor(ui: &mut Ui, state: &mut UiState) {
    widgets::section(ui, "细胞器");
    ui.add_space(1.0);
    ui.label(
        egui::RichText::new("点一下加一级（0→1→2→0）")
            .size(8.5)
            .color(theme::TEXT_FAINT),
    );
    ui.add_space(2.0);
    let mut b = state.workshop.brush;

    // 三行 × 三个：前三个有等级（`level`），后六个是开关。
    // 用「shift / flag 混在一张表里」会要一个枚举，这里直接分成两段写，
    // 读起来比一个 `match idx` 清楚。
    ui.horizontal(|ui| {
        for (shift, name) in [
            (CHLORO_SHIFT, "叶绿体"),
            (FLAGELLUM_SHIFT, "鞭毛"),
            (STING_SHIFT, "刺"),
        ] {
            let lv = b.level(shift);
            let label = if lv == 0 {
                name.to_string()
            } else {
                format!("{name} {lv}")
            };
            let st = if lv > 0 {
                BtnState::Selected
            } else {
                BtnState::Normal
            };
            if widgets::flat_button(ui, &label, st).clicked() {
                b = b.with_level(shift, (lv + 1) % 3);
            }
        }
    });
    ui.horizontal(|ui| {
        for (flag, name) in [(DIGEST, "嘴"), (ARMOR, "装甲"), (GAS, "气囊")] {
            let on = b.has(flag);
            let st = if on {
                BtnState::Selected
            } else {
                BtnState::Normal
            };
            if widgets::flat_button(ui, name, st).clicked() {
                b = Organelles(b.0 ^ flag);
            }
        }
    });
    ui.horizontal(|ui| {
        for (flag, name) in [(EYE, "眼"), (MITO, "线粒体"), (BUILDER, "建造")] {
            let on = b.has(flag);
            let st = if on {
                BtnState::Selected
            } else {
                BtnState::Normal
            };
            if widgets::flat_button(ui, name, st).clicked() {
                b = Organelles(b.0 ^ flag);
            }
        }
    });

    state.workshop.brush = b;
}

/// 身体是不是连成一片（四邻连通，从任意一格出发能走到所有格）。
///
/// 断开的身体在世界上会变成两团各自为政的细胞，却只有一个锚点——
/// 移动时一半跟着走、另一半留在原地，然后在下一 tick 被当成"别人的身体"。
/// 那是很难从画面上看出原因的故障，所以在工坊里就拦下来。
fn has_anchor_neighbour(cells: &[(i16, i16, u16)]) -> bool {
    if cells.len() <= 1 {
        return true;
    }
    let mut seen = vec![false; cells.len()];
    let mut stack = vec![0usize];
    seen[0] = true;
    let mut reached = 1usize;
    while let Some(i) = stack.pop() {
        let (x, y, _) = cells[i];
        for (dx, dy) in [(1i16, 0i16), (-1, 0), (0, 1), (0, -1)] {
            for (j, &(nx, ny, _)) in cells.iter().enumerate() {
                if !seen[j] && nx == x + dx && ny == y + dy {
                    seen[j] = true;
                    reached += 1;
                    stack.push(j);
                }
            }
        }
    }
    reached == cells.len()
}

/// 细胞的中文名。和底部调色板一致。
fn cell_label(kind: CellType) -> &'static str {
    match kind {
        CellType::Mouth => "嘴",
        CellType::Producer => "生产者",
        CellType::Mover => "移动",
        CellType::Eye => "眼",
        CellType::Killer => "杀手",
        CellType::Armor => "装甲",
        CellType::Builder => "建造",
    }
}

/// 悬停提示：这个细胞在收支上是什么。
fn cell_hint(kind: CellType) -> &'static str {
    match kind {
        CellType::Mouth => "吃东西。没有嘴就吃不了任何东西。",
        CellType::Producer => "在浅水里按光照产能。主要的收入来源。",
        CellType::Mover => "每 4 格让每 tick 多走一格。跑得快更烧钱。",
        CellType::Eye => "每 1 只让嗅觉射线长 4 格。",
        CellType::Killer => "咬邻格的别的生物，能量从对方身上转移过来。",
        CellType::Armor => "每格挡掉 8 点咬击伤害。",
        CellType::Builder => "预留：修复相邻细胞。现在只是配重。",
    }
}
