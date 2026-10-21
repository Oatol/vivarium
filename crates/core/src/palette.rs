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
        // 藻：暗绿。它是食物，但**不能比生产者细胞更亮**——
        // 活着的东西必须比任何材质都醒目，否则一眼看不出"哪儿有生物"。
        MaterialKind::Algae => [64, 132, 76],
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

/// 细胞类型 → 颜色。
///
/// 按**功能**上色，不按个体上色——这一条是照着 ALIEN 的做法定的。
/// 按个体上色时，屏幕上是一堆随机色块，你看不出"这条鱼有两个生产者"；
/// 按功能上色时，一眼就能读出身体的构成，而那正是"细胞结构"的意义。
///
/// 色相拉得很开（绿 / 橙 / 青 / 黄 / 紫 / 灰蓝 / 琥珀），因为深色背景下
/// 相邻色相会糊在一起。饱和度也拉满：活着的东西必须比任何材质都醒目。
pub const fn cell_rgb(kind: CellType) -> [u8; 3] {
    match kind {
        // 吃：橙红，和火同色系但更亮，因为它是主动的。
        CellType::Mouth => [230, 96, 64],
        // 光合：绿。这是唯一把光变成能量的地方，绿色是它的功能本身。
        CellType::Producer => [86, 200, 96],
        // 运动：青蓝。和水同色系，但亮得多。
        CellType::Mover => [72, 176, 232],
        // 感知：黄。像一颗眼点。
        CellType::Eye => [240, 208, 88],
        // 攻击：紫红。危险色，但不刺眼。
        CellType::Killer => [214, 72, 160],
        // 防御：灰蓝。刻意不抢眼——装甲不该是身体上最亮的部分。
        CellType::Armor => [128, 152, 176],
        // 建造：琥珀。和沙同色系，因为它处理的就是材质。
        CellType::Builder => [196, 152, 72],
    }
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
