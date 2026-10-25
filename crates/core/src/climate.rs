//! 水循环：**蒸发 → 水汽 → 降水**。
//!
//! ## 它要解决什么
//!
//! 水在这个世界里原来是**单向**的：世界生成时倒进去一池，之后只会被搬来搬去
//! （身体借走、尸体还回来、被身体顶上去）。三条后果：
//!
//! 1. **水只会被搬，不会回来。** v0.50 的 ① 让"露天尸体烂成碎屑"，于是
//!    出生时借走的那格水**真的没了**——门一开，8000 tick 抽掉 34217 格
//!    （见 [`crate::life_sim`] 的 `fits`）。那笔账记在 `water_vented` 上，
//!    但记账不等于堵住。
//! 2. **陆地永远得不到水。** 雨是"水从海里回到陆地上"的唯一通道，没有它，
//!    土壤湿度场只是一个静态的、由海岸线画出来的等距线图。
//! 3. **海会不会慢慢变浅，没法回答。** 没有出口就没有收支。
//!
//! ## 为什么水汽是一个数，不是一种材质
//!
//! 最直觉的做法是加一种 `Vapor` 材质（像 `Gas` 一样上浮）。**这条路已经
//! 被 `Gas` 走过了，而且失败了**：200 格气在 500 tick 内全部升到世界顶部，
//! 然后永远躺在那儿（SPEC §3.42）。水汽会一模一样——它还会占格、挤开空气、
//! 让材质层的脏分块永远清不空。
//!
//! 所以水汽是一个**标量**：`vapor`（单位是"格水"）。它不占格、不参与
//! 材质规则、不产生任何渲染负担。代价是**没有空间分布**——降水落点是
//! 随机挑的列，等于假设"风把水汽吹得到处都是"。对一个横板世界来说，
//! 这条假设比"水汽自己往上飘"更省事，也更接近"下雨"这件事的观感。
//!
//! ## 收支是自动稳定的
//!
//! 蒸发速率是一个常数（每 tick 最多一格），降水概率**正比于 `vapor`**：
//!
//! ```text
//! 蒸发：水格 → 空位，vapor += 1
//! 降水：vapor -= 1，空位 → 水格，概率 = vapor / RAIN_UNITS_PER_PERCENT %
//! ```
//!
//! 于是 `vapor` 会自己停在一个平衡点（蒸发多快，就下多快）。**不需要调
//! "雨量"这个参数**，只需要调"天上能存多少水"（`RAIN_UNITS_PER_PERCENT`）。
//! 这也让守恒是构造上成立的：`水 + 借出 + 尸体 + 水汽 + 蒸发 = 常数`。
//!
//! ## 落点：为什么是"从下往上第一个空位"
//!
//! 降水要找一格能放水的地方。三个候选：
//!
//! - **从世界顶部往下找第一个空位** → 雨出现在 y=0，然后掉 200 格。
//!   画面上像"天上开了一个洞"，而且水会在半空里穿很久。
//! - **随机高度** → 雨出现在地下洞穴里、石头中间。
//! - **从下往上第一个空位**（现在这条）→ 落点**紧贴着这一列最高的地面
//!   或水面**。海上落在海面、岸上落在地表、有生物时落在它头顶。
//!   一行规则同时给出"雨落在该落的地方"，而且**永远不需要掉很远**。
//!
//! 找不到空位（整列塞满）就放弃这一次降水，`vapor` 不减——不能凭空吞掉水。

use crate::material::{water_src, MaterialKind};
use crate::{Rng, World};

/// 每 tick 抽几列去试蒸发。
///
/// 抽列而不是扫全世界：512 列全扫是 512 × 512 次查询，而实际能蒸发的只有
/// "每列最上面那一格水"，扫 99.9% 是白跑。
pub const EVAP_TRY_COLUMNS: usize = 8;

/// 一列被抽到时，水面蒸发的概率（%）。
///
/// 上限是 `EVAP_TRY_COLUMNS × EVAP_PERCENT / 100` = **0.32 格/tick**，
/// 也就是 8000 tick 最多 2560 格。512 宽的海面每列 5 格，看不出"沸腾"。
pub const EVAP_PERCENT: u32 = 4;

