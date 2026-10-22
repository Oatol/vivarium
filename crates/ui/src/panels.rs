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

use vivarium_core::{palette, CellType, MaterialKind, TimeScale};

use crate::state::{Command, Level, SelectedOrganism, Snapshot, Tool, UiState};
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
    central
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
                    Tool::Eraser => Icon::Eraser,
                    Tool::Fill => Icon::Fill,
                    Tool::Pick => Icon::Pick,
                    Tool::Inspect => Icon::Inspect,
                };
                let tip = format!("{}（{}）", tool.label(), tool.hotkey());
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
                "生物工坊（G）——在网格上画一副身体",
            )
            .clicked()
            {
                state.workshop.open = !state.workshop.open;
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
                    MaterialKind::Algae,
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
                caption(ui, "细胞");
                for kind in CellType::ALL {
                    let [r, g, b] = palette::cell_rgb(kind);
                    let resp = widgets::swatch(
                        ui,
                        Color32::from_rgb(r, g, b),
                        cell_label(kind),
                        Some(cell_hint(kind)),
                        false,
                    );
                    // 悬停才提示，点击什么也不做——用 `on_hover_text` 已经给了提示，
                    // 这里只要把它标成"非交互"就够了。
                    let _ = resp;
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
            hairline_top(ui);
        });
}

/// 调色板里的小标题。
fn caption(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(text)
            .size(theme::TITLE_SIZE)
            .color(theme::TEXT_FAINT),
    );
    ui.add_space(4.0);
}

fn vsep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(
        egui::vec2(1.0, theme::PALETTE_H - 16.0),
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
        MaterialKind::Algae => "藻",
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

fn cell_hint(kind: CellType) -> &'static str {
    match kind {
        CellType::Mouth => "嘴：吃掉邻格的食物与细胞",
        CellType::Producer => "生产者：把光转成能量，唯一的环境入口",
        CellType::Mover => "移动：每 4 格买 1 格速度",
        CellType::Eye => "眼：每只加 4 格嗅觉半径",
        CellType::Killer => "杀手：咬邻格的别的生物",
        CellType::Armor => "装甲：每格挡掉 8 点伤害",
        CellType::Builder => "构造器：把材质转成结构（未实现）",
    }
}

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
