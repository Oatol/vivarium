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
    }
}
