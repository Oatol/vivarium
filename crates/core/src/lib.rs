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

pub mod material;
pub mod rng;
pub mod world;

pub use material::MaterialKind;
pub use rng::Rng;
pub use world::{MaterialCell, World, WorldError, CHUNK_SIZE};
