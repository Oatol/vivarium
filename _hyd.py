import re

# ---------------- life.rs：hydration 字段 ----------------
p = "crates/core/src/life.rs"
s = open(p, encoding="utf-8").read()

old = '''    /// **生物量**：这一具身体本身值多少能量。'''
new = '''    /// **含水量**（0..=255）。
    ///
    /// 光合要水，而"水"原来只有一个判据：身体所在的粗格有多少比例是水。
    /// 那让**整株植物都被钉在水里**——离开水面，产量立刻归零，
    /// 没有任何"带一点水走"的概念。
    ///
    /// 现在它变成一个有惯性的量：
    /// - 根贴着水（或有根且身体泡在水里）→ 回满；
    /// - 否则每 tick 掉 `HYDRATION_DRAIN`，光合按 `hydration / 255` 打折。
    ///
    /// 于是"根扎在水边、身子长在岸上"第一次成为可能。
    pub hydration: u8,
    /// **生物量**：这一具身体本身值多少能量。'''
assert old in s
s = s.replace(old, new, 1)

s = s.replace("            energy: 0,\n            structure: 0,",
              "            energy: 0,\n            structure: 0,\n            hydration: HYDRATION_MAX,", 1)

old = '''pub const STRUCTURE_SHARE: u32 = 8;'''
new = '''pub const STRUCTURE_SHARE: u32 = 8;

/// 含水量的满值。
pub const HYDRATION_MAX: u8 = u8::MAX;

/// 每 tick 掉多少含水量（0..=255 的刻度）。
///
/// 1：满水到干透 255 tick，1× 速度下约 4 秒。**这个数是"能观察到的尺度"**——
/// 太大（比如 8）会让植物出水就秒死，太小（比如 0.1 折算不了整数）会让
/// "离水"这件事没有代价。4 秒够玩家看见"它开始发黄、然后停下"。
pub const HYDRATION_DRAIN: u8 = 1;'''
assert old in s
s = s.replace(old, new, 1)
open(p, "w", encoding="utf-8").write(s)
print("life ok")

# ---------------- life_sim.rs ----------------
p = "crates/core/src/life_sim.rs"
s = open(p, encoding="utf-8").read()

# 常量
old = '''/// 完全上岸时，每一步移动贵几倍。'''
new = '''/// 根能提供的**最低湿度**：根扎在水边时，整株植物按这个比例产能。
///
/// 0.35 ≈ 水里的三分之一。够慢但够活——陆生植物本来就该长得慢，
/// 而"慢"正是它和水中植物之间该有的取舍。
pub const LAND_DAMP: f32 = 0.35;

/// 完全上岸时，每一步移动贵几倍。'''
assert old in s
s = s.replace(old, new, 1)

# import ROOT
old = "use crate::organelle::{ARMOR, CHLORO_SHIFT, DIGEST, STING};"
new = "use crate::organelle::{ARMOR, CHLORO_SHIFT, DIGEST, ROOT, STING};"
assert old in s
s = s.replace(old, new, 1)

# 光合：damp 与含水量
old = '''            let damp = dampness(self.field.water_frac(ax, ay));
            if damp <= 0.0 {
                continue;
            }'''
new = '''            let mut damp = dampness(self.field.water_frac(ax, ay));
            // **根贴着水 → 至少 LAND_DAMP。**
            //
            // 这一条才是"植物能上岸"的全部：根扎在岸边的水里，
            // 身体长在岸上，光合按三分之一跑。没有根的话 `damp` 仍然是
            // 粗格水占比——离开水面就是 0。
            if damp < LAND_DAMP && Self::root_is_wet(&*world, org) {
                damp = LAND_DAMP;
            }
            if damp <= 0.0 {
                continue;
            }
            // 再乘含水量：离水之后它靠身体里存的那点水撑一阵。
            let wet = org.hydration as f32 / crate::life::HYDRATION_MAX as f32;
            if wet <= 0.0 {
                continue;
            }'''
