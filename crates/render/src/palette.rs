//! 材质 → 视觉的映射表。两个渲染器共用，保证语义一致。
//!
//! 深色调色板（SPEC 第 1 节第 6 项）：近黑背景 + 高饱和材质色。
//! 这类游戏是要长时间盯着看的，所以底色压到近黑，**只有活着的东西是亮的**。
//!
//! 调色板刻意不用渐变、不用辉光——那是后处理层的事。
//! 这里只回答一个问题：这种材质，是什么颜色。

use vivarium_core::{CellType, MaterialKind};

/// 细胞类型在调色板里的起始下标。**必须与 `blit.wgsl` 里的同名常数一致。**
pub const CELL_PALETTE_BASE: usize = 16;

/// 调色板长度。材质占 `[0, CELL_PALETTE_BASE)`，细胞占 `[16, 23)`。
pub const PALETTE_LEN: usize = 24;

// 下标够不够用是**编译期**的事，不该等到跑测试才发现。
const _: () = assert!(MaterialKind::COUNT <= CELL_PALETTE_BASE);
const _: () = assert!(CELL_PALETTE_BASE + CellType::COUNT <= PALETTE_LEN);

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

/// sRGB 分量 → 线性。
///
/// 着色器输出的是**线性**颜色，硬件负责编码成 sRGB 写进交换链。
/// 直接把 0..255 除以 255 递过去，画面会明显偏亮——这是 wgpu 里
/// 最经典的一次"双重转换"。
fn srgb_to_linear(s: f32) -> f32 {
    if s <= 0.040_45 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// 打包成 shader 用的调色板：`[0..15]` 材质，`[16..22]` 细胞类型。
///
/// 下标是硬约定，与 `blit.wgsl` 里的 `CELL_PALETTE_BASE + kind` 对应。
/// 中间空出整整一段，是为了让"材质"和"细胞"在调试时一眼能分开。
///
/// **这个偏移原本是 8，撞过车。** 材质种类从 7 涨到 9 之后，
/// `Algae = 8` 正好落在细胞的起始下标上——两个循环都往 `palette[8]` 写，
/// 后写的细胞配色覆盖了先写的材质配色，于是**藻被画成了嘴的橙红色**。
/// 画面上它只是"海底有一层橙色"，看起来像 bug 又不像 bug。
/// 现在偏移是 16，材质最多 16 种、细胞最多 7 种，两边都碰不到对方。
///
/// `srgb_output` 为真时转成线性——交换链是 sRGB 格式，着色器必须给线性值。
/// 为假（比如某些平台上拿到的是非 sRGB 格式）就直接给 sRGB 值。
pub fn scene_palette(srgb_output: bool) -> [[f32; 4]; PALETTE_LEN] {
    let to_uniform = |[r, g, b]: [u8; 3]| -> [f32; 4] {
        let f = |c: u8| {
            let s = f32::from(c) / 255.0;
            if srgb_output {
                srgb_to_linear(s)
            } else {
                s
            }
        };
        [f(r), f(g), f(b), 1.0]
    };

    let mut out = [[0.0f32; 4]; PALETTE_LEN];
    for kind in MaterialKind::ALL {
        out[kind.as_u8() as usize] = to_uniform(rgb(kind));
    }
    for kind in CellType::ALL {
        out[CELL_PALETTE_BASE + kind.as_u8() as usize] = to_uniform(cell_rgb(kind));
    }
    out
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

    /// 材质和细胞共用一张调色板，**两边的下标不能撞**。
    ///
    /// 这条测试是被一次真实的撞车逼出来的：材质从 7 种涨到 9 种之后，
    /// `Algae = 8` 正好落在细胞的起始下标（当时的 8）上，两个循环都往
    /// `palette[8]` 写，后写的细胞配色盖掉了先写的材质配色——
    /// **藻被画成了嘴的橙红色**，而画面上看起来只是"海底有一层橙色"。
    #[test]
    fn material_and_cell_palettes_do_not_collide() {
        // 下标够不够用由编译期断言保证（见 `PALETTE_LEN` 附近）；
        // 这里检查**实际写进去的颜色**。
        let p = scene_palette(false);
        let same = |a: [f32; 4], b: [u8; 3]| (0..3).all(|i| (a[i] * 255.0).round() as u8 == b[i]);
        for kind in MaterialKind::ALL {
            assert!(
                same(p[kind.as_u8() as usize], rgb(kind)),
                "{kind} 的配色被别的东西覆盖了"
            );
        }
        for kind in CellType::ALL {
            assert!(
                same(p[CELL_PALETTE_BASE + kind.as_u8() as usize], cell_rgb(kind)),
                "细胞 {kind:?} 的配色被别的东西覆盖了"
            );
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
