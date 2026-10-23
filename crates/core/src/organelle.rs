//! 细胞器：一格细胞里有什么，而不是"它是什么"。
//!
//! ## 为什么从枚举换成位掩码
//!
//! `CellType` 是一个**封闭枚举**：加一种细胞 = 改七处 `match` + 存档升版 +
//! 进化池变大。而且"进化发明新细胞"在类型系统层面就不可能——`random_kind`
//! 只能从 `ALL` 里挑，挑不出一个还没被写进代码的类型。
//!
//! 位掩码把组合数从 `N` 变成 `2^N`，而进化只需**翻一个 bit**：
//!
//! | 变异 | 结果 |
//! |---|---|
//! | 从 5 个枚举里重抽（旧） | `Mover` 变成 `Producer`——**功能整体换掉，形状没变** |
//! | 翻一个 bit（新） | `鞭毛` → `鞭毛 + 气囊`——**其他部分完全不变，只多一个能力** |
//!
//! **重抽是跳崖，翻 bit 是走一步。** 这是"进化从没长出新东西"的直接原因之一。
//!
//! ## 组合就是类型
//!
//! 功能由**组合**决定，不由枚举决定：
//!
//! | 组合 | 涌现出的"类型" |
//! |---|---|
//! | 叶绿体 | 生产者 |
//! | 叶绿体 + 装甲 | 硬壳藻 |
//! | 叶绿体 + 气囊 | 漂浮的藻垫 |
//! | 鞭毛 + 刺 | 冲锋的捕食者 |
//! | 装甲 + 气囊 | 会浮的盾 |
//!
//! 没有人写"叶绿体 + 装甲 = 硬壳藻"这条规则。是 `upkeep` 加性、`density` 平均
//! 这两条算出来之后，**这个组合自己变成了一种活法**。
//!
//! ## 过渡期：`CellType` 还在
//!
//! M2a 阶段 `Cell` 同时有 `kind` 和 `organelles`，两者由 [`Cell::new`] 保证一致。
//! 这是刻意的——`kind` 是旧世界的锚（所有现有 `match` 都读它），`organelles`
//! 是新世界的锚（气囊、密度、梯度规则读它）。M2b 删掉 `kind` 之后，
//! `organelles` 变成唯一真相。

use crate::life::CellType;

/// 一格细胞带的细胞器。
///
/// 位布局见下面那组常量。`u16` 用了 12 位，剩 4 位留给以后。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Organelles(pub u16);

// ---------------------------------------------------------------------------
// 位布局
// ---------------------------------------------------------------------------

/// 叶绿体：按光照产能。**唯一的环境 → 生物能量入口。**
pub const CHLORO_SHIFT: u32 = 0;
/// 鞭毛：提供位移。
pub const FLAGELLUM_SHIFT: u32 = 2;
/// 刺细胞：咬邻格的别的生物。
pub const STING_SHIFT: u32 = 4;

/// "有叶绿体"（任意等级）的掩码。用于 `has(CHLORO)`。
pub const CHLORO: u16 = 0b11 << CHLORO_SHIFT;
/// "有鞭毛"（任意等级）的掩码。
pub const FLAGELLUM: u16 = 0b11 << FLAGELLUM_SHIFT;
/// "有刺细胞"（任意等级）的掩码。
pub const STING: u16 = 0b11 << STING_SHIFT;

/// 消化液泡：吃邻格的碎屑 / 尸体 / 藻。
pub const DIGEST: u16 = 1 << 6;
/// 装甲板：减免受到的伤害。
pub const ARMOR: u16 = 1 << 7;
/// 气囊：把整只生物的密度往下拉。
pub const GAS: u16 = 1 << 8;
/// 眼点：扩大感知范围。
pub const EYE: u16 = 1 << 9;
/// 线粒体：**这一格的电源**。默认存在，可被变异关掉。
///
/// 它的 `upkeep` 是 **0**，这不是笔误。线粒体不是一个"要付钱的器官"，
/// 而是"所有主动功能的前提"——它的代价通过"没有它时主动细胞器降到 25%"
/// 来表达（见 [`Organelles::efficiency`]），而不是通过维持成本。
///
/// 这样设计的好处：一个纯装甲身体可以全身不带线粒体、`upkeep` 极低——
/// **它就是一块石头**，动不了、吃不了、产不了能，只会被啃。这正是它该有的命运。
pub const MITO: u16 = 1 << 10;
/// 建造：让整个身体的维持成本下降。
///
/// **过渡期保留。** 它现在被 `grn.rs` 的 `random_kind` 排除在外
/// （"构造器还没接上，长出来只是白付维持成本"），所以进化从来没长过它，
/// 它只活在手工体型里。M2b 重做身体描述时会重新考虑要不要留。
pub const BUILDER: u16 = 1 << 11;

