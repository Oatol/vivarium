//! 世界生成。
//!
//! ## 这一层为什么在 core 里
//!
//! SPEC 第 2.0 节定下：世界是一张**横板侧视剖面**，有唯一的重力轴（+Y 向下）。
//! 默认地貌是「海洋 + 岛屿」——大片水体，散布几座岛屿与浅滩。
//!
//! 这个决定管着后面的一切：沙往下掉、气往上浮、光自上而下衰减、
//! 生物默认水生（游泳不必对抗重力）。
//!
//! M0 期间这段生成逻辑被复制了三份（`bins/sim`、`bins/game`、
//! `crates/render` 的预览测试），已经开始漂移。收进 core 是为了让它只剩一份。
//!
//! ## 确定性
//!
//! 全部随机走 [`Rng`]，同种子 ⇒ 同一世界。这是 SPEC 可复现性断言的地形入口。

use crate::material::MaterialKind;
use crate::rng::Rng;
use crate::world::World;

/// 天空占世界高度的百分比。水面线在它下面。
///
/// 三成：够给光梯度、气候层和将来的飞行生物留位置，
/// 又不至于让画面上半张全是空的。水体才是主舞台。
const SKY_PERCENT: usize = 30;

/// 海床基准线占世界高度的百分比。它下面是基岩。
///
/// 刻意压在三分之二附近：水深只要一深，岛就得从海床一路堆到水面以上，
/// 画面上会变成几根插在海里的尖塔。浅海让岛能长得宽而低，像岛。
const SEABED_PERCENT: usize = 66;

/// 岛屿数量范围，`[MIN, MAX)`。
///
/// 512 宽的世界里放 3~5 座：再多就会连成一片大陆，
/// 少了又会变成"海上孤零零一座山"。
const ISLAND_COUNT: (usize, usize) = (3, 6);

/// 每多少列放一团残骸。数值越大团块越稀疏。
///
/// 团块本身是透镜状（宽 7~17 格），所以这个密度已经能保证
/// 基岩里到处都有零星的有机沉积，而不是一片纯净的石头。
const REMAINS_CLUMPS_PER_COLUMNS: usize = 6;

/// 放气泡所需的最小水柱高度（格）。
///
/// 一个气泡是 2×2 的方块，覆盖两列、两行。要求水柱至少有这么高，
/// 那一段水里才不会被气泡占掉一大半。
const MIN_BUBBLE_ROOM: usize = 10;

/// 生成结果：地表形状 + 两条基准线。
///
/// 相机取景、统计面板、测试断言都要用它，所以生成函数直接返回，
/// 而不是让调用方自己再扫一遍网格猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldLayout {
    /// 水面线的 y。`y < waterline` 是天空。
    pub waterline: usize,
    /// 海床的**基准**线 y。真实海床在它上下起伏，所以它不是材质分界。
    pub seabed: usize,
    /// 每列的地表 y。`surface[x] < waterline` 表示这一列露出水面。
    pub surface: Vec<isize>,
}

impl WorldLayout {
    /// 露出水面的列数，也就是"陆地宽度"。
    pub fn land_columns(&self) -> usize {
        let wl = self.waterline as isize;
        self.surface.iter().filter(|&&y| y < wl).count()
    }

    /// 最高的地表（最小的 y）。空世界返回水面线。
    pub fn highest(&self) -> isize {
        self.surface
            .iter()
            .copied()
            .min()
            .unwrap_or(self.waterline as isize)
    }

    /// 最高那一列的 x。空世界返回 0。
    pub fn peak_x(&self) -> usize {
        let mut best = 0usize;
        for x in 1..self.surface.len() {
            if self.surface[x] < self.surface[best] {
                best = x;
            }
        }
        best
    }

    /// 相机取景建议：返回视野左沿的 x。
    ///
    /// 对准最高的那座岛。海洋世界里把相机停在开阔水面上，
    /// 屏幕上就只剩一片蓝色——ASCII 视图尤其明显。
    pub fn focus_left(&self, view_width: usize) -> usize {
        self.peak_x().saturating_sub(view_width / 2)
    }

