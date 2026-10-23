//! M1 的测量：**大身体活不下来，是"进不了水"还是"能量不够"？**
//!
//! v0.26 之前 `passable()` 里有一条 `MaterialKind::Water => body <= 16`——
//! 17 格以上的身体在水里，水是固体。它出生时挤开的那个空腔之外全是水，
//! 于是它一格也走不动、被重力拉到海底、搁浅、饿死。
//!
//! 拆掉那道墙之后，这一条要回答的问题是：**如果大身体还是活不下来，
//! 那是不是能量账的问题？** 答案决定后面要不要做浮力（M4）。
//!
//! 跑法（只有 release 有意义，debug 跑 8000 tick 要几分钟）：
//!
//! ```text
//! cargo test -p vivarium-core --test big_body_probe --release -- --ignored --nocapture
//! ```
//!
//! ## 实测（512²，种子 11，12 条起步，8000 tick）
//!
//! | 体型 | 存活 | 最大 | 平均 | 能量合计 | 搁浅 | 累计下落 |
//! |---|---|---|---|---|---|---|
//! | 14 格 | 859 | 14 | 11.7 | 1,665,500 | 0 | 2280 |
//! | 18 格 | 873 | 18 | 15.1 | 2,972,540 | 5 | 884 |
//! | 24 格 | 784 | 24 | 19.6 | 3,782,586 | 0 | 783 |
//!
//! **结论一：大身体活不下来，完全是那道墙造成的，不是能量问题。**
//! 拆掉 `body <= 16` 之后，24 格的身体不但活着，种群数量还和 14 格相当
//! （784 vs 859），而**每条的平均能量是大 2.5 倍**（4824 vs 1940）。
//!
//! **结论二：浮力（M4）从"必须"降级成"想要"。** 大身体不需要被"托住"，
//! 它需要的只是"能进水"。气囊仍然值得做（它给"悬停"和"轻甲"开了一条路），
//! 但它不再是"大身体能不能存在"的前提。
//!
//! **结论三（新问题）：体型进化的方向是"越大越好"，不是"多样化"。**
//! 24 格的净收益是正的：12 个生产者 × 14 ≈ 168/tick，
//! 而 upkeep 107 + 移动 36 = 143/tick。**每多一格生产者就多一份收益，
//! 而代价（加性 upkeep）永远小于收益**——因为产能按**格数**算，不按
//! **受光面积**算，堆叠是免费的。
//!
//! 这条要靠 §7 的"产能按暴露表面积算"来解：上方有遮挡就不满产。
//! 在那之前，体型只有一个进化方向，`MAX_BODY_CELLS = 24` 会变成一个
//! 所有生物都顶到的天花板。

use vivarium_core::body::BodyPlan;
use vivarium_core::life::CellType;
use vivarium_core::life_sim::{handcrafted_fish_genome, LifeSim, Tactic};
use vivarium_core::{worldgen, MaterialKind, MaterialSim, Rng, World};

/// 14 格：和现有最大的手工体型（叶背鱼）一样大，作为对照。
const MID14: BodyPlan = BodyPlan {
    name: "14 格",
    cells: &[
        (CellType::Mouth, 0, 0),
        (CellType::Mover, 1, 0),
        (CellType::Mover, 2, 0),
        (CellType::Mover, 3, 0),
        (CellType::Mover, 4, 0),
        (CellType::Mover, 5, 0),
        (CellType::Eye, 6, 0),
        (CellType::Producer, 0, -1),
        (CellType::Producer, 1, -1),
        (CellType::Producer, 2, -1),
        (CellType::Producer, 3, -1),
        (CellType::Producer, 4, -1),
        (CellType::Producer, 5, -1),
        (CellType::Producer, 6, -1),
    ],
};

/// 18 格：比旧上限 16 多两格，正是当年被挡住的那一类。
const BIG18: BodyPlan = BodyPlan {
    name: "18 格",
    cells: &[
        (CellType::Mouth, 0, 0),
        (CellType::Mover, 1, 0),
        (CellType::Mover, 2, 0),
        (CellType::Mover, 3, 0),
        (CellType::Mover, 4, 0),
        (CellType::Mover, 5, 0),
        (CellType::Mover, 6, 0),
        (CellType::Mover, 7, 0),
        (CellType::Mover, 8, 0),
        (CellType::Eye, 9, 0),
        (CellType::Producer, 0, -1),
        (CellType::Producer, 1, -1),
        (CellType::Producer, 2, -1),
        (CellType::Producer, 3, -1),
        (CellType::Producer, 4, -1),
        (CellType::Producer, 5, -1),
        (CellType::Producer, 6, -1),
        (CellType::Producer, 7, -1),
    ],
};

/// 24 格：新上限（`grn::MAX_BODY_CELLS`）。一半生产者一半移动。
const HUGE24: BodyPlan = BodyPlan {
    name: "24 格",
    cells: &[
        (CellType::Mouth, 0, 0),
        (CellType::Mover, 1, 0),
        (CellType::Mover, 2, 0),
        (CellType::Mover, 3, 0),
        (CellType::Mover, 4, 0),
        (CellType::Mover, 5, 0),
        (CellType::Mover, 6, 0),
        (CellType::Mover, 7, 0),
        (CellType::Mover, 8, 0),
        (CellType::Mover, 9, 0),
        (CellType::Mover, 10, 0),
        (CellType::Eye, 11, 0),
        (CellType::Producer, 0, -1),
        (CellType::Producer, 1, -1),
        (CellType::Producer, 2, -1),
        (CellType::Producer, 3, -1),
        (CellType::Producer, 4, -1),
        (CellType::Producer, 5, -1),
        (CellType::Producer, 6, -1),
        (CellType::Producer, 7, -1),
        (CellType::Producer, 8, -1),
        (CellType::Producer, 9, -1),
        (CellType::Producer, 10, -1),
        (CellType::Producer, 11, -1),
    ],
};

