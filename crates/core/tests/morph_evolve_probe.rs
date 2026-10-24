//! M2c 的**演化**测量：8000 tick 之后，选择把身体塑造成了什么样。
//!
//! `morph_probe` 量的是"变异能走到哪里"（30 次随机变异，没有选择）。
//! 那个数字只能说明空间有多大，不能说明**演化会不会去那里**。
//! 这个文件补上另一半：真的跑 8000 tick，让繁殖、饿死、被吃去做筛选。
//!
//! 跑法（release，约 3 分钟）：
//!
//! ```text
//! cargo test -p vivarium-core --test morph_evolve_probe --release -- --ignored --nocapture
//! ```
//!
//! ## 看什么
//!
//! | 指标 | 为什么 |
//! |---|---|
//! | 不同表型数 / 不同基因组数 | 简并度。> 5 说明中性网络连通（漂移有地方走） |
//! | `repeat` / `mirror` / `children` 的个体数 | 分节 / 成对器官 / 附肢**在演化里**出没出现 |
//! | 梯度带数 > 0 的个体数 | 位置决定类型这条路有没有被用上 |
//! | 最大代数 | 8000 tick 能跑多少代。代数太少，选择就没有时间起作用 |
//! | 还是手工体型的个体数 | 形态有没有真的被改动过 |

use std::collections::HashSet;

use vivarium_core::body::{BodyPlan, PLANS};
use vivarium_core::life_sim::{stock_surface, LifeSim};
use vivarium_core::morph::{Body, Segment};
use vivarium_core::{worldgen, MaterialSim, Rng, World};

/// 一段（含所有子孙段）的统计。
#[derive(Default, Clone, Copy)]
struct ShapeStats {
    segments: usize,
    max_depth: u8,
    has_repeat: bool,
    has_mirror: bool,
    has_children: bool,
}

fn walk(s: &Segment, depth: u8, st: &mut ShapeStats) {
    st.segments += 1;
    st.max_depth = st.max_depth.max(depth);
    if s.repeat > 1 {
        st.has_repeat = true;
    }
    if s.mirror {
        st.has_mirror = true;
    }
    if !s.children.is_empty() {
        st.has_children = true;
    }
    for c in &s.children {
        walk(c, depth + 1, st);
    }
}

fn shape_of(b: &Body) -> ShapeStats {
    let mut st = ShapeStats::default();
    walk(&b.trunk, 0, &mut st);
    st
}

/// 表型指纹：**只看长成什么样**，不看怎么长出来的。
///
/// 用它算简并度（基因组数 / 表型数）——两个不同的基因组长出一模一样的身体，
/// 就是一次中性差异。
fn phenotype(org: &vivarium_core::life::Organism) -> Vec<(i16, i16, u16)> {
    let mut v: Vec<(i16, i16, u16)> = org
        .cells
        .iter()
        .map(|c| (c.offset.0, c.offset.1, c.organelles.0))
        .collect();
    v.sort_unstable();
    v
}

/// 基因型指纹。用 `Debug` 是因为 `Body` 没实现 `Hash`，而这里只在
/// 每 1000 tick 采一次样，`format!` 的开销可以接受。
fn genotype(b: &Body) -> String {
    format!("{b:?}")
}

fn is_stock(b: &Body) -> bool {
    PLANS.iter().any(|p: &BodyPlan| {
        let mut stock = Body::from_plan(p);
        // `from_plan` 的 `max_cells` 按体型格数算，而变异可能只改了它。
        // 这里要比的是**布局**，不是上限。
        stock.max_cells = b.max_cells;
        *b == stock
    })
}

#[test]
#[ignore = "测量用：release 下跑三分钟，不是回归测试"]
fn what_selection_does_to_bodies() {
    let mut world = World::new(512, 512).unwrap();
    let mut rng = Rng::new(11);
    worldgen::sea_and_islands(&mut world, &mut rng);
    let mut sim = LifeSim::new(&mut world);
    let mut mat = MaterialSim::new();
    mat.set_threads(1);

    let ids = stock_surface(&mut world, &mut sim, 16, 300, true);
    println!("投放 {} 条（形态发生）", ids.len());

    for t in 0..8000 {
        mat.step(&mut world);
        sim.step(&mut world);

        if (t + 1) % 1000 == 0 {
            let orgs = sim.organisms();
            let n = orgs.len().max(1);
            let cells: Vec<usize> = orgs.iter().map(|o| o.cells.len()).collect();
            let avg = cells.iter().sum::<usize>() as f64 / n as f64;
            let max = cells.iter().copied().max().unwrap_or(0);

            let mut phenos = HashSet::new();
            let mut genos = HashSet::new();
            let (mut rep, mut mir, mut chd, mut grad) = (0usize, 0usize, 0usize, 0usize);
            let mut stock = 0usize;
            let mut segs = 0usize;
            let mut depth3 = 0usize;
            for o in orgs {
                phenos.insert(phenotype(o));
                genos.insert(genotype(&o.genome.body));
                let st = shape_of(&o.genome.body);
                if st.has_repeat {
                    rep += 1;
                }
                if st.has_mirror {
                    mir += 1;
                }
                if st.has_children {
                    chd += 1;
                }
                if !o.genome.body.gradient.bands.is_empty() {
                    grad += 1;
                }
                if is_stock(&o.genome.body) {
                    stock += 1;
                }
                segs += st.segments;
                if st.max_depth >= 2 {
                    depth3 += 1;
                }
            }
            let max_gen = orgs.iter().map(|o| o.generation).max().unwrap_or(0);
            let deg = genos.len() as f64 / phenos.len().max(1) as f64;

            println!(
                "tick {:5}  活 {:4}  格 平均 {:.1} 最大 {:2}  代数 {:3}  表型 {:4} 基因组 {:4} 简并 {:.2}",
                t + 1,
                orgs.len(),
                avg,
                max,
                max_gen,
                phenos.len(),
                genos.len(),
                deg
            );
            println!(
                "            分节 {:3}  镜像 {:3}  附肢 {:3}  有梯度 {:3}  深≥2 {:3}  段数均值 {:.2}  原样 {:3}",
                rep,
                mir,
                chd,
                grad,
                depth3,
                segs as f64 / n as f64,
                stock
            );
        }
    }

    // 收尾：把存活个体的身体各打一张图，人工看一遍。
    let orgs = sim.organisms();
    println!("\n================ 8000 tick 之后存活的身体（最多 12 个）================");
    for o in orgs.iter().take(12) {
        let a = vivarium_core::anatomy::interpret(&o.genome);
        println!(
            "[gen {:3} · 能量 {:5} · {}] {}",
            o.generation,
            o.energy,
            o.id,
            a.summary()
        );
        println!("{}", a.ascii());
    }
}
