//! 环境材质：世界网格里"非细胞"的那一半。
//!
//! ## 互斥规则
//!
//! 一格要么是材质，要么属于某个有机体的某个细胞，二者互斥（SPEC 第 2.1 节）。
//! 细胞死亡后该格变为 [`MaterialKind::Remains`]——**这是能量回流环境的通道，别省**。
//! 没有这条通道，生态要么饿死，要么爆炸。
//!
//! ## 为什么材质种类必须少
//!
//! 材质反应是"材质对 → 结果"的查找表。种类每多一种，需要考虑的组合就多一档。
//! 7 种材质是这个设计能成立的前提；一旦涨到几十种，你就得开始写规则引擎了。

use std::fmt;

/// 环境材质类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum MaterialKind {
    /// 空——生物可以自由通过。
    #[default]
    Empty = 0,
    /// 石——固体，不动，阻挡一切。
    Stone = 1,
    /// 沙——粉末，向下堆积；只有构造器细胞能改造它。
    Sand = 2,
    /// 水——液体，向下 / 向两侧流动。
    Water = 3,
    /// 火——消耗自身，点燃可燃物；对生物造成伤害。
    Fire = 4,
    /// 气——气体，向上 / 向两侧，可燃。
    Gas = 5,
    /// 残骸——细胞死亡后的产物，可被分解 / 吃掉，缓慢沉降为沙。
    Remains = 6,
}

impl MaterialKind {
    /// 材质种类总数。
    pub const COUNT: usize = 7;

    /// 全部材质，便于遍历与建表。
    pub const ALL: [MaterialKind; Self::COUNT] = [
        Self::Empty,
        Self::Stone,
        Self::Sand,
        Self::Water,
        Self::Fire,
        Self::Gas,
        Self::Remains,
    ];