    /// 相机取景建议：返回视野上沿的 y。
    ///
    /// 把视野中心放在「最高岛顶」与「水面线」之间**偏下**的位置，
    /// 这样天空、岛屿轮廓和水体边界能同时入画。
    /// 按世界几何中心取景只会得到一整屏水——水体占了大半张图。
    pub fn focus_top(&self, view_height: usize) -> usize {
        let highest = self.highest().clamp(0, self.waterline as isize) as usize;
        let center = highest + (self.waterline.saturating_sub(highest)) * 3 / 4;
        center.saturating_sub(view_height / 2)
    }
}

/// 按「海洋 + 岛屿」铺一遍初始地貌，返回地表形状。
pub fn sea_and_islands(world: &mut World, rng: &mut Rng) -> WorldLayout {
    let w = world.width();
    let h = world.height();

    let waterline = (h * SKY_PERCENT / 100).clamp(1, h.saturating_sub(1));
    let seabed = (h * SEABED_PERCENT / 100).clamp(waterline + 1, h);
    let depth = seabed - waterline;

    // 海床起伏的幅度。必须随世界高度缩放——固定值在小世界里
    // 相当于水柱的四分之一，会让"岛一定露出水面"这个保证失效。
    let drift_max = (depth / 8).clamp(3, 20);

    // --- 1. 海床起伏：带均值回归的低频漂移 ---
    //
    // 纯随机游走会一路漂走——M0 的地形就踩过这个坑（沙层爬到了画面半空）。
    // 这里让偏离基线越远，回拉的概率越大。
    let mut drift = vec![0isize; w];
    let mut d = 0isize;
    for slot in drift.iter_mut() {
        if d != 0 && rng.chance(0.34) {
            d -= d.signum();
        } else if rng.chance(0.16) {
            d += if rng.chance(0.5) { 1 } else { -1 };
            d = d.clamp(-(drift_max as isize), drift_max as isize);
        }
        *slot = d;
    }

    // --- 2. 岛屿：每座 = 一个宽而低的浅滩 + 一个窄而高的峰 ---
    let islands = rng.gen_range_usize(ISLAND_COUNT.0, ISLAND_COUNT.1);
    // 第一座岛要露出水面多少格。太小会在小世界里被取整吃掉。
    let emerge = (depth / 12).max(3);
    let mut rise = vec![0isize; w];
    for i in 0..islands {
        // 半宽取到世界的 1/12 ~ 1/6。岛要"宽而低"才像岛——
        // 窄而高只会画出一根插在海里的尖塔，因为岛顶要够到水面以上，
        // 高度是被水深定死的，唯一能调的就是宽度。
        let half = rng.gen_range_usize((w / 12).max(4), (w / 6).max(8));
        let cx = rng.gen_range_usize(half, (w - half).max(half + 1));
        let peak = if i == 0 {
            // 第一座岛**保证**露出水面。
            //
            // "海洋 + 岛屿"里的"岛屿"不该是抽奖——早先版本按固定倍率随机，
            // 结果有些种子一张图全是水。另外预览图要靠这座岛
            // 把调色板里的火画出来。
            //
            // 多给 `drift_max` 是因为峰顶所在列的漂移可能恰好向下，
            // 那会把峰顶又拽回水面以下。
            depth + emerge + drift_max
        } else {
            // 峰高必须**超过水深**，否则岛永远露不出水面——这是几何上的必然：
            // 岛顶要高于水面线，而水面线本身就在海床之上 depth 格。
            // 取 0.92~1.24 倍水深：低的一端成为浅滩，高的一端成为岛。
            depth * rng.gen_range_usize(92, 124) / 100
        };
        raise_bump(&mut rise, cx, half, peak);
        // 浅滩：略宽、更低，让岛的基部有个缓坡而不是一根柱子。
        //
        // 刻意收窄（1.25 倍半宽）而不是放宽到 1.5 倍：早先版本里那道浅滩
        // 铺得太开，几座岛的坡脚连成一片，整张图看起来是一座大山
        // 而不是"海里的几座岛"。
        raise_bump(&mut rise, cx, half + half / 4, peak * 25 / 100);
    }

    let surface: Vec<isize> = (0..w)
        .map(|x| seabed as isize + drift[x] - rise[x])
        .collect();

    // --- 3. 铺材质 ---
    //
    // 逐列自地表往下分层：水 → 沙 → 石。
    //
    // 不先铺一层"平坦的基岩再往上盖水"——海床是起伏的，那样会在海床下凹处
    // 把水灌进基岩里，而且残骸会漂在水中间。（两个都真的发生过。）
    let mut rock_top = vec![0usize; w];
    for x in 0..w {
        let top = surface[x].clamp(0, h as isize) as usize;

        // 水体：水面线 → 地表（只有地表在水面之下才有水柱）
        if top > waterline {
            world.fill_rect(
                x as isize,
                waterline as isize,
                1,
                top - waterline,
                MaterialKind::Water,
            );
        }

        // 表层沙
        let sand = rng.gen_range_usize(3, 9).min(h - top);
        if sand > 0 {
            world.fill_rect(x as isize, top as isize, 1, sand, MaterialKind::Sand);
        }

        // 其下全是基岩
        let rock = top + sand;
        rock_top[x] = rock;
        if rock < h {
            world.fill_rect(x as isize, rock as isize, 1, h - rock, MaterialKind::Stone);
        }
    }

    // 气泡：水体里悬着的几团气（M1 之后它们会真的上浮）。
    //
    // 2×2 的方块要**整块**落在水面线之下、两列地表之上。
    // 早先只按本列的 surface 算范围，结果方块的下半行会从相邻列的地表冒出来，
    // 变成"天上飘着气"；只按本列算还会把气泡塞进浅滩边上那几格水洼里。
    for _ in 0..(w / 6) {
        let x = rng.gen_range_usize(0, w.saturating_sub(1).max(1));
        let right = surface.get(x + 1).copied().unwrap_or(h as isize);
        let floor = surface[x].min(right) as usize;
        let ceiling = waterline + 3;
        // 水太浅就别放了——几格深的水里塞一个 2×2 方块，
        // 那一小段水柱会被占掉一大半，看起来像水底在冒泡。
        if floor < ceiling + MIN_BUBBLE_ROOM {
            continue;
        }
        let y = rng.gen_range_usize(ceiling, floor - 2);
        world.fill_rect(x as isize, y as isize, 2, 2, MaterialKind::Gas);
    }

    // 沉积层里的残骸——细胞死亡后的产物，M2 起它会真的出现。
    //
    // 做成**透镜状团块**，而不是逐列单点撒。单点撒出来是一串孤立的珠子，
    // 既不像有机沉积，也让"团块整体沉降"这件事在 M1 里根本看不出来。
    // 透镜是水平拉长的椭圆：宽 7~17 格、厚 3~7 格，埋在海床基岩里。
    //
    // 判定用整数而不是浮点：`(dx/hw)² + (dy/hh)² ≤ 1` 两边同乘 `hw²·hh²`
    // 就只剩整数乘法。世界生成必须是逐位可复现的，能不用浮点就不用。
    for _ in 0..(w / REMAINS_CLUMPS_PER_COLUMNS) {
        let cx = rng.gen_range_usize(0, w);
        // 团块中心从基岩顶往下 4 格起，保证整个团块埋在石头里。
        let lo = rock_top[cx] + 4;
        if lo >= h {
            continue;
        }
        let cy = rng.gen_range_usize(lo, h);
        let hw = rng.gen_range_usize(3, 9) as isize; // 半宽 3~8
        let hh = rng.gen_range_usize(1, 4) as isize; // 半高 1~3
        let (hw2, hh2) = (hw * hw, hh * hh);
        for dy in -hh..=hh {
            for dx in -hw..=hw {
                if dx * dx * hh2 + dy * dy * hw2 > hw2 * hh2 {
                    continue;
                }
                let (px, py) = (cx as isize + dx, cy as isize + dy);
                if !world.in_bounds(px, py) {
                    continue;
                }
                // **只替换基岩**。海床是起伏的，团块边缘会伸到沙层或水里去，
                // 在那里造残骸就等于凭空往水里塞东西。
                if world.get(px, py) == MaterialKind::Stone {
                    world.set(px, py, MaterialKind::Remains);
                    // 天然碎屑带能量（`flags = 25`），这一条有两个作用：
                    // 1. 它们是可吃的食物，生命层建账时会把这份能量计入注入；
                    // 2. `flags != 0` 让材质层的分解规则跳过它们——
                    //    否则没有生物的世界也会一直"腐烂"，M1 的静止契约直接失效。
                    world.set_flags(px, py, 25);
                }
            }
        }
    }

    // 一小撮火，落在最高的岛顶上方的空中。
    // M1 起它会自己蔓延、被水浇灭、十几 tick 后烧完——正好用来确认反应表接对了。
    let mut peak_x = 0usize;
    for x in 1..w {
        if surface[x] < surface[peak_x] {
            peak_x = x;
        }
    }
    if surface[peak_x] < waterline as isize {
        world.fill_rect(
            (peak_x as isize - 3).max(0),
            (surface[peak_x] - 3).max(1),
            6,
            3,
            MaterialKind::Fire,
        );
    }

    WorldLayout {
        waterline,
        seabed,
        surface,
    }
}

