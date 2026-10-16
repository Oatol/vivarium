# Vivarium

一个格子世界里的生命沙盘——你铺下材质、放下自己编辑基因的生物，然后松手，看它们自己长成没人设计过的样子。

**当前状态：M0 完成** — 世界网格、材质系统、ASCII 渲染器、headless 模拟器、wgpu 像素渲染器与窗口前端均已可用。

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
# 图形前端：开窗，看像素画面
cargo run --release -p vivarium-game

# 没有 GPU 或没有显示器时，在终端里跑同一套内核
cargo run -p vivarium-game -- --ascii

# headless 模拟并打印 ASCII 视图
cargo run -p vivarium-sim -- --width 256 --height 256 --ticks 1000 --ascii

# 查看全部参数
cargo run -p vivarium-game -- --help

# 测试
cargo test --workspace
```

窗口里的操作：`空格` 暂停/继续 · `1/2/3/4` 速度 · `R` 重铺地形 · `Esc` 退出。

`vivarium-sim` 不依赖任何图形库，可以在没有显示器的机器上跑长测。

### 想看一眼渲染结果

不用开窗口，直接把离屏渲染导成 PNG：

```bash
cargo test -p vivarium-render --lib -- --ignored --nocapture render_preview_png
# → target/render-preview.png
```

像素渲染的 bug（颜色偏了、上下颠倒、留边错位）几乎都是"看一眼就知道、
但很难写好断言"的那一类。这张图能一次覆盖全部像素。

## 项目结构

```
crates/
  core/    模拟内核：材质、网格分块、确定性随机  ← 零依赖
  render/  Renderer trait + ASCII 渲染器 + wgpu 像素渲染器
  ui/      面板与检查器（占位）
  app/     主循环、输入、时间控制
bins/
  sim/     headless 模拟器
  game/    图形前端（winit 事件循环）
```

## 三条架构约束

这几条不是风格偏好，是设计决定：

1. **`core` 不依赖任何渲染或窗口库。** 验证进化靠的是"跑几千上万代"，不是"盯着画面看"。
2. **`bins/sim` 刻意不依赖 wgpu / winit / egui。** 一旦它拉进图形依赖，每次编译多等几分钟，长跑调参就废了。
3. **`render` 不依赖 winit。** 它只接受一个已经建好的 `wgpu::Surface`——窗口是应用层的事，画像素不是。这样 headless 渲染不必拖进一整套窗口系统。

另外，`Renderer::render` 只接收 `&World`——"切换渲染器不改变模拟结果"这条验收断言，靠签名在编译期兜底。

## 渲染：脏块增量上传

世界纹理与世界同尺寸（512² 约 1 MiB），但**每帧只重传变过的 64×64 块**。

带宽其实不是瓶颈（1 MiB × 60 FPS = 60 MiB/s，GPU 不在乎）；
真正的成本是 CPU 侧的调色板转换——262144 个格子逐个查表、逐字节写入。
分块之后，静止的块连查表都省了。

这与 SPEC 第 2.4 节一致：落沙层的成本取决于**活跃格子比例**，不是网格大小。

按 `R` 重铺地形时，窗口标题里的"本帧上传"会瞬间跳到满格再掉回 0——
那就是增量逻辑在工作的直接证据。

## 可复现性

所有随机走固定种子的 PCG 发生器（手写、零依赖），禁用系统随机。
契约：**同种子 + 同操作序列 ⇒ 同结果**，网格哈希逐 tick 一致。

```bash
cargo run -p vivarium-sim -- --quiet --seed 42 --ticks 1000 | grep hash
```

## 里程碑

- [x] **M0** 骨架：网格、材质、ASCII 渲染、headless 模拟器、wgpu 像素渲染器
- [ ] M1 无生命的世界：材质流动规则
- [ ] M2 第一个生物：能量、代谢、死亡
- [ ] M3 繁殖与突变：无性繁殖 + 结构性突变
- [ ] M4 行为决策层：感知 + 小型神经网络
- [ ] M5 生态平衡 + 观测工具
- [ ] M6 上帝之手：画笔 / 灾害 / 基因编辑器

## 许可

MIT OR Apache-2.0