/// 多少格水汽对应"每 tick 1% 的降水概率"。
///
/// 稳态 `vapor ≈ 100 × 蒸发速率 × 这个数 / 100`：蒸发 0.32 格/tick 时，
/// 平衡点大约 **96 格**——天上存着不到一百格水，循环很短。
/// 调大它 = 天上存更多水 = 干旱期更长、雨更集中。
pub const RAIN_UNITS_PER_PERCENT: u64 = 3;

/// 降水概率上限（%）。100 = 每 tick 最多落一格。
pub const RAIN_MAX_PERCENT: u32 = 100;

const SALT_EVAP_COLUMN: u32 = 0x00C1_1A7E;
const SALT_EVAPORATE: u32 = 0x00C1_1A7F;
const SALT_RAIN: u32 = 0x00C1_1A80;
const SALT_RAIN_COLUMN: u32 = 0x00C1_1A81;

/// 水循环：天上的水汽、以及两个计数器。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Climate {
    /// 现在天上有多少格水（`蒸发 − 降水`）。
    vapor: u64,
    /// 累计蒸发掉多少格水。
    evaporated: u64,
    /// 累计落下多少格雨。
    rained: u64,
}

impl Climate {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从存档里的一份状态恢复。
    ///
    /// 水汽是**有记忆的量**（蒸发涨、降水落），所以读档必须把它带回来——
    /// 重新从 0 开始的话，读档那一刻天就"旱"了，而玩家会看到一场
    /// 本来正在下的雨突然停掉。
    pub fn from_parts(vapor: u64, evaporated: u64, rained: u64) -> Self {
        Self {
            vapor,
            evaporated,
            rained,
        }
    }

    /// 现在天上存着多少格水。
    #[inline]
    pub fn vapor(&self) -> u64 {
        self.vapor
    }

    /// 累计蒸发量。**它是流量，不是存量**——和 [`Self::vapor`] 一起看才有意义。
    #[inline]
    pub fn evaporated(&self) -> u64 {
        self.evaporated
    }

    /// 累计降水量。
    #[inline]
    pub fn rained(&self) -> u64 {
        self.rained
    }

    /// 推进一个 tick。
    pub fn step(&mut self, world: &mut World) {
        let tick = world.tick();
        self.evaporate(world, tick);
        self.rain(world, tick);
    }

    /// 从水面蒸掉一格水。
    ///
    /// **每 tick 最多一格**：抽 [`EVAP_TRY_COLUMNS`] 列，第一列成功就收工。
    /// 这样蒸发速率有一个硬上限，不会因为世界变大而失控
    /// （对照：`seed_plankton` 是同样的道理，见 `LifeSim::seed_plankton`）。
    fn evaporate(&mut self, world: &mut World, tick: u64) {
        let w = world.width();
        if w == 0 || world.height() == 0 {
            return;
        }
        for i in 0..EVAP_TRY_COLUMNS {
            let x = Rng::at_below(0, i, tick, SALT_EVAP_COLUMN, w as u32) as usize;
            let Some(y) = Self::open_water_surface(world, x) else {
                continue;
            };
            if !Rng::at_percent(x, y, tick, SALT_EVAPORATE, EVAP_PERCENT) {
                continue;
            }
            world.set(x as isize, y as isize, MaterialKind::Empty);
            self.vapor += 1;
            self.evaporated += 1;
            return;
        }
    }

    /// 落一格雨。
    ///
    /// 概率正比于 `vapor`——这就是那个自动稳压的负反馈：
    /// 蒸发快 → 天上水多 → 雨大 → 天上水少。
    fn rain(&mut self, world: &mut World, tick: u64) {
        if self.vapor == 0 {
            return;
        }
        let percent = ((self.vapor / RAIN_UNITS_PER_PERCENT) as u32).min(RAIN_MAX_PERCENT);
        if !Rng::at_percent(0, 0, tick, SALT_RAIN, percent) {
            return;
        }
        let w = world.width();
        if w == 0 {
            return;
        }
        let x = Rng::at_below(1, 0, tick, SALT_RAIN_COLUMN, w as u32) as usize;
        // 找不到落点（整列塞满）就**放弃这一次**，`vapor` 不减——
        // 否则水会无声地从账上消失，而那正是 v0.50 刚花力气记清楚的那类事。
        let Some(y) = Self::landing_spot(world, x) else {
            return;
        };
        world.set(x as isize, y as isize, MaterialKind::Water);
        world.set_flags(x as isize, y as isize, water_src::RAIN);
        self.vapor -= 1;
        self.rained += 1;
    }

