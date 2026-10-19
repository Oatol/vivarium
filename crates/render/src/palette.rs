//! 材质 → 视觉的映射表。两个渲染器共用，保证语义一致。
//!
//! 深色调色板（SPEC 第 1 节第 6 项）：近黑背景 + 高饱和材质色。
//! 这类游戏是要长时间盯着看的，所以底色压到近黑，**只有活着的东西是亮的**。
//!
//! 调色板刻意不用渐变、不用辉光——那是后处理层的事。
//! 这里只回答一个问题：这种材质，是什么颜色。

use vivarium_core::MaterialKind;

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
    }
}

/// 材质对应的 ASCII 字符。
///
/// 直接转发 core 的定义而不是在这里另写一份——两处各写一份必然漂移，
/// 而"ASCII 视图与实际材质对不上"是最难排查的一类 bug。
#[inline]
pub const fn glyph(kind: MaterialKind) -> char {
    kind.glyph()
}

/// 有机体占用的格子画成什么。
///
/// 用一个统一的字符而不是按细胞类型分开：ASCII 视图的价值在于"看得见有没有活物"，
/// 而身体内部的分工在 1 字符的分辨率下根本读不出来。想看细胞类型请用像素视图。
pub const CELL_GLYPH: char = '@';

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
}
