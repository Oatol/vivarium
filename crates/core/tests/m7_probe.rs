//! 生态的多种子测量：**一个规则改动到底有没有一致的方向。**
//!
//! 为什么不能只看一个种子：生态是混沌的。把产量削掉 10% 之后，同一个世界
//! 会走到**另一个吸引子**——单种子对比里"生物数从 798 掉到 726"这种差异
//! 分不清是规则的效果还是混沌，所以这里跑三个种子取方向。
//!
//! 判据（按重要性排）：
//!
//! 1. **物种数**（`LifeStats::species`，按体型指纹去重）——"多样性"本身；
//! 2. **体型分布**——是不是从一个尖峰变成一条尾巴；
//! 3. **扁平度**（包围盒宽 / 高）——自遮挡的预期是把它推大（身体变薄）；
//! 4. **动物数 / 能量**——生态有没有被这条规则搞崩。
//!
//! 跑法（release，三个种子约 2 分钟）：
//!
//! ```text
//! cargo test -p vivarium-core --test m7_probe --release -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;

use vivarium_core::life_sim::{stock_surface, LifeSim};
use vivarium_core::{worldgen, MaterialKind, MaterialSim, Rng, World};

/// 平均"扁平度"：包围盒的宽 / 高。
fn flatness(sim: &LifeSim) -> f64 {
    let (mut sum, mut n) = (0.0f64, 0u32);
    for o in sim.organisms() {
        if o.cell_count() < 2 {
            continue;
        }
        let (mut x0, mut x1, mut y0, mut y1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
        for (x, y) in o.footprint() {
            x0 = x0.min(x);
            x1 = x1.max(x);
            y0 = y0.min(y);
            y1 = y1.max(y);
        }
        let w = (x1 - x0 + 1).max(1) as f64;
        let h = (y1 - y0 + 1).max(1) as f64;
        sum += w / h;
        n += 1;
    }
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

fn run(seed: u64) -> String {
    let mut world = World::new(512, 512).unwrap();
    let mut rng = Rng::new(seed);
    worldgen::sea_and_islands(&mut world, &mut rng);
    let mut sim = LifeSim::new(&mut world);
    let mut mat = MaterialSim::new();
    mat.set_threads(1);
    stock_surface(&mut world, &mut sim, 16, 300, false);
    // 累计"吃掉尸体"的格数。这是食腐生态位的**唯一直接证据**：
    // 尸体存量会同时受死亡率、腐烂、材质模拟影响，掉了分不清是谁干的。
    let mut scavenged = 0u64;
    for _ in 0..8000 {
        mat.step(&mut world);
        sim.step(&mut world);
        scavenged += sim.stats().scavenged as u64;
    }

    let st = sim.stats();
    let mut hist: BTreeMap<usize, u32> = BTreeMap::new();
    for o in sim.organisms() {
        if o.cell_count() > 1 {
            *hist.entry(o.cell_count()).or_insert(0) += 1;
        }
    }
    // 众数：分布是尖峰还是长尾，看它比"最大值"清楚。
    let mode = hist
        .iter()
        .max_by_key(|(_, n)| **n)
        .map(|(k, _)| *k)
        .unwrap_or(0);
    let distinct_shapes = hist.len();

    // **尸体存量**：食腐者存在的唯一可观测证据。
    //
    // 判据不是"有没有生物在吃"（那要抓现行），而是**同时存在的尸体有多少**。
    // 没人吃的时候尸体只会慢慢烂掉，于是存量高、平均能量高、腐烂计数高；
    // 一旦有食腐者，这三条会一起掉。v0.35 之前这个数没有意义（尸体平均
    // 5 点能量，谁来吃都亏），所以它只从 v0.39 开始有解释力。
    let (mut corpses, mut corpse_energy) = (0u64, 0u64);
    for y in 0..512isize {
        for x in 0..512isize {
            if world.get(x, y) == MaterialKind::Corpse {
                corpses += 1;
                corpse_energy += world.flags(x, y) as u64;
            }
        }
    }
    let corpse_mean = if corpses == 0 {
        0.0
    } else {
        corpse_energy as f64 / corpses as f64
    };

    format!(
        "seed {seed:>5}  动物 {:4}  藻 {:3}  物种 {:3}  能量 {:7}  体长 {:2}–{:2} 众数 {:2}  形状 {:2} 种  扁平 {:.2}  脱水 {:2}  尸体 {:5} 格 均 {:3.0}  食腐 {:6}  腐烂 {:6}",
        st.organisms,
        st.plankton,
        st.species,
        st.energy,
        st.min_cells,
        st.max_cells,
        mode,
        distinct_shapes,
        flatness(&sim),
        st.stranded,
        corpses,
        corpse_mean,
        scavenged,
        st.corpses_rotted,
    )
}

#[test]
#[ignore = "测量用：release 下几分钟，不是回归测试"]
fn three_seeds_of_the_same_world() {
    for seed in [11u64, 2026, 7] {
        println!("{}", run(seed));
    }
}