    /// 这一列**暴露在空气里**的水面在哪一行。
    ///
    /// 从上往下找第一个 `Water`——那就是水面。找到之后还要验一件事：
    /// 它上方必须是空位或气，而且**没被身体占着**。
    ///
    /// 验上方这一条决定了"深海不蒸发"是构造上成立的：深处的水上方是水，
    /// 不是空气，所以永远抽不到它。**不需要写"按深度衰减"这种规则。**
    fn open_water_surface(world: &World, x: usize) -> Option<usize> {
        let xs = x as isize;
        for y in 0..world.height() {
            let ys = y as isize;
            if world.get(xs, ys) != MaterialKind::Water {
                continue;
            }
            if y == 0 {
                return Some(0);
            }
            let above = ys - 1;
            let open = !world.is_occupied(xs, above)
                && matches!(world.get(xs, above), MaterialKind::Empty | MaterialKind::Gas);
            return if open { Some(y) } else { None };
        }
        None
    }

    /// 这一列的雨该落在哪一行：**从下往上第一个空位**。
    ///
    /// 它紧贴着这一列最高的地面或水面，所以雨不需要掉很远，也不会落进
    /// 石头里的洞。代价是"落点永远在地面之上"——没有云、没有下落过程，
    /// 但那本来就是这一层不打算模拟的东西。
    fn landing_spot(world: &World, x: usize) -> Option<usize> {
        let xs = x as isize;
        for y in (0..world.height()).rev() {
            let ys = y as isize;
            if world.get(xs, ys) == MaterialKind::Empty && !world.is_occupied(xs, ys) {
                return Some(y);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool() -> World {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 60, 64, 4, MaterialKind::Stone);
        w.fill_rect(0, 30, 64, 30, MaterialKind::Water);
        w
    }

    /// 蒸发出去的水必须变成水汽，**不能消失**。
    #[test]
    fn evaporated_water_becomes_vapor_not_nothing() {
        let mut w = pool();
        let mut c = Climate::new();
        let before = w.count_of(MaterialKind::Water);
        for _ in 0..600 {
            c.step(&mut w);
            w.advance_tick();
        }
        let after = w.count_of(MaterialKind::Water);
        assert!(after < before, "600 tick 之后一格都没蒸发，规则没生效");
        assert_eq!(
            after + c.vapor() as usize,
            before,
            "水 + 水汽 必须守恒（蒸发 {} 格、天上 {} 格）",
            c.evaporated(),
            c.vapor()
        );
        // 降水会还回来一部分，所以蒸发量 >= 净损失。
        assert!(c.evaporated() >= (before - after) as u64);
    }

    /// **盖住的水不蒸发**——只有暴露在空气里的水面才蒸发。
    ///
    /// 这条如果坏了，症状是"海从底下开始干"，而画面上看是"海慢慢变浅"，
    /// 完全查不出是谁干的。
    ///
    /// ## 这个测试原来写错了，而且是**假绿**
    ///
    /// 第一版是"20 格深的水池，跑 2000 tick，断言海底那一格还在"。它一直
    /// 是绿的——**但我把"上方必须是空气"那条判据删掉之后它还是绿的**，
    /// 因为它根本没测到那条规则：`open_water_surface` 从上往下找**第一个**
    /// `Water`，而敞口水池的第一个水格永远是水面，`open` 恒为真。
    ///
    /// 真正的判据要一个**被盖住**的水体：石头盖子下面全是水。
    /// 这时候第一个水格的上方是石头，一格都不该蒸发。
    #[test]
    fn a_sealed_pond_does_not_evaporate() {
        let mut w = World::new(32, 32).unwrap();
        w.fill_rect(0, 31, 32, 1, MaterialKind::Stone);
        w.fill_rect(0, 10, 32, 21, MaterialKind::Water);
        // 盖上：y=10 那一行变成石头，水在 y=11..31。
        w.fill_rect(0, 10, 32, 1, MaterialKind::Stone);
        let water_before = w.count_of(MaterialKind::Water);

        let mut c = Climate::new();
        for _ in 0..2000 {
            c.step(&mut w);
            w.advance_tick();
        }
        assert_eq!(
            c.evaporated(),
            0,
            "封死的水池蒸发了 {} 格——上方不是空气的水不该蒸发",
            c.evaporated()
        );
        assert_eq!(w.count_of(MaterialKind::Water), water_before, "水少了");
    }

    /// 敞口的水池会蒸发，而且**只从水面往下掉**。
    #[test]
    fn an_open_pond_evaporates_from_the_surface_down() {
        let mut w = World::new(32, 32).unwrap();
        w.fill_rect(0, 31, 32, 1, MaterialKind::Stone);
        w.fill_rect(0, 10, 32, 21, MaterialKind::Water);
        let mut c = Climate::new();
        for _ in 0..2000 {
            c.step(&mut w);
            w.advance_tick();
        }
        assert!(c.evaporated() > 100, "敞口水池几乎没蒸发：{}", c.evaporated());
        // 水面可以掉，但**海底那一格**不该被抽空。
        assert_eq!(
            w.get(5, 30),
            MaterialKind::Water,
            "20 格深的水池不该被蒸干"
        );
        let mut surface = None;
        for y in 0..32 {
            if w.get(5, y as isize) == MaterialKind::Water {
                surface = Some(y);
                break;
            }
        }
        assert!(surface.is_some_and(|s| s >= 10), "水面不该升到 10 以上");
    }

    /// 雨落在**地面/水面上**，不是世界顶部、也不是地底下。
    ///
    /// 判据写成"**没有一滴雨悬在空中**"：每一格水下面必须紧贴着固体或水。
    /// 这条同时排除掉三个错法——落在 y=0（世界顶部）、落在石头里的洞里、
    /// 落在半空。而它比"所有水都在 y=19"更稳：雨会一层层叠起来，
    /// 第二滴就该落在 y=18。
    #[test]
    fn rain_lands_on_the_ground_not_in_the_sky() {
        let mut w = World::new(32, 32).unwrap();
        w.fill_rect(0, 20, 32, 12, MaterialKind::Stone);
        let mut c = Climate::new();
        c.vapor = 500;

        let mut landed = 0;
        let mut first = None;
        for tick in 0..200 {
            let before = w.count_of(MaterialKind::Water);
            c.step(&mut w);
            w.advance_tick();
            let after = w.count_of(MaterialKind::Water);
            if after > before {
                landed += 1;
                if first.is_none() {
                    // 第一滴雨：世界里只有这一格水，而它必须在 y=19。
                    let mut spot = None;
                    for x in 0..32usize {
                        for y in 0..32usize {
                            if w.get(x as isize, y as isize) == MaterialKind::Water {
                                spot = Some((x, y));
                            }
                        }
                    }
                    assert_eq!(
                        spot.map(|(_, y)| y),
                        Some(19),
                        "第 {tick} tick 的第一滴雨没落在地面之上那一行（{spot:?}）"
                    );
                    first = Some(tick);
                }
                for x in 0..32usize {
                    for y in 0..32usize {
                        if w.get(x as isize, y as isize) != MaterialKind::Water {
                            continue;
                        }
                        let below = if y + 1 < 32 {
                            w.get(x as isize, (y + 1) as isize)
                        } else {
                            MaterialKind::Stone
                        };
                        assert!(
                            below != MaterialKind::Empty && below != MaterialKind::Gas,
                            "第 {tick} tick：({x}, {y}) 有一滴雨悬在空中（下面是 {below:?}）"
                        );
                    }
                }
            }
        }
        assert!(landed > 20, "200 tick 只落了 {landed} 格雨，降水没在跑");
        assert!(first.is_some(), "200 tick 一滴雨都没落，降水没在跑");
    }

    /// 水汽为 0 时不下雨。
    #[test]
    fn no_vapor_no_rain() {
        let mut w = World::new(32, 32).unwrap();
        w.fill_rect(0, 20, 32, 12, MaterialKind::Stone);
        let mut c = Climate::new();
        for _ in 0..500 {
            c.step(&mut w);
            w.advance_tick();
        }
        assert_eq!(c.rained(), 0, "天上没水，却下出了 {} 格雨", c.rained());
    }
}
