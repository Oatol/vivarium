//! 调色板的 **GPU 侧**：sRGB → 线性、打包成 uniform 数组、下标偏移。
//!
//! ## 数据不在这里
//!
//! "这种材质是什么颜色"住在 [`vivarium_core::palette`]——因为界面层也要用它
//! （底部调色板上的色块必须和世界里那一格是同一个颜色）。这里只剩
//! **渲染细节**：交换链是 sRGB 格式，着色器必须拿线性值。
//!
//! 一开始整张表都在这里，理由是"只有渲染器需要颜色"。加了界面之后
//! 这个理由不成立了，而在两处各维护一份 90% 相同的表必然漂移。
//!
//! 为了不让调用点全改一遍，数据部分在下面原样转发。

use vivarium_core::MaterialKind;

pub use vivarium_core::palette::{
    cell_rgb, glyph, organelle_rgb, organism_rgb, rgb, BACKGROUND, CELL_GLYPH, ORGANELLE_RGB,
};

/// 细胞类型在调色板里的起始下标。**必须与 `blit.wgsl` 里的同名常数一致。**
pub const CELL_PALETTE_BASE: usize = 16;

/// 调色板长度。材质占 `[0, 16)`，细胞器占 `[16, 26)`。
///
/// **v0.41 从 24 涨到 32。** 细胞那一半从"7 种类型"变成了"10 种细胞器"
/// （叶绿体/鞭毛/刺/嘴/装甲/气囊/眼/线粒体/建造/根），`16 + 10 = 26`，
/// 24 装不下了。32 是 16 的倍数，uniform 的 16 字节对齐好算。
pub const PALETTE_LEN: usize = 32;

// 下标够不够用是**编译期**的事，不该等到跑测试才发现。
const _: () = assert!(MaterialKind::COUNT <= CELL_PALETTE_BASE);
const _: () = assert!(CELL_PALETTE_BASE + ORGANELLE_RGB.len() <= PALETTE_LEN);

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
    // 细胞那一半装的是**细胞器**的基色，不是"细胞类型"的颜色。
    // 混色在 shader 里做（一格可能同时有叶绿体和气囊），
    // 所以这里只提供 10 个基色。
    for (i, c) in ORGANELLE_RGB.iter().enumerate() {
        out[CELL_PALETTE_BASE + i] = to_uniform(*c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for (i, c) in ORGANELLE_RGB.iter().enumerate() {
            assert!(
                same(p[CELL_PALETTE_BASE + i], *c),
                "细胞器 {i} 的配色被别的东西覆盖了"
            );
        }
        // 预设色块走的仍然是 `cell_rgb`，它必须等于"单一细胞器"的混色结果。
        // 不等的话，底部调色板上的色块和世界里画出来的颜色会对不上。
        for kind in vivarium_core::CellType::ALL {
            let single = organelle_rgb(vivarium_core::organelle::Organelles::from_cell_type(kind));
            assert_eq!(cell_rgb(kind), single, "{kind:?} 的预设色和混色结果不一致");
        }
    }
}
