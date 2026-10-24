//! M1.5 的基线测量：**现在的基因组能长出什么，以及改一个基因动多少格。**
//!
//! 这三组数字是判据 J1/J2/J3 的"现在"那一列。没有基线，"改进了"就无从谈起。
//!
//! 跑法：
//!
//! ```text
//! cargo test -p vivarium-core --test morph_probe --release -- --ignored --nocapture
//! ```

use std::collections::HashSet;

use vivarium_core::anatomy::{interpret, sensitivity};
use vivarium_core::body::PLANS;
use vivarium_core::morph::Body;
use vivarium_core::life::Genome;
use vivarium_core::rng::Rng;

fn genome_of(plan: &vivarium_core::body::BodyPlan) -> Genome {
    let mut g = Genome::blank();
    g.body = Body::from_plan(plan);
    g
}

/// 从银鱼出发，变异 `edits` 次。
fn mutated(edits: usize, seed: u64) -> Genome {
    let mut rng = Rng::new(seed);
    let mut g = genome_of(&vivarium_core::body::SILVERFISH);
    for _ in 0..edits {
        g.body.mutate(&mut rng, 1);
    }
    g
}

#[test]
#[ignore = "测量用：打印基线报告，不是回归测试"]
fn baseline_of_the_current_grn() {
    println!("\n================ 手工体型 ================");
    for plan in PLANS {
        let a = interpret(&genome_of(plan));
        print!("{}", a.report());
        println!();
    }

    println!("\n================ 变异 5 次之后（20 个）================");
    for seed in 0..20u64 {
        let a = interpret(&mutated(5, seed * 7919 + 1));
        println!("[seed {seed:2}] {}", a.summary());
        for line in a.ascii().lines() {
            println!("           {line}");
        }
    }

    println!("\n================ 判据基线 ================");

    // J2：模块化。改一个基因位点，平均影响多少比例的身体格。
    let g = genome_of(&vivarium_core::body::SILVERFISH);
    let s = sensitivity(&g, &mut Rng::new(11), 6, 40);
    println!("J2 模块化（银鱼）：{}", s.summary());
    let mut worst = s.per_slot.clone();
    worst.sort_by(|a, b| b.reach.partial_cmp(&a.reach).unwrap());
    for slot in worst.iter().take(5) {
        println!("   最坏：{:<24} {:5.0}%", slot.name, slot.reach * 100.0);
    }
    let mut best = s.per_slot.clone();
    best.sort_by(|a, b| a.reach.partial_cmp(&b.reach).unwrap());
    for slot in best.iter().take(3) {
        println!("   最好：{:<24} {:5.0}%", slot.name, slot.reach * 100.0);
    }

    // J3：简并度 = 不同基因组数 / 不同表型数。
    // 100 个"各变异 1 次"的基因组，看它们产出几种不同的身体。
    let mut shapes: HashSet<Vec<(i16, i16, u8)>> = HashSet::new();
    let mut genomes = 0usize;
    for seed in 0..100u64 {
        let g = mutated(1, seed * 104_729 + 3);
        let a = interpret(&g);
        let mut sig: Vec<(i16, i16, u8)> =
            a.cells.iter().map(|c| (c.offset.0, c.offset.1, c.kind.as_u8())).collect();
        sig.sort_unstable();
        shapes.insert(sig);
        genomes += 1;
    }
    let degen = genomes as f64 / shapes.len().max(1) as f64;
    println!(
        "J3 简并度：{genomes} 个基因组 → {} 种表型 → 简并度 {:.2}（目标 > 5）",
        shapes.len(),
        degen
    );

    // J1：PCA 低维性。这里只报"表型矩阵的秩"，完整的 PCA 要等 M2b 之后。
    println!("J1 PCA 低维性：待 M2b 之后实现（需要批量表型矩阵）");

    assert!(degen > 0.0, "简并度算出来是 0");
}
