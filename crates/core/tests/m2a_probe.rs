//! M2a 的生态测量：**斜向移动和递归状态对生态有什么影响。**
//!
//! M2a 改了三件会改变行为的事：
//!
//! 1. `INPUTS` 17 → 22（4 路递归 + 1 路 `IN_STUCK`）——权重从 300 涨到 370；
//! 2. `pick_direction` 允许**斜向**（主方向 + 够强的次方向）；
//! 3. 撞墙 **fallback**（斜向失败退到两个单向）。
//!
//! 手工基因组的新输入权重全是 0（`Genome::blank()` 的默认），所以
//! "记忆"这一层在**这一代**里不起作用——它要等突变去填。真正立刻生效的
//! 是斜向和 fallback。
//!
//! 跑法：
//!
//! ```text
//! cargo test -p vivarium-core --test m2a_probe --release -- --ignored --nocapture
//! ```

use vivarium_core::life_sim::{stock_surface, LifeSim};
use vivarium_core::{worldgen, MaterialSim, Rng, World};

#[test]
#[ignore = "测量用：release 下跑几分钟，不是回归测试"]
fn what_m2a_did_to_the_ecosystem() {
    let mut world = World::new(512, 512).unwrap();
    let mut rng = Rng::new(11);
    worldgen::sea_and_islands(&mut world, &mut rng);
    let mut sim = LifeSim::new(&mut world);
    let mut mat = MaterialSim::new();
    mat.set_threads(1);

    let ids = stock_surface(&mut world, &mut sim, 16, 300, false);
    println!("投放 {} 条", ids.len());

    let mut total_diagonal = 0u64;
    let mut total_moved = 0u64;
    for t in 0..8000 {
        mat.step(&mut world);
        sim.step(&mut world);
        let st = sim.stats();
        total_diagonal += st.diagonal as u64;
        total_moved += st.moved.iter().sum::<u32>() as u64;
        if (t + 1) % 1000 == 0 {
            println!(
                "  tick {:5}  活 {:4}  最大 {:2} 格  平均 {:.1}  能量 {:8}  斜向 {:6}/{:.1}%  搁浅 {:3}",
                t + 1,
                sim.organisms().len(),
                sim.organisms().iter().map(|o| o.cells.len()).max().unwrap_or(0),
                sim.organisms().iter().map(|o| o.cells.len() as f64).sum::<f64>()
                    / sim.organisms().len().max(1) as f64,
                sim.organisms().iter().map(|o| o.energy as u64).sum::<u64>(),
                total_diagonal,
                if total_moved > 0 {
                    total_diagonal as f64 * 100.0 / total_moved as f64
                } else {
                    0.0
                },
                st.stranded,
            );
        }
    }

    println!(
        "\n汇总：斜向 {total_diagonal} / 总移动 {total_moved}（{:.1}%）",
        if total_moved > 0 {
            total_diagonal as f64 * 100.0 / total_moved as f64
        } else {
            0.0
        }
    );

    // 判据不是"必须多"，而是**必须不为零**。
    // 恒为 0 说明斜向从来没被选中过，"鱼能上岸"就只是个说法。
    assert!(total_diagonal > 0, "斜向一次都没发生——斜向逻辑没接上");
}
