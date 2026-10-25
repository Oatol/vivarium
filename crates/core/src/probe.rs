//! 悬停读数：**这一格有什么值得看的**。
//!
//! ## 为什么要有这个模块
//!
//! 悬停读数原来分在三处：`CellInfo` 定义字段、主循环填值、界面排版成几行字。
//! 加一项要改三处，而且**漏掉一处不会编译报错**——它只是不显示。于是
//! "界面上看不到新信息"变成了一件必须靠记性才能避免的事。
//!
//! 现在只有**一处**：这个文件。界面拿到的是一张
//! `Vec<Group>`，它只负责把 `Value` 排版成字符串，不认识任何一个具体的量。
//!
//! ## "自动"的边界
//!
//! 说清楚比较好：**这里买到的是"只写一处"，不是"不用写"。**
//! 没有人能自动知道哪个数值得显示——真正的反射式转储会把 398 个权重
//! 一起倒出来。所以新增一个概念仍然要在这里加一行 `push`；
//! 但那一行加完就一定会显示，界面不可能忘、也不可能显示错位。
//!
//! ## 分组
//!
//! 三组：**格**（坐标、深度、材质）、**场**（光、温度、湿度）、
//! **生物**（只有这一格属于某个个体时才有）。分组不是为了好看——
//! 是让"这条读数和上一条是不是同一个层次"一眼能看出来，
//! 而混在一起的 11 行里没人分得清"湿度 12"是环境还是生物自己的。

use crate::life::CellType;
use crate::life_sim::LifeSim;
use crate::{MaterialKind, World};

/// 一个探针的值。
///
/// 刻意做成**封闭枚举**而不是 `String`：格式化的决定权在这里，
/// 而界面只负责"Int 怎么画、Ratio 怎么画"。于是"能量显示成 1234/5000"
/// 这种格式不会在界面的三个地方各写一份。
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// 整数。
    Int(i64),
    /// 浮点，保留 `digits` 位，后面跟 `unit`。
    ///
    /// 单位放在这里而不是拼进字符串，是因为**界面要按数值本身取色**
    /// （光照越亮越绿、温度越高越红、湿度越高越蓝）。一旦先拼成
    /// `"20 °C"`，取色那边就得去解析字符串——那种"先格式化再解析"
    /// 的往返是这类代码最常见的坏味道。
    Float {
        /// 数值。
        v: f32,
        /// 小数位。
        digits: usize,
        /// 单位（可以为空）。
        unit: &'static str,
    },
    /// 比值，画成 `a/b`。
    Ratio {
        /// 分子。
        a: u32,
        /// 分母。
        b: u32,
    },
    /// 一段文字（材质名、细胞器清单）。
    Text(String),
    /// 是/否。
    Flag(bool),
    /// 这一项不适用于当前这一格（不画这一行）。
    Missing,
}

/// 值的"语气"——界面据此挑颜色，而**颜色本身住在界面里**。
///
/// 这一层是必要的：`core` 不该知道 `Color32` 是什么（它不依赖 egui），
/// 但它确实知道"这个数是危险值"或"这是生物"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tint {
    /// 标题、次要说明。
    Faint,
    /// 普通数值。
    Normal,
    /// 补充说明。
    Dim,
    /// 强调（编号、物种）。
    Accent,
    /// 警告（快饿死了、完全脱水）。
    Danger,
    /// 按数值本身取色（光、温度、湿度各有一条色带）。
    Light,
    /// 温度色带。
    Temp,
    /// 湿度色带。
    Moisture,
}

/// 一行读数。
#[derive(Debug, Clone, PartialEq)]
pub struct Probe {
    /// 左边的标签（"光照"、"能量"）。空字符串表示这是一整行文字。
    pub label: &'static str,
    /// 右边的值。
    pub value: Value,
    /// 颜色语气。
    pub tint: Tint,
}

impl Probe {
    fn new(label: &'static str, value: Value, tint: Tint) -> Self {
        Self { label, value, tint }
    }
}

/// 一组读数（"格" / "场" / "生物"）。
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// 组标题。
    pub title: &'static str,
    /// 这一组的行。
    pub probes: Vec<Probe>,
}

/// 这一格的全部读数。
///
/// **唯一的"什么值得看"的地方。** 新增概念在这里加一行。
pub fn cell_probes(world: &World, sim: &LifeSim, x: i32, y: i32) -> Vec<Group> {
    let (xs, ys) = (x as isize, y as isize);
    let material = world.get(xs, ys);
    let owner = world.owner(xs, ys);

    // --- 格 ---
    //
    // 深度 = 这一格在**海平面**上下多少格。用湿度场记下来的海平面，
    // 而不是"这一列最上面那格水"——后者在纯陆地列上不存在，
    // 于是同一片大陆上有的地方有深度、有的地方没有。
    let sea = sim.moisture().sea_level();
    let depth = if sim.moisture().is_empty() {
        0
    } else {
        y - sea
    };
    let mut grid = vec![
        Probe::new(
            "坐标",
            Value::Text(format!("{x}, {y}")),
            Tint::Normal,
        ),
        Probe::new(
            "深度",
            Value::Text(format!("{depth:+} 格")),
            Tint::Dim,
        ),
        Probe::new(
            "材质",
            Value::Text(material.name().to_owned()),
            Tint::Normal,
        ),
    ];
    // 碎屑/尸体带能量——那正是"能吃多少"。
    if matches!(material, MaterialKind::Remains | MaterialKind::Corpse) {
        grid.push(Probe::new(
            "残骸能量",
            Value::Int(world.flags(xs, ys) as i64),
            Tint::Normal,
        ));
    }

    // --- 场 ---
    let light = sim.field().light_at(x as f32, y as f32);
    let temp = sim.field().temp_at(x as f32, y as f32);
    let moisture = sim.moisture_at(x, y);
    let mut field = vec![
        Probe::new(
            "光照",
            Value::Float {
                v: light,
                digits: 2,
                unit: "",
            },
            Tint::Light,
        ),
        Probe::new(
            "温度",
            Value::Float {
                v: temp,
                digits: 0,
                unit: " °C",
            },
            Tint::Temp,
        ),
        Probe::new(
            "湿度",
            Value::Ratio {
                a: moisture as u32,
                b: 255,
            },
            Tint::Moisture,
        ),
    ];
    // 这一格是不是水：水的"湿度"恒为满值，说清楚比让玩家自己猜好。
    if material == MaterialKind::Water {
        field.push(Probe::new(
            "水体",
            Value::Flag(true),
            Tint::Dim,
        ));
    }

    let mut out = vec![
        Group {
            title: "格",
            probes: grid,
        },
        Group {
            title: "场",
            probes: field,
        },
    ];

    // --- 生物 ---
    if owner.organism != 0 {
        if let Some(org) = sim.organism(owner.organism) {
            out.push(bio_group(org, x, y));
        }
    }
    out
}