    /// 从 `u8` 还原；越界时回退到 [`MaterialKind::Empty`]。
    #[inline]
    pub const fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Stone,
            2 => Self::Sand,
            3 => Self::Water,
            4 => Self::Fire,
            5 => Self::Gas,
            6 => Self::Remains,
            _ => Self::Empty,
        }
    }

    /// 转为 `u8`，用于紧凑存储与哈希。
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// 密度。液体 / 气体交换位置时比较，数值越大越"沉"。
    ///
    /// 这组数值同时决定了沉降顺序：气 < 火 < 水 < 沙 < 残骸 < 石。
    #[inline]
    pub const fn density(self) -> u8 {
        match self {
            Self::Empty => 0,
            Self::Gas => 1,
            Self::Fire => 2,
            Self::Water => 4,
            Self::Sand => 6,
            Self::Remains => 7,
            Self::Stone => 8,
        }
    }

    /// 是否为固体：不可移动，阻挡生物移动与繁殖。
    ///
    /// 注意 [`MaterialKind::Sand`] 也是固体——它靠重力堆积，不靠流动。
    #[inline]
    pub const fn is_solid(self) -> bool {
        matches!(self, Self::Stone | Self::Sand | Self::Remains)
    }

    /// 是否为流体（会移动的材质）。
    #[inline]
    pub const fn is_fluid(self) -> bool {
        matches!(self, Self::Water | Self::Gas | Self::Fire)
    }

    /// 是否可燃——火遇到它会点燃。
    #[inline]
    pub const fn is_flammable(self) -> bool {
        self.flammability() > 0
    }

    /// 可燃度，`0` 表示不可燃，越大越容易被点着。
    ///
    /// 火不会无条件点燃邻居，而是按这个值掷骰子（见 [`IGNITE_BASE_PERCENT`]）。
    /// 可燃度统一放在这里而不是散在模拟循环里，是因为"哪些东西能烧"是材质的属性，
    /// 不是火的属性——将来加一种可燃材质时只该改这一个函数。
    #[inline]
    pub const fn flammability(self) -> u8 {
        match self {
            // 气体易燃：一旦点着就顺着气团迅速蔓延。
            Self::Gas => 90,
            // 残骸是有机物，烧得起来但没那么猛。
            Self::Remains => 40,
            _ => 0,
        }
    }

    /// 是否对生物有害——生物进入该格会受伤。
    #[inline]
    pub const fn is_hazardous(self) -> bool {
        matches!(self, Self::Fire)
    }

    /// 该格能否被生物占用（M2 起用于移动判定）。
    ///
    /// 严格来说水只对"小生物"放行（SPEC 第 2.2 节），
    /// 尺寸判定留给有机体层，这里只回答"材质本身是否绝对阻挡"。
    #[inline]
    pub const fn blocks_organism(self) -> bool {
        self.is_solid()
    }

    // -----------------------------------------------------------------------
    // 移动规则
    // -----------------------------------------------------------------------

    /// 该材质在重力下的移动方式（SPEC 第 2.3 节的 `move_rule`）。
    #[inline]
    pub const fn move_rule(self) -> MoveRule {
        match self {
            Self::Stone | Self::Empty => MoveRule::Static,
            // 沙与残骸都是粉末：往下掉，按休止角堆积。
            Self::Sand | Self::Remains => MoveRule::Falls,
            Self::Water => MoveRule::Flows,
            // 火和气体一样向上走；火的额外行为（寿命、点燃）在模拟层。
            Self::Fire | Self::Gas => MoveRule::Rises,
        }
    }

    /// 是否会自己移动。空与石不会。
    ///
    /// 注意这和 [`MaterialKind::is_solid`] 问的是两件事：
    /// 残骸在移动意义上会沉降，在阻挡意义上仍然拦生物。
    #[inline]
    pub const fn is_mobile(self) -> bool {
        !matches!(self.move_rule(), MoveRule::Static)
    }

    /// 是否为粉末——靠重力下落、按休止角堆积，**不会横向找平**。
    #[inline]
    pub const fn is_powder(self) -> bool {
        matches!(self.move_rule(), MoveRule::Falls)
    }

    /// `other` 能否挤开 `self` 占据这一格。
    ///
    /// 这是材质层唯一的"谁给谁让路"判定，粉末下坠、液体下沉、气体上浮全都走它，
    /// 避免每个移动分支各写一套比较。规则有两条：
    ///
    /// 1. 让路方只能是**空**或**流体**——固体与粉末靠堆积互相支撑，不让路。
    ///    这条保证了沙丘能立住、石头能当地基。
    /// 2. 挤开方必须比让路方**更重**。这条保证了沙沉进水里、水沉进气里、
    ///    而气体永远顶不开水。
    #[inline]
    pub const fn displaceable_by(self, other: Self) -> bool {
        if !other.is_mobile() {
            return false;
        }
        if !matches!(self, Self::Empty) && !self.is_fluid() {
            return false;
        }
        other.density() > self.density()
    }

    /// 单字符表示，供 ASCII 渲染器与调试输出使用。
    #[inline]
    pub const fn glyph(self) -> char {
        match self {
            Self::Empty => ' ',
            Self::Stone => '#',
            Self::Sand => '.',
            Self::Water => '~',
            Self::Fire => '*',
            Self::Gas => ':',
            Self::Remains => '%',
        }
    }

    /// 稳定短名，供日志与统计使用。
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Stone => "stone",
            Self::Sand => "sand",
            Self::Water => "water",
            Self::Fire => "fire",
            Self::Gas => "gas",
            Self::Remains => "remains",
        }
    }
}

impl fmt::Display for MaterialKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// 移动规则
// ---------------------------------------------------------------------------

/// 材质在重力下的移动方式。
///
/// 做成枚举而不是"每个材质各自一堆布尔标志"，是为了让模拟循环能写成
/// `match rule { ... }` 的四个分支——**规则数量固定，材质数量可以涨**。
/// 加一种新液体时不需要动模拟循环，只把它映射到 `Flows` 即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MoveRule {
    /// 不动：空与石。
    Static,
    /// 粉末：向下落；下方被挡时尝试左下 / 右下。**不横向找平**，于是堆成休止角。
    Falls,
    /// 液体：向下落；下方被挡时尝试左下 / 右下；仍然被挡就向两侧走远一点找平。
    Flows,
    /// 气体 / 火：向上浮；上方被挡时尝试左上 / 右上；再不行就向两侧扩散。
    Rises,
}

// ---------------------------------------------------------------------------
// 反应表
// ---------------------------------------------------------------------------