/// 岛屿的横向剖面，输入 `t ∈ [-1, 1]`，输出 `0..=1` 的相对高度。
///
/// **平顶 + 陡坡**，而不是纯余弦钟形。纯钟形画出来是一个圆锥，
/// 几座锥体拼在一起看着像山脉，不像岛——真实岛屿是有一块台地的。
/// 中间一半是平的（沙滩 / 台地），两侧用余弦平滑落到零（岸坡）。
fn bump_profile(t: f32) -> f32 {
    const FLAT: f32 = 0.5;
    let a = t.abs();
    if a <= FLAT {
        1.0
    } else {
        let u = (a - FLAT) / (1.0 - FLAT);
        0.5 * (1.0 + (std::f32::consts::PI * u).cos())
    }
}

/// 在 `rise` 上叠加一个岛形隆起，逐列取最大值。
///
/// 用 `max` 而不是累加：两座岛重叠时应当取高的那座，
/// 而不是叠成一座谁也没设计过的尖山。
fn raise_bump(rise: &mut [isize], cx: usize, half: usize, peak: usize) {
    if half == 0 || peak == 0 || rise.is_empty() {
        return;
    }
    let lo = cx.saturating_sub(half);
    let hi = (cx + half).min(rise.len() - 1);
    for (x, slot) in rise.iter_mut().enumerate().take(hi + 1).skip(lo) {
        let t = (x as f32 - cx as f32) / half as f32;
        let r = (bump_profile(t) * peak as f32).round() as isize;
        if r > *slot {
            *slot = r;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 256;
    const H: usize = 256;
    const SEED: u64 = 2026_1016;

    fn build(w: usize, h: usize, seed: u64) -> (World, WorldLayout) {
        let mut world = World::new(w, h).expect("世界尺寸必须是 64 的倍数");
        let mut rng = Rng::new(seed);
        let layout = sea_and_islands(&mut world, &mut rng);
        (world, layout)
    }

    #[test]
    fn same_seed_same_world() {
        let (a, la) = build(W, H, SEED);
        let (b, lb) = build(W, H, SEED);
        assert_eq!(a.hash(), b.hash(), "同种子必须给出同一个世界");
        assert_eq!(la, lb, "地表形状也要一致");
    }

    #[test]
    fn different_seed_different_world() {
        let (a, _) = build(W, H, SEED);
        let (b, _) = build(W, H, SEED + 1);
        assert_ne!(a.hash(), b.hash(), "换种子应当换一张图");
    }

    #[test]
    fn waterline_and_seabed_are_ordered_and_inside_the_world() {
        for (w, h) in [(64, 64), (256, 256), (512, 512)] {
            let (_, l) = build(w, h, SEED);
            assert!(l.waterline < l.seabed, "{w}x{h}: 水面线应当在海床之上");
            assert!(l.seabed < h, "{w}x{h}: 海床线应当在世界之内");
            assert_eq!(l.surface.len(), w, "{w}x{h}: 地表数组应当每列一个");
        }
    }

    #[test]
    fn each_column_is_layered_sky_water_sand_stone() {
        // 这条替代了早先"海床线以下一律是石"的断言。
        // 那条是错的：海床会起伏，所以材质分界跟着地表走，不是一条水平线。
        let (world, l) = build(W, H, SEED);
        for x in 0..W as isize {
            let top = l.surface[x as usize].clamp(0, H as isize) as usize;

            // 地表正上方：海面上是水，陆地上是空天。
            //
            // 陆地允许 Fire——那座演示用的火堆就摆在最高的岛顶上，
            // 是唯一一处刻意放在地表之上的东西。
            if top > 0 {
                let kind = world.get(x, top as isize - 1);
                let ok = if top > l.waterline {
                    kind == MaterialKind::Water
                } else {
                    matches!(kind, MaterialKind::Empty | MaterialKind::Fire)
                };
                assert!(
                    ok,
                    "x={x} 地表正上方是 {kind:?}（地表 y={top}，水面线 {}）",
                    l.waterline
                );
            }

            // 地表那一格是沙
            if top < H {
                assert_eq!(
                    world.get(x, top as isize),
                    MaterialKind::Sand,
                    "x={x} 地表那一格不是沙"
                );
            }

            // 地表往下 12 格比最厚的沙层（8）还深，一定已经进了基岩。
            // 允许 Remains——残骸本来就是埋在沉积层里的。
            if top + 12 < H {
                let kind = world.get(x, (top + 12) as isize);
                assert!(
                    matches!(kind, MaterialKind::Stone | MaterialKind::Remains),
                    "x={x} 地表下 12 格是 {kind:?}，本该是基岩"
                );
            }

            // 最底下一行一定在基岩里
            let bottom = world.get(x, (H - 1) as isize);
            assert!(
                matches!(bottom, MaterialKind::Stone | MaterialKind::Remains),
                "x={x} 世界底部是 {bottom:?}，本该是基岩"
            );
        }
    }

    #[test]
    fn sky_above_the_waterline_is_empty() {
        let (world, l) = build(W, H, SEED);
        for x in 0..W as isize {
            if l.surface[x as usize] > l.waterline as isize {
                assert_eq!(world.get(x, 0), MaterialKind::Empty, "x={x} 顶部不是空天");
            }
        }
    }

    #[test]
    fn water_fills_the_column_below_the_waterline() {
        let (world, l) = build(W, H, SEED);
        let mut checked = 0;
        for x in 0..W as isize {
            let top = l.surface[x as usize];
            if top <= l.waterline as isize + 2 {
                continue;
            }
            // 紧贴水面线下面一定是水——气泡最低也从水面线 +3 起。
            assert_eq!(
                world.get(x, l.waterline as isize + 1),
                MaterialKind::Water,
                "x={x} 水面线正下方不是水"
            );
            // 整根水柱里，非水的格子只能是气泡，而且不能太多。
            //
            // 一列最多被两个气泡盖到（原点只能落在 x-1 或 x），
            // 每个贡献两行，所以上限是 4 格；而水柱至少 MIN_BUBBLE_ROOM+3 格。
            let height = top - l.waterline as isize;
            let mut bubbles = 0isize;
            for y in (l.waterline as isize + 1)..top {
                let k = world.get(x, y);
                assert!(
                    matches!(k, MaterialKind::Water | MaterialKind::Gas),
                    "x={x} y={y} 水柱里混进了 {k:?}"
                );
                if k != MaterialKind::Water {
                    bubbles += 1;
                }
            }
            assert!(
                bubbles * 2 < height,
                "x={x} 水柱只有 {height} 格，气泡却占了 {bubbles} 格"
            );
            checked += 1;
        }
        assert!(checked > W / 2, "大部分列都该有水柱，实际只有 {checked}");
    }

    #[test]
    fn land_columns_are_solid_from_surface_down() {
        let (world, l) = build(W, H, SEED);
        for x in 0..W as isize {
            let top = l.surface[x as usize];
            if top < l.waterline as isize {
                // 露出水面的列：地表往下应当是沙或石，绝不能是水
                let kind = world.get(x, top + 1);
                assert!(
                    matches!(kind, MaterialKind::Sand | MaterialKind::Stone),
                    "x={x} 陆地下方是 {kind:?}，不该是水"
                );
                // 水面线高度上，这一列不该有水
                assert_ne!(
                    world.get(x, l.waterline as isize),
                    MaterialKind::Water,
                    "x={x} 已经露出水面，水面线上不该有水"
                );
            }
        }
    }

    #[test]
    fn every_seed_grows_land_above_the_waterline() {
        // 这是条**硬契约**，不是概率：第一座岛保证露出水面。
        // 早先按固定倍率随机时，有些种子一张图全是水，"海洋 + 岛屿"
        // 名不副实，预览图里连火都画不出来。
        for (w, h) in [(64, 64), (256, 256), (512, 512)] {
            for seed in 0..16u64 {
                let (_, l) = build(w, h, seed);
                assert!(
                    l.land_columns() > 0,
                    "{w}x{h} seed {seed}: 一张图全是水，没有岛露出水面"
                );
            }
        }
    }

    #[test]
    fn demo_world_uses_every_material() {
        // 预览图要能一眼看出调色板有没有配错，所以每种材质都得出现。
        //
        // 例外是**生命制造的**材质：尸体只能由细胞死亡产生，worldgen 永远
        // 不该产出它。用排除列表而不是"只查这几种"——将来新增地貌材质时，
        // 这条测试仍然会抓到"忘了铺进世界"。
        let life_made = [MaterialKind::Corpse];
        let (world, _) = build(512, 512, SEED);
        let census = world.census();
        for kind in MaterialKind::ALL {
            if life_made.contains(&kind) {
                continue;
            }
            assert!(
                census[kind.as_u8() as usize] > 0,
                "{} 在生成的世界里没出现",
                kind.name()
            );
        }
    }

    #[test]
    fn focus_top_puts_the_waterline_in_frame() {
        let (_, l) = build(512, 512, SEED);
        let vh = 40;
        let top = l.focus_top(vh);
        assert!(top <= l.waterline, "视野上沿应当在水面线之上");
        assert!(l.waterline < top + vh, "水面线应当落在视野之内");
    }

    #[test]
    fn focus_left_puts_the_tallest_island_in_frame() {
        let (_, l) = build(512, 512, SEED);
        let vw = 120;
        let left = l.focus_left(vw);
        assert!(
            left <= l.peak_x() && l.peak_x() < left + vw,
            "最高的岛（x={}）应当落在视野 [{left}, {}) 之内",
            l.peak_x(),
            left + vw
        );
    }

    #[test]
    fn peak_x_is_the_highest_column() {
        let (_, l) = build(256, 256, SEED);
        let px = l.peak_x();
        for (x, &y) in l.surface.iter().enumerate() {
            assert!(y >= l.surface[px], "x={x} 比 peak_x={px} 还高");
        }
    }

    #[test]
    fn bump_peaks_at_center_and_fades_to_zero_at_edges() {
        let mut rise = vec![0isize; 101];
        raise_bump(&mut rise, 50, 50, 100);
        assert_eq!(rise[50], 100, "中心应当等于峰高");
        assert_eq!(rise[0], 0, "左边缘应当归零");
        assert_eq!(rise[100], 0, "右边缘应当归零");
        for x in 51..=100 {
            assert!(rise[x] <= rise[x - 1], "x={x} 处不单调递减");
        }
    }

    #[test]
    fn overlapping_bumps_take_the_taller_one() {
        let mut rise = vec![0isize; 101];
        raise_bump(&mut rise, 50, 50, 100);
        raise_bump(&mut rise, 50, 50, 40);
        assert_eq!(rise[50], 100, "重叠时应当取高的那座，而不是累加");
    }
}
