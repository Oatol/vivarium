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

/// **海平面以上陆地表面的平均湿度**（`0..=255`）。
///
/// 水循环唯一能直接改善的东西就是这个：雨落在岸上 → 那一带的水格变多 →
/// 湿度场在陆地上抬高 → 根吸水更快。**"雨有没有让陆地变湿"只能看这个数**，
/// 看总水量是看不出来的（水会流回海里，总量几乎不变）。
fn land_surface_moisture(world: &World, sim: &LifeSim) -> f64 {
    let sea = sim.moisture().sea_level();
    let (mut sum, mut n) = (0.0f64, 0u32);
    for x in 0..world.width() as i32 {
        // 这一列最高的非空材质格 = 地表。
        for y in 0..world.height() as i32 {
            if world.get(x as isize, y as isize) == MaterialKind::Empty {
                continue;
            }
            if y < sea {
                sum += sim.moisture().at(x, y) as f64;
                n += 1;
            }
            break;
        }
    }
    if n == 0 {
        0.0
    } else {
        sum / n as f64
    }
}

/// 海平面以上那批水**离海平面有多远**，以及"水上个体"有多少。
///
/// `water_above` 只回答"有多少格"，不回答"在哪儿"。这两个问题差别很大：
///
/// - **贴着海平面几行** = 身体把水面顶起来一点，那是浮力，物理上正确；
/// - **在陆地列上方** = 雨落在山上 / 岸上的水洼，那是**地形**，不是泵；
/// - **在水面列的高处** = 水被搬到半空里悬着，那才是"水泵"。
///
/// 同一个数字（11910 格）可以是三件完全不同的事。`水上 深 67` 这个读数
/// 第一眼像"水被顶到 67 格高的天上"，但它也可能是"山有 67 格高、山上在下雨"。
/// **判据必须能把这两件事分开**，否则修的是不存在的东西。
fn above_sea_shape(world: &World, sim: &LifeSim) -> AboveSea {
    let sea = sim.moisture().sea_level();
    let w = world.width() as isize;
    // 每一列的地形顶：最高的 Stone / Sand 那一行。**不看 `Remains`**——
    // 碎屑是粉末、会被水冲走，把它算进地形会让"岸"随尸体漂移。
    let mut terrain = vec![i32::MAX; w as usize];
    for x in 0..w {
        for y in 0..world.height() as isize {
            if matches!(
                world.get(x, y),
                MaterialKind::Stone | MaterialKind::Sand
            ) {
                terrain[x as usize] = y as i32;
                break;
            }
        }
    }
    let mut out = AboveSea::default();
    for y in 0..sea.max(0) as isize {
        let dy = sea - y as i32;
        for x in 0..w {
            if world.get(x, y) != MaterialKind::Water {
                continue;
            }
            let over_land = terrain[x as usize] < sea;
            if over_land {
                out.on_land += 1;
            } else if dy <= 4 {
                out.film += 1;
            } else {
                out.sky += 1;
                // **悬空水靠什么撑着？** 看它下面那一格是谁。
                //
                // 三个候选，对应三种完全不同的病因：
                // - `身体`：身体对水是墙（`yields_to` 把占位当墙），于是
                //   "水托身体、身体托水"可以互相撑住——一座空中的塔；
                // - `水`：下面是水，说明它是**一整根水柱**的一部分，
                //   那要问"水柱底下是什么"；
                // - `空位`：下面是空的，那它就是**该掉而没掉**——
                //   材质层的问题，不是生命层的。
                let below = y + 1;
                let bs = below;
                if !world.in_bounds(x, bs) {
                    out.sky_on_floor += 1;
                } else if world.is_occupied(x, bs) {
                    out.sky_on_body += 1;
                } else if world.get(x, bs) == MaterialKind::Water {
                    out.sky_on_water += 1;
                } else if world.get(x, bs) == MaterialKind::Empty {
                    out.sky_on_nothing += 1;
                } else {
                    out.sky_on_solid += 1;
                }
                // 侧面：这根"柱子"是不是被身体围出来的。
                let (l, r) = ((x - 1 + w) % w, (x + 1) % w);
                let mut sides_open = 0;
                for sx in [l, r] {
                    if world.is_occupied(sx, y) {
                        out.sky_side_body += 1;
                    } else if matches!(
                        world.get(sx, y),
                        MaterialKind::Stone | MaterialKind::Sand | MaterialKind::Remains
                    ) {
                        // 固体也是墙，但那不是"身体的错"。
                    } else {
                        sides_open += 1;
                    }
                }
                if sides_open >= 1 {
                    out.sky_side_air += 1;
                }
                let band = match dy {
                    0..=4 => 0,
                    5..=16 => 1,
                    17..=32 => 2,
                    33..=64 => 3,
                    _ => 4,
                };
                out.depth_bands[band] += 1;
            }
            out.deepest = out.deepest.max(dy);
        }
    }
    out.orgs = sim
        .organisms()
        .iter()
        .filter(|o| o.footprint().all(|(_, y)| y < sea))
        .count();
    out
}

