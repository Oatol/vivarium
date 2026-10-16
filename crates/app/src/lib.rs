//! # vivarium-app
//!
//! 应用层：主循环、输入、时间控制。
//!
//! ## M0 状态：部分实现
//!
//! 时间控制（[`TimeScale`]）与帧预算（[`FrameBudget`]）已经可用，
//! 它们不依赖窗口，可以单独测试。窗口与事件循环随 wgpu 一起接入。

#![allow(dead_code)]

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
    /// 尽可能快，由 [`FrameBudget`] 裁剪。
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
}

/// 每帧的 tick 预算。
///
/// 这是"最大速度"档位的刹车片：不管机器多快，单帧模拟时间不超过 `budget`，
/// 剩下的时间留给渲染和输入，否则界面会卡到无法操作。
#[derive(Debug, Clone, Copy)]
pub struct FrameBudget {
    /// 单帧允许用于模拟的时间上限。
    pub max_sim_time: std::time::Duration,
}

impl Default for FrameBudget {
    fn default() -> Self {
        Self {
            max_sim_time: std::time::Duration::from_millis(8),
        }
    }
}

impl FrameBudget {
    /// 给定档位下，本帧实际应当推进多少 tick。
    ///
    /// `cost_per_tick` 是上一帧测得的单 tick 成本；传 0 时按预算上限返回。
    pub fn plan_ticks(&self, scale: TimeScale, cost_per_tick: std::time::Duration) -> u32 {
        let requested = scale.requested_ticks();
        if requested == u32::MAX {
            if cost_per_tick.is_zero() {
                return 1;
            }
            let affordable =
                (self.max_sim_time.as_nanos() / cost_per_tick.as_nanos().max(1)) as u64;
            affordable.clamp(1, u32::MAX as u64) as u32
        } else {
            requested
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn paused_runs_nothing() {
        let b = FrameBudget::default();
        assert_eq!(b.plan_ticks(TimeScale::Paused, Duration::from_micros(10)), 0);
    }

    #[test]
    fn fixed_scales_ignore_cost() {
        let b = FrameBudget::default();
        assert_eq!(b.plan_ticks(TimeScale::X1, Duration::from_millis(50)), 1);
        assert_eq!(b.plan_ticks(TimeScale::X5, Duration::from_millis(50)), 5);
        assert_eq!(b.plan_ticks(TimeScale::X50, Duration::from_millis(50)), 50);
    }

    #[test]
    fn max_scale_is_capped_by_budget() {
        let b = FrameBudget {
            max_sim_time: Duration::from_millis(8),
        };
        // 单 tick 1 ms ⇒ 8 ms 预算最多跑 8 个 tick。
        assert_eq!(b.plan_ticks(TimeScale::Max, Duration::from_millis(1)), 8);
        // 单 tick 0.1 ms ⇒ 80 个 tick。
        assert_eq!(b.plan_ticks(TimeScale::Max, Duration::from_micros(100)), 80);
    }

    #[test]
    fn max_scale_never_stalls_at_zero() {
        let b = FrameBudget::default();
        // 就算单 tick 比整个预算还贵，也要跑 1 个，否则模拟永远不前进。
        assert_eq!(b.plan_ticks(TimeScale::Max, Duration::from_secs(1)), 1);
        assert_eq!(b.plan_ticks(TimeScale::Max, Duration::ZERO), 1);
    }
}
