//! # vivarium-core
//!
//! Vivarium 的模拟内核。
//!
//! ## 唯一的硬约束
//!
//! **这一层不依赖任何渲染或窗口库。**
//!
//! 理由写在 SPEC 里：验证进化靠的是"跑几千上万代"，而不是"盯着画面看"。
//! 把 core 与 render 彻底分离之后，`bins/sim` 可以在没有 GPU、没有窗口、
//! 甚至没有显示器的机器上把一万代跑完——这是调参和回归测试的主力工具。
//!
//! ## 确定性
//!
//! 所有随机都走 [`rng::Rng`]（固定种子），**禁用系统随机**。
//! 契约：同种子 + 同操作序列 ⇒ 同结果，且网格哈希逐 tick 一致。
//!
//! ## 世界的形状
//!
//! 世界是一张**横板侧视剖面**，有唯一的重力轴（+Y 向下）：
//! 沙往下掉、气往上浮、光自上而下衰减。默认地貌是「海洋 + 岛屿」，
//! 由 [`worldgen::sea_and_islands`] 生成。这条决定见 SPEC 第 2.0 节——
//! 它管着材质规则、并行扫描顺序和生物的运动方式。

pub mod body;
pub mod grn;
pub mod life;
pub use body::{BodyPlan, PLANS};
pub use grn::Grn;
pub use life::CellType;
pub mod life_sim;
pub mod material;
pub mod rng;
pub mod sim;
pub mod world;
pub mod worldgen;

pub use material::{
    ignite_chance, react, MaterialKind, MoveRule, Reaction, FIRE_LIFETIME, IGNITE_BASE_PERCENT,
    REACTIONS,
};
pub use rng::Rng;
pub use sim::{MaterialSim, MovedMask};
pub use world::{CellOwner, MaterialCell, World, WorldError, CHUNK_SIZE};
pub use worldgen::{sea_and_islands, WorldLayout};
