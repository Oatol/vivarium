//! # vivarium-app
//!
//! 应用层：主循环、输入、时间控制。
//!
//! ## M0 状态：部分实现
//!
//! 时间控制（[`TimeScale`]）与帧预算（[`FrameBudget`]）已经可用，
//! 它们不依赖窗口，可以单独测试。窗口与事件循环随 wgpu 一起接入。

#![allow(dead_code)]

// 速度档住在 `vivarium-core`，这里原样转发。
//
// 挪下去的理由是**依赖方向**：界面层要显示"现在几倍速"，而它只能依赖
// core（不能依赖 app，app 依赖它）。`TimeScale` 又是纯数据、不含任何
// 循环逻辑，所以放进 core 是最省事且不欠债的做法。
//
// `FrameBudget` 留在这一层：它算的是"一帧留多少时间给模拟"，
// 那是应用层的事，和 core 无关。
pub use vivarium_core::TimeScale;

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
        assert_eq!(
            b.plan_ticks(TimeScale::Paused, Duration::from_micros(10)),
            0
        );
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