/// 生物那一组。单独拆出来只为了 `cell_probes` 别太长。
fn bio_group(org: &crate::life::Organism, x: i32, y: i32) -> Group {
    let organelles = org
        .cells
        .iter()
        .find(|c| org.cell_at(c) == (x, y))
        .map(|c| c.organelles)
        .unwrap_or_else(|| org.cells[0].organelles);

    // 这一格的细胞器清单。**按位掩码念，不按"代表类型"念**——
    // 工坊能拼出"叶绿体 + 气囊"，那个组合没有名字。
    let kinds = organelle_list(organelles);
    let roots = org
        .cells
        .iter()
        .filter(|c| c.organelles.has(crate::organelle::ROOT))
        .count();
    let producers = org.count(CellType::Producer);

    let mut probes = vec![
        Probe::new(
            "个体",
            Value::Text(format!(
                "#{} · {} 格 · 代 {}",
                org.id,
                org.cells.len(),
                org.generation
            )),
            Tint::Accent,
        ),
        Probe::new("细胞", Value::Text(kinds), Tint::Normal),
        Probe::new(
            "能量",
            Value::Ratio {
                a: org.energy,
                b: org.max_energy(),
            },
            if org.energy * 4 < org.max_energy() {
                Tint::Danger
            } else {
                Tint::Normal
            },
        ),
        Probe::new(
            "生物量",
            Value::Ratio {
                a: org.structure,
                b: org.max_structure(),
            },
            Tint::Dim,
        ),
        Probe::new(
            "含水量",
            Value::Ratio {
                a: org.hydration as u32,
                b: crate::life::HYDRATION_MAX as u32,
            },
            if org.hydration == 0 {
                Tint::Danger
            } else {
                Tint::Dim
            },
        ),
        Probe::new(
            "生产者",
            Value::Text(if roots > 0 {
                format!("{producers} 格 · 根 {roots}")
            } else {
                format!("{producers} 格")
            }),
            Tint::Dim,
        ),
        Probe::new("年龄", Value::Int(org.age as i64), Tint::Dim),
    ];
    // 状态行：把"它现在有什么毛病"合成一句话，比在 7 行数字里找更快。
    if let Some(state) = org_state(org) {
        probes.push(Probe::new("", Value::Text(state), Tint::Danger));
    }
    Group {
        title: "生物",
        probes,
    }
}

/// 一句话概括"它现在有什么问题"。没有问题时返回 `None`。
fn org_state(org: &crate::life::Organism) -> Option<String> {
    let mut bad = Vec::new();
    if org.hydration == 0 && org.count(CellType::Producer) > 0 {
        bad.push("脱水（光合已停）");
    }
    if org.energy < org.upkeep() * 2 {
        bad.push("快饿死");
    }
    if bad.is_empty() {
        None
    } else {
        Some(bad.join(" · "))
    }
}

/// 细胞器位掩码 → "叶绿体 ×2 + 气囊" 这样的短标签。
///
/// 和工坊里那份（`workshop::organelle_list`）是同一套念法，但**不能共用**：
/// 那一份在 `vivarium-ui` 里，而 `core` 不能依赖界面。两份都短，
/// 漂移的代价只是"两边用词不同"，不影响任何判据。
pub fn organelle_list(o: crate::organelle::Organelles) -> String {
    use crate::organelle::{
        ARMOR, BUILDER, CHLORO_SHIFT, DIGEST, EYE, FLAGELLUM_SHIFT, GAS, MITO, STING_SHIFT,
    };
    let mut parts: Vec<String> = Vec::new();
    for (shift, name) in [
        (CHLORO_SHIFT, "叶绿体"),
        (FLAGELLUM_SHIFT, "鞭毛"),
        (STING_SHIFT, "刺"),
    ] {
        match o.level(shift) {
            0 => {}
            1 => parts.push(name.to_string()),
            n => parts.push(format!("{name} ×{n}")),
        }
    }
    for (flag, name) in [
        (DIGEST, "嘴"),
        (ARMOR, "装甲"),
        (GAS, "气囊"),
        (EYE, "眼"),
        (MITO, "线粒体"),
        (BUILDER, "建造"),
        (crate::organelle::ROOT, "根"),
    ] {
        if o.has(flag) {
            parts.push(name.to_string());
        }
    }
    if parts.is_empty() {
        "空细胞".to_string()
    } else {
        parts.join(" + ")
    }
}

/// 悬停读数的完整快照。
///
/// 只是 `Vec<Group>` 的别名——界面要的是"一张能遍历的表"，
/// 不需要知道它是怎么来的。
pub type Hover = Vec<Group>;