#[derive(Default)]
struct AboveSea {
    /// 贴着海平面（≤4 行）的水。
    film: u64,
    /// 落在"地形高出海平面"的那些列上方的水 = 山上的雨/水洼。
    on_land: u64,
    /// 剩下的：水面列的高处，也就是真的悬在半空。
    sky: u64,
    /// 悬空水的**下面一格是谁**（四类，加起来等于 `sky`）。
    sky_on_body: u64,
    sky_on_water: u64,
    sky_on_nothing: u64,
    sky_on_solid: u64,
    sky_on_floor: u64,
    /// 悬空水的**左右邻居里有几格是身体**（一个格子最多算 2）。
    ///
    /// 这是"双层模型能不能修好水柱"的判据。水柱的形状是"下面 85% 是水"，
    /// 所以看下面看不出病因；但如果它的**侧面**被身体围住，那它就是一个
    /// "身体当墙的尖峰"——身体不再当墙，它就塌。侧面也是水/空的话，
    /// 那它靠的是别的东西（扫描顺序、横向流动规则），**双层救不了**。
    sky_side_body: u64,
    /// 悬空水里，**至少有一侧是空气**的格数。
    ///
    /// 这批水是"露在空气里"的：它旁边就是空的，只要规则允许就该摊出去。
    /// 而内部格（两侧都是水）不算——那是水的内部，量它没有意义。
    ///
    /// 判据：**露在空气里的悬空水**有多少。如果它接近 `sky`，那这团水
    /// 是一面暴露在空中的墙；如果它很小，那这团水其实是被地形围住的
    /// "抬高的海面"，跟身体一点关系都没有。
    sky_side_air: u64,
    /// 悬空水按"离海平面多远"分桶：1–4 / 5–16 / 17–32 / 33–64 / 65+。
    ///
    /// **这是"平台还是尖刺"的判据。** 平顶的一大团水，每一层都接近满宽，
    /// 分桶会前小后大（绝大部分在 33+）；细柱子则相反，越往上越少。
    /// 两种形状的病因完全不同：前者是"海面被抬高了"，后者才是"被顶上去"。
    depth_bands: [u64; 5],
    deepest: i32,
    orgs: usize,
}

