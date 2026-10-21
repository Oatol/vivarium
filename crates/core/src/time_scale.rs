//! 时间控制：一帧跑几个 tick。
//!
//! ## 为什么住在 core
//!
//! 它看起来像"应用层"的东西（循环、帧率、渲染），但实际上是**纯数据**：
//! 一个枚举加一个 `requested_ticks`。放在这里的理由是依赖方向——
//! 界面层要显示"现在几倍速"，而界面只能依赖 core；如果它住在 app，
//! 就会出现 `app → ui → app` 的循环依赖。
//!
//! `FrameBudget`（一帧留多少时间给模拟）仍然在 app：那才是真正
//! 和帧率挂钩的东西。

/// 模拟速度档位。
///
/// SPEC 第 6 节 D 类工具（时间控制）。这类游戏的标配是"模拟速度与帧率解耦"——
/// 允许每帧跑多个 tick，而不是被 60 FPS 锁死。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeScale {
    /// 暂停。
    Paused,
    /// 1 tick / 帧。
    #[default]
    X1,
    /// 5 tick / 帧。
    X5,
    /// 50 tick / 帧。
    X50,
    /// 尽可能快，由 `FrameBudget` 裁剪。
    Max,
}

impl TimeScale {
    /// 本档位请求的 tick 数。`Max` 返回 `u32::MAX`，交由帧预算裁剪。
    pub const fn requested_ticks(self) -> u32 {
        match self {
            Self::Paused => 0,
            Self::X1 => 1,
            Self::X5 => 5,
            Self::X50 => 50,
            Self::Max => u32::MAX,
        }
    }

    /// 是否处于暂停。
    pub const fn is_paused(self) -> bool {
        matches!(self, Self::Paused)
    }

    /// 界面标签。**放在这里而不是界面层**：速度档的语义由 core 定义，
    /// 界面只负责画。哪天加一档 500×，标签跟着枚举一起改，不会漏。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Paused => "暂停",
            Self::X1 => "1×",
            Self::X5 => "5×",
            Self::X50 => "50×",
            Self::Max => "MAX",
        }
    }

    /// 键盘快捷键（1 / 2 / 3 / 4）。
    pub const fn hotkey(self) -> Option<char> {
        match self {
            Self::Paused => None,
            Self::X1 => Some('1'),
            Self::X5 => Some('2'),
            Self::X50 => Some('3'),
            Self::Max => Some('4'),
        }
    }

    /// 除暂停外的四个档位，按从慢到快。
    pub const SPEEDS: [Self; 4] = [Self::X1, Self::X5, Self::X50, Self::Max];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_requests_nothing() {
        assert_eq!(TimeScale::Paused.requested_ticks(), 0);
        assert!(TimeScale::Paused.is_paused());
    }

    /// 每个档位的标签和快捷键都得有，否则界面上会出现一个点不动的按钮。
    #[test]
    fn every_speed_is_labelled_and_reachable() {
        for scale in TimeScale::SPEEDS {
            assert!(!scale.label().is_empty());
            assert!(scale.hotkey().is_some(), "{scale:?} 没有快捷键");
            assert!(!scale.is_paused());
        }
    }
}