assert old in s
s = s.replace(old, new, 1)

old = '''            let gain = (PRODUCER_YIELD as f32 * light * warmth * damp * units) as u32;'''
new = '''            let gain = (PRODUCER_YIELD as f32 * light * warmth * damp * wet * units) as u32;'''
assert old in s
s = s.replace(old, new, 1)

# tick_hydration + root_is_wet：插在 inject_sunlight 之前
old = '''    /// 生产者按**所在位置的光照**产能量。'''
new = '''    /// 这具身体有没有**根贴着水**。
    ///
    /// 判据是"某一格根细胞的四邻里有水"——**根要够得着水，不是身体泡在水里**。
    /// 这就是根和"整具身体泡在水里"的区别：后者要求锚点所在的粗格有水，
    /// 前者只要求某一格根挨着一格水。
    fn root_is_wet(world: &World, org: &Organism) -> bool {
        for cell in &org.cells {
            if !cell.organelles.has(ROOT) {
                continue;
            }
            let (x, y) = org.cell_at(cell);
            for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                if world.get((x + dx) as isize, (y + dy) as isize) == MaterialKind::Water {
                    return true;
                }
            }
        }
        false
    }

    /// 每 tick 更新含水量。
    ///
    /// 放在 `inject_sunlight` **之前**：光合这一 tick 用的是这一 tick 的水。
    fn tick_hydration(&mut self, world: &World) {
        for org in &mut self.organisms {
            let wet = Self::root_is_wet(world, org)
                || dampness(self.field.water_frac(org.anchor.0 as f32, org.anchor.1 as f32)) > 0.5;
            if wet {
                org.hydration = crate::life::HYDRATION_MAX;
            } else {
                org.hydration = org
                    .hydration
                    .saturating_sub(crate::life::HYDRATION_DRAIN);
            }
        }
    }

    /// 生产者按**所在位置的光照**产能量。'''
assert old in s
s = s.replace(old, new, 1)

old = '''        // 1) 太阳能。生产者是唯一的"环境 → 生物"入口（SPEC §3.3）。
        self.inject_sunlight(world);'''
new = '''        // 1) 含水量。放在太阳能**之前**：这一 tick 的光合用这一 tick 的水。
        self.tick_hydration(world);

        // 1b) 太阳能。生产者是唯一的"环境 → 生物"入口（SPEC §3.3）。
        self.inject_sunlight(world);'''
assert old in s
s = s.replace(old, new, 1)

# 子代含水量
old = '''            // 子代的生物量从 0 开始：它还没长大。'''
new = '''            hydration: crate::life::HYDRATION_MAX,
            // 子代的生物量从 0 开始：它还没长大。'''
assert old in s
s = s.replace(old, new, 1)
open(p, "w", encoding="utf-8").write(s)
print("life_sim ok")

# ---------------- save.rs：v5 ----------------
p = "crates/core/src/save.rs"
s = open(p, encoding="utf-8").read()
s = s.replace("pub const VERSION: u16 = 4;", "pub const VERSION: u16 = 5;", 1)
old = '''    out.u32(org.structure);'''
new = '''    out.u32(org.structure);
    // v5：含水量。它是**积分状态**（离水之后慢慢掉），不是布局的纯函数，
    // 不存的话读档之后每一株植物都会瞬间"满水"，而画面上看不出来。
    out.u8(org.hydration);'''
assert old in s
s = s.replace(old, new, 1)
old = '''    let structure = r.u32()?;'''
new = '''    let structure = r.u32()?;
    let hydration = r.u8()?;'''
assert old in s
s = s.replace(old, new, 1)
old = '''        energy,
        structure,'''
new = '''        energy,
        structure,
        hydration,'''
assert old in s
s = s.replace(old, new, 1)
open(p, "w", encoding="utf-8").write(s)
print("save ok")