/// 火在熄灭前能活多少个 tick。
///
/// 火焰必须有寿命，否则它会无限蔓延下去把可燃物烧光、并且永远占据格子。
/// 12 个 tick 在 60 tick/s 下约 0.2 秒，视觉上正好是"一簇火苗烧一下"。
pub const FIRE_LIFETIME: u8 = 12;

/// 点燃可燃物的基础成功率（百分比）。
///
/// 实际成功率 = `IGNITE_BASE_PERCENT × flammability / 100`。
/// 不取 100% 是因为确定性的火焰蔓延会烧出几何感极强的规则形状；
/// 留一点随机反而更像火。
pub const IGNITE_BASE_PERCENT: u32 = 40;

/// 一条反应规则：`a` 与 `b` 相邻时，它们分别变成 `into_a` 与 `into_b`。
///
/// 规则是**无序**的——`(a, b)` 与 `(b, a)` 命中同一条，产物按传入顺序对应返回。
/// 这样写表时不必为每个组合写两遍。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Reaction {
    /// 反应物之一。
    pub a: MaterialKind,
    /// 反应物之二。
    pub b: MaterialKind,
    /// 反应后 `a` 所在格变成什么。
    pub into_a: MaterialKind,
    /// 反应后 `b` 所在格变成什么。
    pub into_b: MaterialKind,
}

impl Reaction {
    /// 构造一条规则。
    #[inline]
    pub const fn new(
        a: MaterialKind,
        b: MaterialKind,
        into_a: MaterialKind,
        into_b: MaterialKind,
    ) -> Self {
        Self {
            a,
            b,
            into_a,
            into_b,
        }
    }

    /// 给定的一对材质是否命中本条规则（无序）。
    ///
    /// 比较的是 `as_u8()` 而不是 `==`：派生的 `PartialEq` 不是 `const`，
    /// 而这张表要在编译期建起来。
    #[inline]
    pub const fn matches(self, x: MaterialKind, y: MaterialKind) -> bool {
        (self.a.as_u8() == x.as_u8() && self.b.as_u8() == y.as_u8())
            || (self.a.as_u8() == y.as_u8() && self.b.as_u8() == x.as_u8())
    }

    /// 按传入顺序给出产物；不命中则返回 `None`。
    #[inline]
    pub const fn outcome(
        self,
        x: MaterialKind,
        y: MaterialKind,
    ) -> Option<(MaterialKind, MaterialKind)> {
        if self.a.as_u8() == x.as_u8() && self.b.as_u8() == y.as_u8() {
            Some((self.into_a, self.into_b))
        } else if self.a.as_u8() == y.as_u8() && self.b.as_u8() == x.as_u8() {
            Some((self.into_b, self.into_a))
        } else {
            None
        }
    }
}

/// 反应表（SPEC 第 2.3 节）。
///
/// ## 为什么必须是表，不能是 `if-else`
///
/// 规则一旦散落进模拟循环，加一种材质就要去循环里补分支，漏一处就是隐蔽的 bug：
/// 表现是"新材质能被烧但不会被浇灭"，而你会去查火的代码，永远查不到。
///
/// 放在表里，模拟循环只问一句"这两格有反应吗"，新增材质只改这一处。
///
/// ## 关于蒸汽与冰
///
/// SPEC 第 2.3 节提过 `火×水 → 蒸汽`、`水×低温 → 冰`。这两条**故意没实现**：
/// 它们需要新增材质种类，而第 2.1 节自己写着"材质种类必须少"。
/// 先让七种材质把生态跑起来，等真的需要相变时再加——那时候加两种材质
/// 也只改这张表，不改循环。
/// 残骸清扫的间隔（tick）。
///
/// 分解**不能**塞进分块访问里：残骸沉到海底之后世界就静止了，静止的块没人访问，
/// 塞在 `visit` 里的分解永远不会发生（实测 2000 tick 一格都没烂）。
/// 所以改成周期性的全局清扫——它不依赖任何块保持活跃。
pub const REMAINS_DECAY_INTERVAL: u64 = 50;

/// 每次清扫时，一格无能量残骸被分解的概率（百分比）。
///
/// 只有 `flags == 0` 的残骸会分解——也就是**饿死**留下的那些。
/// 带着能量的残骸是食物，不该凭空消失；天然碎屑的 `flags` 非零，
/// 所以它们永远稳定。这一条区分是"世界能不能静下来"的关键：
/// worldgen 撒下的碎屑如果也会消失，没有生物的世界就永远在动。
///
/// 20% / 50 tick ⇒ 平均 250 tick 消失。
pub const REMAINS_DECAY_PERCENT: u32 = 20;

