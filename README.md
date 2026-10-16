# Vivarium

一个格子世界里的生命沙盘——你铺下材质、放下自己编辑基因的生物，然后松手，看它们自己长成没人设计过的样子。

**当前状态：M0（骨架）** — 世界网格、材质系统、ASCII 渲染器、headless 模拟器已可用；wgpu 像素渲染器待接入。

---

## 这是什么

不是"一个有生命的游戏"，而是一个**会自己长出生命的盒子**。三条参考血统：

| 血统 | 代表作品 | 借什么 |
|---|---|---|
| 上帝沙盒 | WorldBox、The Powder Toy | 画笔式神力、灾害工具 |
| 进化模拟 | LifeEngine、The Bibites | 无设计的自然选择、形态自发演化 |
| 物质物理 | Noita、Evolving-Protozoa、ALIEN | 材质规则表、GRN 形态发生 |

刻意**不含**文明 / 王国 / 历史事件层。

## 快速开始

需要 Rust stable 与 MSVC 工具链（Windows）。

```bash
# 跑一个 headless 模拟并打印 ASCII 视图
cargo run -p vivarium-sim -- --width 256 --height 256 --ticks 1000 --ascii

# 查看全部参数
cargo run -p vivarium-sim -- --help

# 测试
cargo test --workspace
```

`vivarium-sim` 不依赖任何图形库，可以在没有显示器的机器上跑长测。

## 项目结构

```
crates/
  core/    模拟内核：材质、网格分块、确定性随机  ← 零依赖
  render/  Renderer trait + ASCII 渲染器 + wgpu 像素渲染器（占位）
  ui/      面板与检查器（占位）
  app/     主循环、输入、时间控制
bins/
  sim/     headless 模拟器
  game/    图形前端
```

## 两条架构约束

这两条不是风格偏好，是设计决定：

1. **`core` 不依赖任何渲染或窗口库。** 验证进化靠的是"跑几千上万代"，不是"盯着画面看"。
2. **`bins/sim` 刻意不依赖 wgpu / winit / egui。** 一旦它拉进图形依赖，每次编译多等几分钟，长跑调参就废了。

另外，`Renderer::render` 只接收 `&World`——"切换渲染器不改变模拟结果"这条验收断言，靠签名在编译期兜底。

## 可复现性

所有随机走固定种子的 PCG 发生器（手写、零依赖），禁用系统随机。
契约：**同种子 + 同操作序列 ⇒ 同结果**，网格哈希逐 tick 一致。

```bash
cargo run -p vivarium-sim -- --quiet --seed 42 --ticks 1000 | grep hash
```

## 里程碑

- [x] **M0** 骨架：网格、材质、ASCII 渲染、headless 模拟器
- [ ] M1 无生命的世界：材质流动规则
- [ ] M2 第一个生物：能量、代谢、死亡
- [ ] M3 繁殖与突变：无性繁殖 + 结构性突变
- [ ] M4 行为决策层：感知 + 小型神经网络
- [ ] M5 生态平衡 + 观测工具
- [ ] M6 上帝之手：画笔 / 灾害 / 基因编辑器

## 许可

MIT OR Apache-2.0
