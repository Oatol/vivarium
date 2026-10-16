//! # vivarium-ui
//!
//! 界面层：参数面板、生物检查器、统计图表、基因编辑器。
//!
//! ## M0 状态：空壳
//!
//! 目前只有面板状态的数据结构。这样做的理由是先把"要显示什么"钉下来——
//! 界面代码最容易失控的地方不是布局，而是需求漂移。
//!
//! 接入 egui 之后，这里会承载 SPEC 第 6 节的 E 类工具（观测）与 F 类工具（规则编辑）。

#![allow(dead_code)]

/// 面板显示状态。
///
/// 保持成纯数据、不含任何渲染依赖，这样 headless 长跑时也能构造它来做回归测试。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PanelState {
    /// 统计面板：种群 / 物种数 / 平均寿命 / 平均体型曲线。
    pub show_stats: bool,
    /// 生物检查器：选中个体的基因组、能量、谱系。
    pub show_inspector: bool,
    /// 神经网络视图：选中个体的网络结构与当前激活值。
    pub show_neural_view: bool,
    /// 叠加层：能量热力图 / 物种分布图。
    pub overlay: Option<Overlay>,
}

impl PanelState {
    /// 全部关闭的初始状态。
    pub const fn hidden() -> Self {
        Self {
            show_stats: false,
            show_inspector: false,
            show_neural_view: false,
            overlay: None,
        }
    }
}

/// 可切换的世界叠加层。
///
/// 这三个视图不是装饰——它们是**读懂进化**的唯一手段。
/// 没有物种分布图，你只能看到"一堆东西在动"，看不出"谁取代了谁"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// 能量热力图。
    Energy,
    /// 物种分布图（同物种同色系）。
    Species,
    /// 材质活跃度（哪块区域在动，用于定位性能热点）。
    Activity,
}