/// 可数细胞器的等级上限（0..=2）。
pub const MAX_LEVEL: u8 = 2;

impl Organelles {
    /// 什么都没有。
    pub const NONE: Self = Self(0);

    /// 单细胞器的便捷构造（等价于 `level = 1`）。
    pub const fn one(flag: u16) -> Self {
        Self(flag)
    }

    /// 有没有这个（布尔）细胞器。
    #[inline]
    pub const fn has(self, flag: u16) -> bool {
        self.0 & flag != 0
    }

    /// 可数细胞器的等级（0..=2）。
    ///
    /// 越界（存了个 3）会被夹到 2——存档被改坏时宁可退化成一个合法的身体，
    /// 也不要让一次读档变成 panic。
    #[inline]
    pub fn level(self, shift: u32) -> u8 {
        (((self.0 >> shift) & 0b11) as u8).min(MAX_LEVEL)
    }

    /// 设置等级，返回新值。越界的等级被夹到 `MAX_LEVEL`。
    #[inline]
    pub const fn with_level(self, shift: u32, level: u8) -> Self {
        let mask = !(0b11u16 << shift);
        let lv = if level > MAX_LEVEL { MAX_LEVEL } else { level };
        Self((self.0 & mask) | ((lv as u16) << shift))
    }

    /// 加一个布尔细胞器。
    #[inline]
    pub const fn with(self, flag: u16) -> Self {
        Self(self.0 | flag)
    }

    /// 去掉一个布尔细胞器。
    #[inline]
    pub fn without(self, flag: u16) -> Self {
        Self(self.0 & !flag)
    }

    /// 翻转一个 bit。**这是变异的基本操作。**
    #[inline]
    pub fn toggle(self, flag: u16) -> Self {
        Self(self.0 ^ flag)
    }

    /// 有没有任何一个"主动"细胞器（需要线粒体才能工作的那些）。
    pub fn has_active(self) -> bool {
        self.level(CHLORO_SHIFT) > 0
            || self.level(FLAGELLUM_SHIFT) > 0
            || self.level(STING_SHIFT) > 0
            || self.has(DIGEST)
            || self.has(EYE)
            || self.has(BUILDER)
    }

    /// 这一格的**功能效率**。
    ///
    /// 没有线粒体时，主动细胞器全部降到 25%；装甲和气囊是**被动结构**，
    /// 本来就不需要能量，所以不受影响。
    ///
    /// **为什么是 25% 而不是 0**：完全关闭是一次**跳崖式变异**（一次翻转让整格
    /// 功能归零），和"渐变"原则相反。25% 让"关掉线粒体"成为一个
    /// **有程度、可反悔**的决定——如果这个方向不利，变异可以把线粒体再打开。
    #[inline]
    pub fn efficiency(self) -> f32 {
        if self.has(MITO) || !self.has_active() {
            1.0
        } else {
            0.25
        }
    }

    /// 这一格的维持成本。
    ///
    /// **加性**：一格干两件事就付两份钱。这是"多功能细胞更贵"的正确表达。
    ///
    /// **等级超线性**（`base × level^1.2`）：1 级 5，2 级 12（不是 10）。
    /// 这条不是装饰——没有它，一格 2 级叶绿体就**严格优于**两格各 1 级
    /// （省一半体积、被吃面积一半、移动质量少一半），于是所有生物都会把细胞
    /// 堆满等级，"多长一格 / 长成什么形状"这个维度就废了。
    pub fn upkeep(self) -> u32 {
        let mut sum = 0u32;
        for (shift, base) in [
            (CHLORO_SHIFT, 5u32),
            (FLAGELLUM_SHIFT, 4),
            (STING_SHIFT, 3),
        ] {
            sum += match self.level(shift) {
                0 => 0,
                1 => base,
                // level^1.2，四舍五入。2^1.2 ≈ 2.297
                _ => (base as f32 * 2.297).round() as u32,
            };
        }
        for (flag, cost) in [
            (DIGEST, 2u32),
            (ARMOR, 1),
            (GAS, 1),
            (EYE, 1),
            (BUILDER, 6),
        ] {
            if self.has(flag) {
                sum += cost;
            }
        }
        // MITO 不计费，见它的文档。
        sum
    }