pub static REACTIONS: &[Reaction] = &[
    // 火 × 气：气体被点燃，火势顺气团扩散。
    Reaction::new(
        MaterialKind::Fire,
        MaterialKind::Gas,
        MaterialKind::Fire,
        MaterialKind::Fire,
    ),
    // 火 × 残骸：有机物烧起来。
    Reaction::new(
        MaterialKind::Fire,
        MaterialKind::Remains,
        MaterialKind::Fire,
        MaterialKind::Fire,
    ),
    // 火 × 水：火被浇灭，水本身不消耗（没有蒸汽材质，先当它凭空散热）。
    Reaction::new(
        MaterialKind::Fire,
        MaterialKind::Water,
        MaterialKind::Empty,
        MaterialKind::Water,
    ),
];

/// 查询材质对 `a` / `b` 的反应结果，顺序对应传入顺序；无反应返回 `None`。
///
/// 线性扫描——条目只有个位数，比建一张 `7×7` 的二维表更省心，
/// 而且新增材质不需要动表结构。
#[inline]
pub fn react(a: MaterialKind, b: MaterialKind) -> Option<(MaterialKind, MaterialKind)> {
    REACTIONS.iter().find_map(|r| r.outcome(a, b))
}

/// 火尝试点燃邻居 `target` 时的成功率（百分比）。
#[inline]
pub fn ignite_chance(target: MaterialKind) -> u32 {
    IGNITE_BASE_PERCENT * target.flammability() as u32 / 100
}

#[cfg(test)]
mod tests {
    use super::MaterialKind;

    #[test]
    fn u8_roundtrip_is_stable() {
        for k in MaterialKind::ALL {
            assert_eq!(MaterialKind::from_u8(k.as_u8()), k);
        }
    }

    #[test]
    fn unknown_u8_falls_back_to_empty() {
        assert_eq!(MaterialKind::from_u8(200), MaterialKind::Empty);
    }

    #[test]
    fn all_covers_every_variant() {
        // 防止新增材质时忘了同步 ALL / COUNT。
        for v in 0..MaterialKind::COUNT as u8 {
            let k = MaterialKind::from_u8(v);
            assert_eq!(k.as_u8(), v, "variant {v} missing from ALL");
        }
    }

    #[test]
    fn density_ordering_is_sane() {
        assert!(MaterialKind::Gas.density() < MaterialKind::Water.density());
        assert!(MaterialKind::Water.density() < MaterialKind::Sand.density());
        assert!(MaterialKind::Sand.density() < MaterialKind::Stone.density());
    }

    #[test]
    fn solids_block_organisms() {
        assert!(MaterialKind::Stone.blocks_organism());
        assert!(!MaterialKind::Water.blocks_organism());
        assert!(!MaterialKind::Empty.blocks_organism());
    }

    // -----------------------------------------------------------------------
    // 移动规则
    // -----------------------------------------------------------------------

    #[test]
    fn every_material_has_a_move_rule() {
        // 断言的是覆盖面：新增材质时如果忘了归类，这里会红。
        use super::MoveRule;
        for k in MaterialKind::ALL {
            let rule = k.move_rule();
            let expected = match k {
                MaterialKind::Empty | MaterialKind::Stone => MoveRule::Static,
                MaterialKind::Sand | MaterialKind::Remains => MoveRule::Falls,
                MaterialKind::Water => MoveRule::Flows,
                MaterialKind::Fire | MaterialKind::Gas => MoveRule::Rises,
            };
            assert_eq!(rule, expected, "{k} has wrong move rule");
        }
    }

    #[test]
    fn static_materials_are_not_mobile() {
        assert!(!MaterialKind::Empty.is_mobile());
        assert!(!MaterialKind::Stone.is_mobile());
        assert!(MaterialKind::Sand.is_mobile());
        assert!(MaterialKind::Water.is_mobile());
    }

