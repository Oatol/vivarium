//! 材质 / 细胞 → 颜色的映射表。
//!
//! ## 为什么住在 core，而不是渲染层
//!
//! 一开始它在 `vivarium-render` 里，理由是"只有渲染器需要颜色"。加了界面之后
//! 这个理由就不成立了：底部的材质调色板必须画出**和世界里那一格完全相同**的颜色，
//! 否则"点这个色块会画出什么"要重新学一遍。
//!
//! 而在渲染层各写一份必然漂移——两处 90% 相同的表，改一处忘一处，
//! 症状是"界面上的藻是绿的，世界里的藻偏黄"，而且没人会想到去查调色板。
//!
//! 所以这张表是**语义**（这种材质是什么颜色），不是渲染细节。
//! 真正的渲染细节（sRGB→线性、打包成 uniform 数组、下标偏移）留在渲染层。

use crate::life::CellType;
use crate::material::MaterialKind;
use crate::organelle::{
    Organelles, ARMOR, BUILDER, CHLORO_SHIFT, DIGEST, EYE, FLAGELLUM_SHIFT, GAS, ROOT,
    STING_SHIFT,
};

/// 背景色，同时也是 [`MaterialKind::Empty`] 的颜色。
pub const BACKGROUND: [u8; 3] = [12, 13, 18];

/// 材质对应的 RGB。
#[inline]
pub const fn rgb(kind: MaterialKind) -> [u8; 3] {
    match kind {
        MaterialKind::Empty => BACKGROUND,
        MaterialKind::Stone => [92, 96, 106],
        MaterialKind::Sand => [186, 160, 96],
        MaterialKind::Water => [46, 108, 176],
        MaterialKind::Fire => [222, 108, 48],
        MaterialKind::Gas => [96, 132, 120],
        MaterialKind::Remains => [128, 92, 132],
        // 尸体：比天然碎屑更暖、更亮一点。
        //
        // 这不是装饰——"哪儿死过东西"和"哪儿是海底沉积"是两种完全不同的信息，
        // 而它们的形状、密度、行为都一样。颜色是唯一能区分它们的通道。
        MaterialKind::Corpse => [176, 108, 112],
    }
}

/// 材质对应的 ASCII 字符。
#[inline]
pub const fn glyph(kind: MaterialKind) -> char {
    kind.glyph()
}

/// 有机体占用的格子画成什么。
///
/// 用一个统一的字符而不是按细胞类型分开：ASCII 视图的价值在于"看得见有没有活物"，
/// 而身体内部的分工在 1 字符的分辨率下根本读不出来。想看细胞类型请用像素视图。
pub const CELL_GLYPH: char = '@';

/// 细胞器的基色。**下标就是调色板里的位置**（`CELL_PALETTE_BASE + 下标`）。
///
/// ## 顺序是一份契约
///
/// 这张表同时被三处读：CPU 侧的 [`organelle_rgb`]、渲染层上传的 uniform
/// 数组、以及 `blit.wgsl` 里那段混色。**三处的下标必须一致**——
/// 所以顺序在这里写死，并且有一条测试钉住"每个下标对应的位掩码"。
///
/// ## 为什么线粒体在里面却不参与混色
///
/// 它几乎每格都有（"所有主动功能的前提"），按一份掺进去会把所有颜色
/// 一律拉白：装甲 `#B4C4D0`、根 `#C0B0A2`、气囊 `#D5E2EE`——三个分不出。
/// 所以它只做**标记**：shader 在这一格里点一个白点。
pub const ORGANELLE_RGB: [[u8; 3]; 10] = [
    [86, 200, 96],   // 0 叶绿体：绿。光合作用就是它，绿色是功能本身
    [72, 176, 232],  // 1 鞭毛：青蓝。和水同色系，但亮得多
    [214, 72, 160],  // 2 刺：紫红。危险色，但不刺眼
    [230, 96, 64],   // 3 嘴：橙红。和火同色系但更亮，因为它是主动的
    [128, 152, 176], // 4 装甲：灰蓝。刻意不抢眼
    [200, 220, 240], // 5 气囊：淡蓝白。它是"什么都没有"的器官
    [240, 208, 88],  // 6 眼：黄。像一颗眼点
    [240, 244, 248], // 7 线粒体：白。**不参与混色**，只画点
    [196, 152, 72],  // 8 建造：琥珀。和沙同色系
    [140, 96, 60],   // 9 根：深棕。埋在土里那一半，不该抢眼
];

