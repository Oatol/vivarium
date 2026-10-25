//! 退出确认。
//!
//! ## 为什么要有这个
//!
//! Esc 原来是**直接退出程序**的（除非正在放置身体）。而"放置身体 → 想取消
//! → 按 Esc"是一个很自然的连续动作：**手一抖按第二下，程序就没了。**
//! 用户报的"容易误触"就是这件事。
//!
//! 现在 Esc 只负责**打开这个框**，而框里只有两个按钮，且
//! **Esc 永远只取消、永远不确认**——一旦 Esc 能确认，防误触就白做了。
//!
//! ## 为什么窗口右上角的 X 不接确认
//!
//! 点 X 是一个**明确的、只可能有一个意思**的动作，不需要二次确认。
//! 给它加确认不会减少误触，只会让"关不掉"变成新的问题。
//! 这个框服务的是**键盘**，因为键盘上才有"同一个键在不同上下文里
//! 意思不同"这件事。

use crate::state::{Command, UiState};
use crate::theme;
use crate::{widgets, BtnState};
use egui::Ui;

/// 画退出确认框。返回它有没有吃掉这一帧的鼠标（模态）。
pub fn draw(ui: &mut Ui, state: &mut UiState) -> bool {
    if !state.confirm_quit {
        return false;
    }
    let mut open = true;
    let mut answer = None;

    egui::Window::new("vivarium-quit")
        .title_bar(false)
        .collapsible(false)
        .resizable(false)
        .default_width(268.0)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .frame(
            egui::Frame::NONE
                .fill(theme::PANEL)
                .stroke(egui::Stroke::new(1.0, theme::LINE_STRONG))
                .inner_margin(egui::Margin {
                    left: 12,
                    right: 12,
                    top: 10,
                    bottom: 10,
                }),
        )
        .open(&mut open)
        .show(ui.ctx(), |ui| {
            widgets::section(ui, "退出 Vivarium？");
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new("这一局没有存盘，退出之后世界就没了。")
                    .size(9.5)
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                // **「继续」在左边且是默认语气**：误触之后，鼠标最可能落在
                // 的位置上应该是"什么都没发生"。
                if widgets::flat_button(ui, "继续", BtnState::Selected).clicked() {
                    answer = Some(false);
                }
                ui.add_space(6.0);
                if widgets::flat_button(ui, "退出", BtnState::Normal).clicked() {
                    answer = Some(true);
                }
            });
        });

    // 点右上角的 X 等同于「继续」：**关掉这个框 ≠ 退出程序**。
    // 反过来（X = 确认退出）会让"我想关掉这个提示"变成"我退出了"。
    if !open {
        answer = Some(false);
    }

    match answer {
        Some(true) => {
            state.confirm_quit = false;
            state.emit(Command::Quit);
        }
        Some(false) => state.confirm_quit = false,
        None => {}
    }
    // 只要框还在（或者这一帧刚被它处理），就把鼠标吃掉。
    true
}