fn run(seed: u64) -> String {
    let mut world = World::new(512, 512).unwrap();
    let mut rng = Rng::new(seed);
    worldgen::sea_and_islands(&mut world, &mut rng);
    let mut sim = LifeSim::new(&mut world);
    // 水循环默认关（见 `LifeSim::water_cycle` 的字段说明），测量里显式打开。
    // `M7_RAIN=0` 做对照。
    let rain = std::env::var("M7_RAIN").map(|v| v != "0").unwrap_or(true);
    sim.set_water_cycle(rain);
    // 陆地繁殖（`can_stand`）。默认关，`M7_LAND=1` 打开做对照。
    let land = std::env::var("M7_LAND").map(|v| v != "0").unwrap_or(false);
    sim.set_land_reproduction(land);
    let mut mat = MaterialSim::new();
    mat.set_threads(1);
    stock_surface(&mut world, &mut sim, 16, 300, false);
    // 水的基线。**必须在 `stock_surface` 之后取**：投放时每个细胞都要借走
    // 它占住的那一格水，提前取会把"借"算成"漏"。
    let water_start = world.count_of(MaterialKind::Water);
    // 累计"吃掉尸体"的格数。这是食腐生态位的**唯一直接证据**：
    // 尸体存量会同时受死亡率、腐烂、材质模拟影响，掉了分不清是谁干的。
    let mut scavenged = 0u64;
    for _ in 0..8000 {
        mat.step(&mut world);
        sim.step(&mut world);
        scavenged += sim.stats().scavenged as u64;
    }

    // **判决实验：这堆水是不想动，还是没被扫描？**
    //
    // 材质层只扫**脏块**（SPEC §2.4 的性能前提）。如果一团水在某个 tick
    // 之后不再有任何格子移动，它所在的块就再也不进扫描队列——哪怕它的
    // 形状在物理上根本站不住。`M7_STIR=1` 每 tick 把所有块标脏，
    // 强制重新扫一遍：水掉下来 = 这是脏块追踪的问题，不是物理的问题。
    let mut stirred = String::new();
    if std::env::var("M7_STIR").map(|v| v != "0").unwrap_or(false) {
        let before = sim.water_above(&world);
        for _ in 0..200 {
            world.mark_all_dirty();
            mat.step(&mut world);
        }
        let after = sim.water_above(&world);
        stirred = format!("  [强制扫描 200 tick：水上 {before} → {after}]");
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
    let shape = above_sea_shape(&world, &sim);
    // 陆地碎屑：`Remains` 在海平面以上有多少格。这是"① 的产物有没有出口"
    // 的唯一直接读数——它在涨就说明陆地会一直变高。
    let sea = sim.moisture().sea_level();
    let mut land_remains = 0u64;
    for y in 0..sea.max(0) as isize {
        for x in 0..world.width() as isize {
            if world.get(x, y) == MaterialKind::Remains {
                land_remains += 1;
            }
        }
    }

    // 海平面以上那一片的**形状**：每 4×4 一格，`~` 有水、`#` 有身体、`.` 全空。
    // 一个数（25171）可以是"平顶的一团"也可以是"一根柱子"，
    // 而这两种形状的病因完全不同——**先看图，再谈因果**。
    let sea = sim.moisture().sea_level();
    let mut map = String::new();
    let want_map = std::env::var("M7_MAP").map(|v| v != "0").unwrap_or(false);
    let mut y = (sea - 116).max(0) as isize;
    while y < sea as isize && want_map {
        map.push_str("  ");
        let mut x = 0;
        while x < world.width() as isize {
            let mut has_water = false;
            let mut has_body = false;
            for dy in 0..4isize {
                for dx in 0..4isize {
                    let (px, py) = (x + dx, y + dy);
                    if !world.in_bounds(px, py) {
                        continue;
                    }
                    if world.get(px, py) == MaterialKind::Water {
                        has_water = true;
                    }
                    if world.is_occupied(px, py) {
                        has_body = true;
                    }
                }
            }
            map.push(if has_body {
                '#'
            } else if has_water {
                '~'
            } else {
                '.'
            });
            x += 4;
        }
        map.push('\n');
        y += 4;
    }

    format!(
        "{map}\nseed {seed:>5}  初始水 {water_start:6}  动物 {:4}  藻 {:3}  物种 {:3}  能量 {:7}  体长 {:2}–{:2} 众数 {:2}  形状 {:2} 种  扁平 {:.2}  脱水 {:2}  最高 y {:3}  尸体 {:5} 格 均 {:3.0}  食腐 {:6}  腐烂 {:6}  总水 {:6}  水上 {:5} [膜 {} 陆 {} 空 {} 深 {:2}：托体 {} 托水 {} 托空 {} 托固 {} | 侧挡体 {} 露空气 {} | 分桶 {}/{}/{}/{}/{}] 水上个体 {:3}  搬水 {:6}  销毁 {:6}  汽化 {:6}  蒸发 {:6}  雨 {:6}  水汽 {:5}  陆湿 {:5.1}  陆屑 {:5}",
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
        st.shallowest,
        corpses,
        corpse_mean,
        scavenged,
        st.corpses_rotted,
        world.count_of(MaterialKind::Water),
        sim.water_above(&world),
        shape.film,
        shape.on_land,
        shape.sky,
        shape.deepest,
        shape.sky_on_body,
        shape.sky_on_water,
        shape.sky_on_nothing,
        shape.sky_on_solid,
        shape.sky_side_body,
        shape.sky_side_air,
        shape.depth_bands[0],
        shape.depth_bands[1],
        shape.depth_bands[2],
        shape.depth_bands[3],
        shape.depth_bands[4],
        shape.orgs,
        sim.water_moved(),
        sim.water_vented(),
        sim.water_vaporized(),
        sim.climate().evaporated(),
        sim.climate().rained(),
        sim.climate().vapor(),
        land_surface_moisture(&world, &sim),
        land_remains,
    ) + &stirred
}

#[test]
#[ignore = "测量用：release 下几分钟，不是回归测试"]
fn three_seeds_of_the_same_world() {
    // 只跑一个种子调试时用 `M7_SEEDS=11`，省掉两次三分钟。
    let seeds: Vec<u64> = std::env::var("M7_SEEDS")
        .ok()
        .map(|s| s.split(',').filter_map(|t| t.trim().parse().ok()).collect())
        .filter(|v: &Vec<u64>| !v.is_empty())
        .unwrap_or_else(|| vec![11, 2026, 7]);
    for seed in seeds {
        println!("{}", run(seed));
    }
}