/// 每个下标对应的"有没有"判据，供测试与 UI 复用。
///
/// 返回值是 `(下标, 位掩码, 是不是有等级的)`。有等级的三个用
/// `level >> shift`，其余用 `has`。
pub const ORGANELLE_SLOTS: [(usize, u16, Option<u32>); 9] = [
    (0, 0b11 << CHLORO_SHIFT, Some(CHLORO_SHIFT)),
    (1, 0b11 << FLAGELLUM_SHIFT, Some(FLAGELLUM_SHIFT)),
    (2, 0b11 << STING_SHIFT, Some(STING_SHIFT)),
    (3, DIGEST, None),
    (4, ARMOR, None),
    (5, GAS, None),
    (6, EYE, None),
    (8, BUILDER, None),
    (9, ROOT, None),
];

/// 细胞里**除了线粒体**之外一共几份"功能"。
///
/// 它就是混色的权重和，同时决定"浓淡"：份数越多颜色越暗（见 [`organelle_rgb`]）。
pub fn organelle_weight(o: Organelles) -> u32 {
    let mut total = 0u32;
    for (_, mask, shift) in ORGANELLE_SLOTS {
        total += match shift {
            Some(sh) => u32::from(o.level(sh)),
            None => u32::from(o.has(mask)),
        };
    }
    total
}

/// 一份"功能"让颜色暗多少。
///
/// 0.10：一格 2 级叶绿体比 1 级暗 10%，2 级叶绿体 + 嘴暗 20%。
/// **这是"等级看得见"的唯一机制**——单看一个细胞器时，加权平均会把
/// 权重约掉（1 级和 2 级混出来一模一样），所以浓度必须单独表达。
const SHADE_PER_EXTRA: f32 = 0.10;

/// 最暗压到多少。
///
/// 0.62：再多器官也不会糊成一团黑。深色背景下"暗"很容易变成"看不见"。
const SHADE_FLOOR: f32 = 0.62;

/// 细胞器 → 颜色。**这是"一格长什么样"的唯一真相。**
///
/// ```text
/// 颜色 = (各细胞器基色的加权平均) × (1 - 0.10 × (份数 - 1))
/// 权重：有等级的三个按等级算（2 级 = 2 份），开关类的算 1 份
/// 线粒体：**不参与**（它是所有细胞的底色，掺进来会把颜色一律拉白）
/// ```
///
/// 于是"生产者 = 绿"、"2 级叶绿体 = 浓绿"、"叶绿体 + 气囊 = 薄荷"、
/// "叶绿体 + 装甲 = 灰绿（硬壳藻）"——**没有人写这些规则，是混色自己算出来的。**
///
/// 只有线粒体的空细胞返回中性灰：它确实什么功能都没有，但必须看得见。
pub fn organelle_rgb(o: Organelles) -> [u8; 3] {
    let mut sum = [0u32; 3];
    let mut total = 0u32;
    for (idx, mask, shift) in ORGANELLE_SLOTS {
        let w = match shift {
            Some(sh) => u32::from(o.level(sh)),
            None => u32::from(o.has(mask)),
        };
        if w == 0 {
            continue;
        }
        let c = ORGANELLE_RGB[idx];
        for k in 0..3 {
            sum[k] += c[k] as u32 * w;
        }
        total += w;
    }
    if total == 0 {
        return [96, 100, 108];
    }
    let shade = (1.0 - SHADE_PER_EXTRA * (total - 1) as f32).max(SHADE_FLOOR);
    let mut out = [0u8; 3];
    for k in 0..3 {
        let v = (sum[k] as f32 / total as f32) * shade;
        out[k] = v.round().clamp(0.0, 255.0) as u8;
    }
    out
}