/// 结果：一个体型跑完之后的样子。
#[derive(Debug, Default)]
struct Result_ {
    alive: usize,
    max_cells: usize,
    avg_cells: f64,
    total_energy: u64,
    /// 掉到海底搁浅的个数（`stranded` 是"卡住了"的直接证据）。
    stranded: usize,
    /// 累计下落次数。拆墙之后如果这个数很大，说明身体还在往下掉。
    fell: u64,
    /// 能量账。
    injected: u64,
    dissipated: u64,
}

/// 在世界里种下 `n` 条指定的身体，返回编号。
fn plant(world: &mut World, sim: &mut LifeSim, plan: &BodyPlan, n: usize) -> Vec<u32> {
    let (w, h) = (world.width(), world.height());
    let mut ids = Vec::new();
    let mut rng = Rng::new(0xB16_B0D7);
    for i in 0..n {
        // 沿世界宽度均匀撒开，但避开最左最右的边界。
        let x = (w * (i + 1) / (n + 1)).clamp(20, w.saturating_sub(40));
        // 从水面往下找一个能整副身体都放下的位置。
        let mut placed = false;
        for _ in 0..200 {
            let y = rng.gen_range_usize(20, h.saturating_sub(20));
            let ok = plan.cells.iter().all(|&(_, dx, dy)| {
                let (px, py) = (x as isize + dx as isize, y as isize + dy as isize);
                world.get(px, py) == MaterialKind::Water && !world.is_occupied(px, py)
            });
            if !ok {
                continue;
            }
            let org = LifeSim::from_plan(plan, (x as i32, y as i32), handcrafted_fish_genome(Tactic::Ignore), 20_000);
            if let Ok(id) = sim.spawn_injected(world, org) {
                ids.push(id);
                placed = true;
                break;
            }
        }
        if !placed {
            eprintln!("  第 {i} 条放不下（{} 格），跳过", plan.len());
        }
    }
    ids
}

fn run(plan: &BodyPlan, ticks: usize) -> Result_ {
    let mut world = World::new(512, 512).unwrap();
    let mut rng = Rng::new(11);
    worldgen::sea_and_islands(&mut world, &mut rng);
    let mut sim = LifeSim::new(&mut world);
    let mut mat = MaterialSim::new();
    mat.set_threads(1);

    // 藻要开着——不然"吃不到东西"和"能量不够"分不开。
    let planted = plant(&mut world, &mut sim, plan, 12);
    println!(
        "\n=== {} · 种下 {} 条 · {} tick ===",
        plan.name,
        planted.len(),
        ticks
    );

    let mut out = Result_::default();
    for t in 0..ticks {
        mat.step(&mut world);
        sim.step(&mut world);
        if (t + 1) % 1000 == 0 {
            let orgs = sim.organisms();
            let max_cells = orgs.iter().map(|o| o.cells.len()).max().unwrap_or(0);
            let avg = if orgs.is_empty() {
                0.0
            } else {
                orgs.iter().map(|o| o.cells.len() as f64).sum::<f64>() / orgs.len() as f64
            };
            println!(
                "  tick {:5}  活 {:4}  最大 {:2} 格  平均 {:.1} 格  能量合计 {:7}",
                t + 1,
                orgs.len(),
                max_cells,
                avg,
                orgs.iter().map(|o| o.energy as u64).sum::<u64>()
            );
            out.alive = orgs.len();
            out.max_cells = max_cells;
            out.avg_cells = avg;
            out.total_energy = orgs.iter().map(|o| o.energy as u64).sum();
            out.stranded = sim.stats().stranded;
        }
    }
    out.fell = sim.fell_total();
    let ledger = sim.ledger();
    out.injected = ledger.injected;
    out.dissipated = ledger.dissipated;
    out
}

#[test]
#[ignore = "测量用：release 下跑几分钟，不是回归测试"]
fn how_big_a_body_can_actually_live() {
    let small = run(&MID14, 8000);
    let mid = run(&BIG18, 8000);
    let big = run(&HUGE24, 8000);

    println!("\n=== 汇总 ===");
    for (name, r) in [("14 格", &small), ("18 格", &mid), ("24 格", &big)] {
        println!(
            "{name}: 活 {:4} · 最大 {:2} 格 · 平均 {:.1} 格 · 能量 {:7} · 搁浅 {:3} · 累计下落 {:6} · 注入 {:8} 耗散 {:8}",
            r.alive, r.max_cells, r.avg_cells, r.total_energy, r.stranded, r.fell, r.injected, r.dissipated
        );
    }

    // 判据不是"必须活"，而是**要有一个明确的结论**。
    // 如果 18/24 格在 8000 tick 里全部灭绝，而 14 格活着，
    // 那就说明拆墙之后还有第二个原因（能量 / 繁殖门槛），需要继续查。
    assert!(
        mid.alive > 0 || big.alive > 0,
        "18 格和 24 格全灭了：拆墙不是唯一原因，能量账还有问题"
    );
}