    /// 这一格的密度（相对于水 = 1.0）。
    ///
    /// **加权平均**，不是加性。加性的话气囊（0.15）永远压不过装甲（1.35）——
    /// `0.15` 加进总和只会让平均值更糟。平均才让"10 格里塞 3 个气囊就能浮起来"
    /// 这件事成立。
    ///
    /// **线粒体不计入密度。** 它是一格内部的"器官"，不是体积——如果计入，
    /// 每格都会多一个 1.0 的权重，把装甲和气囊都往 1.0 拽：
    /// 纯气囊会变成 `(0.15 + 1.0) / 2 = 0.575`，浮力直接腰斩。
    /// 而"气囊 0.15 / 装甲 1.35"这组数是按"不计线粒体"标定的。
    pub fn density(self) -> f32 {
        let mut sum = 0.0f32;
        let mut n = 0u32;
        for (shift, d) in [
            (CHLORO_SHIFT, 0.95f32),
            (FLAGELLUM_SHIFT, 1.05),
            (STING_SHIFT, 1.05),
        ] {
            let lv = self.level(shift);
            if lv > 0 {
                sum += d * lv as f32;
                n += lv as u32;
            }
        }
        for (flag, d) in [
            (DIGEST, 1.00f32),
            (ARMOR, 1.35),
            (GAS, 0.15),
            (EYE, 1.00),
            (BUILDER, 1.00),
        ] {
            if self.has(flag) {
                sum += d;
                n += 1;
            }
        }
        if n == 0 {
            // 空细胞（或只有线粒体）：当作和水一样重。
            1.0
        } else {
            sum / n as f32
        }
    }

    /// 从旧枚举映射过来。**这是过渡期的唯一桥。**
    ///
    /// 每个 `CellType` 映射到"一个细胞器 + 线粒体"。因为是一对一，
    /// [`Organelles::primary_cell_type`] 能精确还原，所以 M2a 是零行为变化。
    pub const fn from_cell_type(k: CellType) -> Self {
        let base = match k {
            CellType::Mouth => Self(DIGEST),
            CellType::Producer => Self(1 << CHLORO_SHIFT),
            CellType::Mover => Self(1 << FLAGELLUM_SHIFT),
            CellType::Eye => Self(EYE),
            CellType::Killer => Self(1 << STING_SHIFT),
            CellType::Armor => Self(ARMOR),
            CellType::Builder => Self(BUILDER),
        };
        // 线粒体默认打开——"默认所有细胞都带一个"。
        Self(base.0 | MITO)
    }

    /// 反查一个"代表类型"，用于渲染和统计。
    ///
    /// 组合出来的细胞（叶绿体 + 气囊）没有唯一的类型，这里按**功能强度**
    /// 挑一个最有代表性的：攻击 > 进食 > 产能 > 移动 > 防御 > 感知 > 建造。
    /// 顺序和渲染层的配色优先级一致。
    ///
    /// 只有气囊 / 线粒体的细胞会落到兜底分支——它没有代表类型，
    /// 兜底返回 `Mouth` 只为了不让调用方拿到 `Option`。
    pub fn primary_cell_type(self) -> CellType {
        if self.level(STING_SHIFT) > 0 {
            CellType::Killer
        } else if self.has(DIGEST) {
            CellType::Mouth
        } else if self.level(CHLORO_SHIFT) > 0 {
            CellType::Producer
        } else if self.level(FLAGELLUM_SHIFT) > 0 {
            CellType::Mover
        } else if self.has(ARMOR) {
            CellType::Armor
        } else if self.has(EYE) {
            CellType::Eye
        } else if self.has(BUILDER) {
            CellType::Builder
        } else {
            CellType::Mouth
        }
    }