/// 细胞类型 → 颜色。
///
/// 按**功能**上色，不按个体上色——这一条是照着 ALIEN 的做法定的。
/// 按个体上色时，屏幕上是一堆随机色块，你看不出"这条鱼有两个生产者"；
/// 按功能上色时，一眼就能读出身体的构成，而那正是"细胞结构"的意义。
///
/// 色相拉得很开（绿 / 橙 / 青 / 黄 / 紫 / 灰蓝 / 琥珀），因为深色背景下
/// 相邻色相会糊在一起。饱和度也拉满：活着的东西必须比任何材质都醒目。
/// 它现在只是 [`organelle_rgb`] 的一个特例（单一细胞器、1 份），
/// 保留它是为了调色板和工坊的预设色块——那些地方画的确实是"一种细胞"。
pub fn cell_rgb(kind: CellType) -> [u8; 3] {
    organelle_rgb(Organelles::from_cell_type(kind))
}

/// 有机体编号 → 颜色。
///
/// 用黄金角散布色相（137.5°），相邻编号的颜色差得最开——
/// 这样"两条鱼贴在一起"也能一眼分出是两条，而不是一团同色的方块。
///
/// 亮度拉到满、饱和度压到 0.7：深色调色板下**只有活着的东西是亮的**（SPEC §1），
/// 生物必须比任何材质都醒目。
pub fn organism_rgb(id: u32) -> [u8; 3] {
    let hue = (id.wrapping_mul(137) % 360) as f32;
    hsv_to_rgb(hue, 0.70, 1.0)
}

