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
        matches!(self, Self::Gas | Self::Remains)
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
}