    #[test]
    fn only_sand_and_remains_are_powders() {
        assert!(MaterialKind::Sand.is_powder());
        assert!(MaterialKind::Remains.is_powder());
        assert!(!MaterialKind::Water.is_powder(), "liquid must level out");
        assert!(!MaterialKind::Gas.is_powder());
        assert!(!MaterialKind::Stone.is_powder());
    }

    // -----------------------------------------------------------------------
    // 让路判定
    // -----------------------------------------------------------------------

    #[test]
    fn anything_mobile_falls_into_empty() {
        // 石也"能"进空位，但它从不主动移动，所以这个判定对它没意义；
        // 这里只覆盖真正会动的材质。
        for k in MaterialKind::ALL {
            if !k.is_mobile() {
                continue;
            }
            assert!(
                MaterialKind::Empty.displaceable_by(k),
                "{k} should be able to move into empty space"
            );
        }
    }

    #[test]
    fn sand_sinks_through_water_but_not_the_other_way() {
        assert!(MaterialKind::Water.displaceable_by(MaterialKind::Sand));
        assert!(!MaterialKind::Sand.displaceable_by(MaterialKind::Water));
    }

    #[test]
    fn gas_cannot_sink_through_water() {
        assert!(!MaterialKind::Water.displaceable_by(MaterialKind::Gas));
        // 反过来水可以沉进气体里。
        assert!(MaterialKind::Gas.displaceable_by(MaterialKind::Water));
    }

    #[test]
    fn solids_never_give_way() {
        for other in MaterialKind::ALL {
            assert!(
                !MaterialKind::Stone.displaceable_by(other),
                "stone gave way to {other}"
            );
            assert!(
                !MaterialKind::Sand.displaceable_by(other),
                "sand gave way to {other}"
            );
        }
    }

    #[test]
    fn static_materials_never_push() {
        // 空与石不会主动移动，所以它们不能"挤开"任何东西。
        for target in MaterialKind::ALL {
            assert!(!target.displaceable_by(MaterialKind::Empty));
            assert!(!target.displaceable_by(MaterialKind::Stone));
        }
    }

    // -----------------------------------------------------------------------
    // 反应表
    // -----------------------------------------------------------------------

    #[test]
    fn fire_burns_flammables_into_fire() {
        for fuel in [MaterialKind::Gas, MaterialKind::Remains] {
            assert_eq!(
                super::react(MaterialKind::Fire, fuel),
                Some((MaterialKind::Fire, MaterialKind::Fire)),
                "fire should consume {fuel} into fire"
            );
        }
    }

    #[test]
    fn water_puts_out_fire() {
        assert_eq!(
            super::react(MaterialKind::Water, MaterialKind::Fire),
            Some((MaterialKind::Water, MaterialKind::Empty)),
            "outcome must follow argument order: water stays, fire becomes empty"
        );
    }

    #[test]
    fn reaction_lookup_is_order_independent() {
        let a = super::react(MaterialKind::Fire, MaterialKind::Gas);
        let b = super::react(MaterialKind::Gas, MaterialKind::Fire);
        assert!(a.is_some() && b.is_some());
        // 产物随参数顺序翻转，但"谁变成什么"必须一致。
        assert_eq!(a.unwrap(), (b.unwrap().1, b.unwrap().0));
    }

    #[test]
    fn inert_pairs_do_not_react() {
        assert_eq!(super::react(MaterialKind::Stone, MaterialKind::Water), None);
        assert_eq!(super::react(MaterialKind::Sand, MaterialKind::Sand), None);
        assert_eq!(super::react(MaterialKind::Empty, MaterialKind::Fire), None);
    }

    #[test]
    fn flammability_matches_is_flammable() {
        for k in MaterialKind::ALL {
            assert_eq!(
                k.is_flammable(),
                k.flammability() > 0,
                "{k} disagrees between is_flammable and flammability"
            );
        }
    }

    #[test]
    fn ignite_chance_is_zero_for_inert_materials() {
        assert_eq!(super::ignite_chance(MaterialKind::Stone), 0);
        assert_eq!(super::ignite_chance(MaterialKind::Water), 0);
        assert!(super::ignite_chance(MaterialKind::Gas) > 0);
        assert!(
            super::ignite_chance(MaterialKind::Gas) > super::ignite_chance(MaterialKind::Remains)
        );
    }
}