/// HSV → RGB。`h` 是 0..360 的色相，`s` / `v` 是 0..1。
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    [
        ((r1 + m) * 255.0).round() as u8,
        ((g1 + m) * 255.0).round() as u8,
        ((b1 + m) * 255.0).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_material_has_a_color() {
        for kind in MaterialKind::ALL {
            let c = rgb(kind);
            // 只要不是恰好等于背景色（Empty 除外），就算有可辨识的颜色。
            if kind != MaterialKind::Empty {
                assert_ne!(c, BACKGROUND, "{kind} shares the background color");
            }
        }
    }

    #[test]
    fn glyph_is_never_newline() {
        // ASCII 渲染器靠 '\n' 分行，任何材质字符都不能是它。
        for kind in MaterialKind::ALL {
            assert_ne!(glyph(kind), '\n');
            assert_ne!(glyph(kind), '\r');
        }
        assert_ne!(CELL_GLYPH, '\n');
    }

    #[test]
    fn neighbouring_organisms_get_visibly_different_colours() {
        // 黄金角散布的意义就在这里：id 相邻的两条鱼不能是同一个颜色。
        for id in 1..64u32 {
            let a = organism_rgb(id);
            let b = organism_rgb(id + 1);
            let dist: i32 = (0..3).map(|i| (a[i] as i32 - b[i] as i32).abs()).sum();
            assert!(
                dist > 60,
                "id {id} 与 {} 的颜色太接近：{a:?} vs {b:?}",
                id + 1
            );
        }
    }

    #[test]
    fn organisms_are_brighter_than_the_background() {
        for id in 1..32u32 {
            let c = organism_rgb(id);
            let lum: u32 = c.iter().map(|v| *v as u32).sum();
            let bg: u32 = BACKGROUND.iter().map(|v| *v as u32).sum();
            assert!(lum > bg * 8, "id {id} 不够亮：{c:?}");
        }
    }

    /// **线粒体不参与混色。** 它几乎每格都有，掺进去会把所有颜色拉白——
    /// 装甲 / 根 / 气囊会糊成三个分不出的浅灰。
    ///
    /// 判据：加不加线粒体，颜色必须**完全一样**。
    #[test]
    fn mitochondria_do_not_change_the_color() {
        let plain = Organelles(1 << CHLORO_SHIFT);
        let with_mito = Organelles((1 << CHLORO_SHIFT) | crate::organelle::MITO);
        assert_eq!(organelle_rgb(plain), organelle_rgb(with_mito));
        assert_eq!(organelle_weight(plain), organelle_weight(with_mito));
    }

    /// **等级必须看得见**：2 级叶绿体比 1 级更暗（浓）。
    ///
    /// 单看一个细胞器时，加权平均会把权重约掉——1 级和 2 级混出来
    /// 一模一样。所以浓度由"份数越多越暗"单独表达，这条测试钉的就是它。
    #[test]
    fn a_higher_level_is_darker() {
        let one = Organelles(1 << CHLORO_SHIFT);
        let two = Organelles(2 << CHLORO_SHIFT);
        let lum = |c: [u8; 3]| c.iter().map(|v| *v as u32).sum::<u32>();
        assert!(
            lum(organelle_rgb(two)) < lum(organelle_rgb(one)),
            "2 级叶绿体该更浓：1 级 {:?}、2 级 {:?}",
            organelle_rgb(one),
            organelle_rgb(two)
        );
    }

    /// 组合是**混出来的**，不是查表查出来的。
    ///
    /// "叶绿体 + 气囊"必须落在两者之间（薄荷），而不是等于其中任何一个。
    /// 这条是"能发明新细胞"在颜色上的对应物。
    #[test]
    fn combinations_blend_between_their_parts() {
        let green = organelle_rgb(Organelles(1 << CHLORO_SHIFT));
        let gas = organelle_rgb(Organelles(GAS));
        let mixed = organelle_rgb(Organelles((1 << CHLORO_SHIFT) | GAS));
        for k in 0..3 {
            let lo = green[k].min(gas[k]) as i32;
            let hi = green[k].max(gas[k]) as i32;
            // 混色之后还会按份数压暗，所以只要求"落在两端之间（含压暗余量）"。
            let v = mixed[k] as i32;
            assert!(
                v >= lo - 40 && v <= hi + 4,
                "通道 {k} 混出来是 {v}，两端是 {lo}..{hi}"
            );
        }
        assert_ne!(mixed, green, "混色不能等于其中任何一端");
        assert_ne!(mixed, gas, "混色不能等于其中任何一端");
    }

    /// 每个参与混色的细胞器颜色都得**互相分得开**。
    ///
    /// 判据是两两的欧氏距离，而不是"不相等"：差 3 个色阶的颜色技术上
    /// 不同，画面上是同一个。
    #[test]
    fn organelle_colors_are_far_enough_apart() {
        let idx: Vec<usize> = ORGANELLE_SLOTS.iter().map(|(i, _, _)| *i).collect();
        for (n, &a) in idx.iter().enumerate() {
            for &b in &idx[n + 1..] {
                let (ca, cb) = (ORGANELLE_RGB[a], ORGANELLE_RGB[b]);
                let d: f32 = (0..3)
                    .map(|k| {
                        let x = ca[k] as f32 - cb[k] as f32;
                        x * x
                    })
                    .sum::<f32>()
                    .sqrt();
                assert!(d > 30.0, "细胞器 {a} 和 {b} 的颜色太近：距离 {d:.1}");
            }
        }
    }

    /// 每种细胞都得有一个**和背景分得开**的颜色。
    ///
    /// 判据用亮度差而不是 `!=`：一个和背景只差 3 个色阶的颜色，
    /// 技术上不相等，实际上看不见。
    #[test]
    fn every_cell_type_is_visible_on_the_background() {
        let bg: u32 = BACKGROUND.iter().map(|v| *v as u32).sum();
        for kind in CellType::ALL {
            let c = cell_rgb(kind);
            let lum: u32 = c.iter().map(|v| *v as u32).sum();
            assert!(
                lum > bg * 4,
                "{kind:?} 的颜色 {c:?} 太接近背景，画面上看不见"
            );
        }
    }
}
