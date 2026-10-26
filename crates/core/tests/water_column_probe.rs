//! 属性测试：**一根孤立水柱会自己塌。**
//!
//! 这条在 v0.54 那次"要不要改成双层模型"的讨论里写下的。当时的现象是
//! "海平面以上悬着两万五千格水"，而"身体当墙"只能解释其中 7.7% 的侧壁
//! ——所以得先确认材质层自己有没有找平能力。
//!
//! 现实世界里的"悬空水"98.5% 都露在空气里（两侧至少一侧是空的），
//! 却一动不动。那要么是材质层根本不会找平，要么是有别的东西挡着。
//! 这个文件把"别的东西"全部去掉，只留一根柱子和一块地板。

use vivarium_core::{MaterialKind, MaterialSim, World};

fn column(w: usize, h: usize, floor_y: usize, col_x: usize, top_y: usize) -> World {
    let mut world = World::new(w, h).unwrap();
    for x in 0..w {
        world.set(x as isize, floor_y as isize, MaterialKind::Stone);
    }
    for y in top_y..floor_y {
        world.set(col_x as isize, y as isize, MaterialKind::Water);
    }
    world
}

#[test]
fn a_lone_water_column_levels_out() {
    let mut world = column(64, 64, 40, 32, 20);
    let before = world.count_of(MaterialKind::Water);
    let mut mat = MaterialSim::new();
    mat.set_threads(1);
    for _ in 0..400 {
        mat.step(&mut world);
    }
    let after = world.count_of(MaterialKind::Water);
    // 最高的一格水在哪儿（越小越高）。
    let mut top = 64;
    for x in 0..64 {
        for y in 0..40 {
            if world.get(x, y) == MaterialKind::Water {
                top = top.min(y);
                break;
            }
        }
    }
    println!(
        "水 {before} → {after}，最高水行 {top}（初始 20，地板 40；找平后应当是 39 附近）"
    );
    assert_eq!(before, after, "水总量不该变");
    assert!(
        top >= 35,
        "20 格高的水柱应当摊平到地板附近（≤5 行厚），实测最高还在第 {top} 行"
    );
}