    /// 有没有任何一个细胞器（不含线粒体）。
    ///
    /// "只有线粒体的细胞"在功能上等于空细胞，但它是合法的中间态。
    pub fn is_bare(self) -> bool {
        (self.0 & !MITO) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cell_type_round_trips() {
        // 这是 M2a 的全部赌注：如果往返不精确，行为就会变。
        for k in CellType::ALL {
            let o = Organelles::from_cell_type(k);
            assert_eq!(o.primary_cell_type(), k, "{k:?} 往返失败");
            assert!(o.has(MITO), "{k:?} 应当默认带线粒体");
        }
    }

    #[test]
    fn from_cell_type_preserves_upkeep() {
        // 零行为变化的直接判据：新旧两条路的 upkeep 必须完全相同。
        for k in CellType::ALL {
            assert_eq!(
                Organelles::from_cell_type(k).upkeep(),
                k.upkeep(),
                "{k:?} 的 upkeep 变了"
            );
        }
    }

    #[test]
    fn level_is_clamped() {
        // 存档被改坏时宁可退化，不要 panic。
        let o = Organelles(0b1111 << CHLORO_SHIFT);
        assert_eq!(o.level(CHLORO_SHIFT), MAX_LEVEL);
        assert_eq!(Organelles::NONE.with_level(CHLORO_SHIFT, 9).level(CHLORO_SHIFT), 2);
    }

    #[test]
    fn levels_are_superlinear() {
        // 一格 2 级必须比两格 1 级更贵，否则"堆满等级"是纯赚。
        let one = Organelles::NONE.with_level(CHLORO_SHIFT, 1);
        let two = Organelles::NONE.with_level(CHLORO_SHIFT, 2);
        assert_eq!(one.upkeep(), 5);
        assert!(
            two.upkeep() > one.upkeep() * 2,
            "2 级 upkeep {} 没有超过两倍 1 级 {}",
            two.upkeep(),
            one.upkeep() * 2
        );
    }

    #[test]
    fn armor_sinks_and_gas_floats() {
        // 密度必须是**加权平均**。加性的话气囊（0.15）永远压不过装甲（1.35）。
        let armor = Organelles::one(ARMOR).with(MITO);
        assert!(armor.density() > 1.3, "纯装甲应当沉底：{}", armor.density());

        let gas = Organelles::one(GAS).with(MITO);
        assert!(gas.density() < 0.2, "纯气囊应当上浮：{}", gas.density());

        // 整只生物的密度是逐格平均，所以"10 格里塞 3 个气囊"要能浮起来。
        // 这条正是设计文档里那张表的复算。
        let body = |n_armor: usize, n_gas: usize| {
            let mut sum = 0.0f32;
            for _ in 0..n_armor {
                sum += armor.density();
            }
            for _ in 0..n_gas {
                sum += gas.density();
            }
            sum / (n_armor + n_gas) as f32
        };
        assert!(body(10, 0) > 1.3, "10 格全装甲该沉");
        assert!(body(7, 3) > 0.95, "7 装甲 + 3 气囊应当勉强偏沉");
        assert!(body(5, 5) < 0.80, "5 + 5 应当上浮");
    }

    #[test]
    fn a_cell_without_mitochondria_is_handicapped() {
        let with = Organelles::NONE.with_level(CHLORO_SHIFT, 1).with(MITO);
        let without = Organelles::NONE.with_level(CHLORO_SHIFT, 1);
        assert_eq!(with.efficiency(), 1.0);
        assert_eq!(without.efficiency(), 0.25);

        // 被动结构不需要能量：纯装甲没有线粒体也是满效率。
        let armor = Organelles::one(ARMOR);
        assert_eq!(armor.efficiency(), 1.0, "装甲是被动结构，不该受线粒体影响");
    }

    #[test]
    fn all_organelles_is_a_losing_cell() {
        // 全能细胞必须是**负收益**——这就是"能量账本身就是禁令"。
        //
        // 一格叶绿体在浅水的净收益是 `PRODUCER_YIELD × 光 − upkeep`
        // ≈ 24 × 0.8 − 5 ≈ +14。所以"什么都长一点"的一格，
        // 只要 upkeep 超过 14，它就比不过一个纯生产者。
        let mut all = Organelles::NONE
            .with_level(CHLORO_SHIFT, 1)
            .with_level(FLAGELLUM_SHIFT, 1)
            .with_level(STING_SHIFT, 1)
            .with(MITO);
        for flag in [DIGEST, ARMOR, GAS, EYE] {
            all = all.with(flag);
        }
        assert!(
            all.upkeep() > 14,
            "全能细胞的 upkeep 只有 {}，比纯生产者的净收益还低——它就成了纯赚",
            all.upkeep()
        );
    }

    #[test]
    fn toggling_a_bit_only_changes_one_thing() {
        // 这是"翻 bit 是走一步"的直接证据：加一个气囊不改变任何别的细胞器。
        let base = Organelles::from_cell_type(CellType::Mover);
        let with_gas = base.toggle(GAS);
        assert!(with_gas.has(GAS));
        assert_eq!(with_gas.level(FLAGELLUM_SHIFT), base.level(FLAGELLUM_SHIFT));
        assert_eq!(with_gas.has(MITO), base.has(MITO));
        assert_eq!(with_gas.has(ARMOR), base.has(ARMOR));
    }
}
