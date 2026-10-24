//! `.vivarium` 存档：一个世界的完整快照。
//!
//! ## 为什么是二进制，不是 JSON
//!
//! 512² 的世界有 26 万格材质，加上海底那层藻、几百个有机体各自的
//! 2240 格形态发生规则表——JSON 化之后是几十 MB，而其中 99% 是
//! 同一个值的重复（空材质、空规则槽）。二进制 + 游程编码（RLE）之后：
//!
//! | 世界 | 时刻 | 生物 | 存档 | 逐格裸存材质 |
//! |---|---|---|---|---|
//! | 512² | tick 0 | 17 只 | 101 KB | 512 KB |
//! | 512² | tick 2000 | 623 只 | 1.0 MB | 512 KB |
//!
//! 也就是说：**材质段压了 5 倍，而生物一多，存档的大小就由基因组决定**
//! （每条 300 个权重 = 1200 字节，623 条就是 750 KB）。
//! 想让存档更小，该动的是权重（RLE 一堆 0），不是材质。

//!
//! ## 存什么，不存什么
//!
//! 存的是"这个世界的全部状态"，判据只有一条：**读回来之后继续跑，
//! 必须和从没存过档逐 tick 一致**。所以下面这些看着"像缓存"的东西
//! 也必须存：
//!
//! - **随机数发生器的当前位置**，不是种子。只存种子会回到序列开头，
//!   同一段随机数被用第二次。
//! - **`next_id`**。它进 [`World::hash`]（占格表里存着有机体编号）。
//! - **温度场**。它是积分状态，不是布局的纯函数。
//!
//! 不存的是**每 tick 重建**的东西：`stats`（`step` 开头重置）、
//! `plankton_ids`（每 tick 重建）、`acted` / `last_inputs` / `last_out`
//! （一 tick 的临时量，且 `acted` 必须归零——否则读档后第一批个体
//! 会以为自己这个 tick 已经行动过，白等一 tick）。
//!
//! 光照也不存：它是布局的**纯函数**，读档时按装回去的材质重算一遍就行，
//! 存一份反而给了"和布局对不上"的机会。
//!
//! ## 格式
//!
//! 全部小端、定宽。顺序固定，没有可选段（可选段会让"哪个版本有哪个字段"
//! 变成一张要记住的表）。
//!
//! ```text
//! "VIVARIUM"  8B     魔数
//! u16                版本
//! u16                保留（必须为 0）
//! u32 u32            宽 高
//! u64                tick
//! --- 材质 ---
//! u32                游程数
//! 每段: u16 长度, u8 材质, u8 flags
//! --- 场 ---
//! u16                光源数
//! 每个: i32 x, i32 y, f32 强度
//! u32                温度格数（= ceil(w/4)*ceil(h/4)）
//! 每格: f32
//! --- 账与时钟 ---
//! u64 u64            注入 / 耗散
//! u64 u64            rng state / inc
//! u32                next_id
//! u8                 food_growth
//! --- 初代基因组 ---
//! u8                 有/没有，有则跟一段基因组
//! --- 有机体 ---
//! u32                个数
//! 每个: 见 `put_organism`
//! "ENDVIVAR"  8B     尾标记
//! ```
//!
//! 尾标记不是仪式感：**截断的存档是最常见的坏档**（磁盘满、进程被杀、
//! 手滑复制了一半），而少了它，"读到一半没数据了"和"文件本来就这么长"
//! 无法区分。有它，坏档在打开那一刻就被拒掉，而不是变成一个少了半条鱼的世界。

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::field::LightSource;
use crate::morph::MAX_BODY_CELLS;
use crate::life::{Cell, Facing, Genome, Organism, OrganismId, ENERGY_HIST, INPUTS, OUTPUTS};
use crate::life_sim::{EnergyLedger, LifeSim};
use crate::material::MaterialKind;
use crate::organelle::Organelles;
use crate::world::{MaterialCell, World, MAX_WORLD_SIDE};

/// 文件魔数。
pub const MAGIC: [u8; 8] = *b"VIVARIUM";
/// 尾标记。见模块文档最后一节。
pub const TRAILER: [u8; 8] = *b"ENDVIVAR";
/// 当前格式版本。
pub const VERSION: u16 = 5;
/// 存档扩展名（不带点）。
pub const EXTENSION: &str = "vivarium";

/// 头部字节数。
const HEADER_LEN: usize = 8 + 2 + 2 + 4 + 4 + 8;

/// 存档头部。读档列表只要它，不需要解析整个世界。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaveHeader {
    /// 格式版本。
    pub version: u16,
    /// 世界宽（格）。
    pub width: usize,
    /// 世界高（格）。
    pub height: usize,
    /// 存档那一刻的 tick。
    pub tick: u64,
}

/// 读档失败的原因。
#[derive(Debug)]
pub enum SaveError {
    /// 文件读写失败。
    Io(io::Error),
    /// 不是 `.vivarium`（魔数不对）。
    NotAVivarium,
    /// 版本比这个程序新。老版本读新档**必然**读错，所以要明确拒绝。
    UnsupportedVersion(u16),
    /// 数据不够长。
    Truncated,
    /// 内容自相矛盾（尺寸对不上、RLE 铺不满、尾标记缺失……）。
    Corrupt(&'static str),
    /// 尺寸超出 [`MAX_WORLD_SIDE`]。
    TooLarge { width: usize, height: usize },
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "读写失败：{e}"),
            Self::NotAVivarium => write!(f, "这不是一个 .vivarium 存档"),
            Self::UnsupportedVersion(v) => {
                write!(f, "存档版本 {v} 比这个程序支持的 {VERSION} 新")
            }
            Self::Truncated => write!(f, "存档被截断了"),
            Self::Corrupt(why) => write!(f, "存档内容损坏：{why}"),
            Self::TooLarge { width, height } => write!(
                f,
                "存档里的世界 {width}×{height} 超过上限 {MAX_WORLD_SIDE}"
            ),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for SaveError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// 交给 [`LifeSim::restore`] 的一份完整生命层状态。
///
/// 字段是"存进去之后原样拿出来"的那些，一个不多一个不少。
#[derive(Debug, Clone, Default)]
pub struct LifeSave {
    /// 全部有机体。
    pub organisms: Vec<Organism>,
    /// 下一个要分配的编号。
    pub next_id: OrganismId,
    /// 能量账。
    pub ledger: EnergyLedger,
    /// 随机数发生器内部状态 `(state, inc)`。
    pub rng_state: (u64, u64),
    /// 初代基因组（度量"种群漂了多远"的基准）。
    pub founder: Option<Genome>,
    /// 藻席生长开关。
    pub food_growth: bool,
    /// 粗粒度温度表。
    pub temp: Vec<f32>,
    /// 光源。
    pub sources: Vec<LightSource>,
}

/// 一条存档列表项。界面"读取"那一栏直接吃它。
#[derive(Debug, Clone)]
pub struct SaveInfo {
    /// 完整路径。
    pub path: PathBuf,
    /// 文件名去掉扩展名——界面上显示的名字。
    pub name: String,
    /// 头部信息。
    pub header: SaveHeader,
    /// 文件大小（字节）。
    pub bytes: u64,
    /// 修改时间。取不到时是 `None`（有些文件系统不提供）。
    pub modified: Option<SystemTime>,
}

// -- 编码 -------------------------------------------------------------------

/// 把一个世界编码成 `.vivarium` 字节流。
pub fn encode(world: &World, sim: &LifeSim) -> Vec<u8> {
    let mut out = Out::default();
    out.0.extend_from_slice(&MAGIC);
    out.u16(VERSION);
    out.u16(0);
    out.u32(world.width() as u32);
    out.u32(world.height() as u32);
    out.u64(world.tick());

    put_materials(&mut out, world);

    let field = sim.field();
    out.u16(field.sources().len() as u16);
    for s in field.sources() {
        out.i32(s.x);
        out.i32(s.y);
        out.f32(s.intensity);
    }
    put_f32s(&mut out, field.temp());

    let ledger = sim.ledger();
    out.u64(ledger.injected);
    out.u64(ledger.dissipated);
    let (state, inc) = sim.rng_state();
    out.u64(state);
    out.u64(inc);
    out.u32(sim.next_id());
    out.u8(u8::from(sim.food_growth()));

    match sim.founder() {
        Some(g) => {
            out.u8(1);
            put_genome(&mut out, g);
        }
        None => out.u8(0),
    }

    let organisms = sim.organisms();
    out.u32(organisms.len() as u32);
    for org in organisms {
        put_organism(&mut out, org);
    }

    out.0.extend_from_slice(&TRAILER);
    out.0
}

/// 材质表：游程编码。
///
/// 世界在绝大多数时刻是**大片同质**的（整片空、整片水），
/// 逐格写 2 字节等于把 26 万个 `Empty` 一个一个写进磁盘。
fn put_materials(out: &mut Out, world: &World) {
    let cells = world.cells();
    // 先数一遍段数，才能把段数写在前面。
    let mut runs = 0u32;
    let mut i = 0;
    while i < cells.len() {
        let cur = cells[i];
        let mut j = i + 1;
        while j < cells.len() && cells[j] == cur && j - i < u16::MAX as usize {
            j += 1;
        }
        runs += 1;
        i = j;
    }
    out.u32(runs);
    let mut i = 0;
    while i < cells.len() {
        let cur = cells[i];
        let mut j = i + 1;
        while j < cells.len() && cells[j] == cur && j - i < u16::MAX as usize {
            j += 1;
        }
        out.u16((j - i) as u16);
        out.u8(cur.kind.as_u8());
        out.u8(cur.flags);
        i = j;
    }
}

fn put_f32s(out: &mut Out, values: &[f32]) {
    out.u32(values.len() as u32);
    for v in values {
        out.f32(*v);
    }
}

fn put_genome(out: &mut Out, g: &Genome) {
    out.u16(g.weights.len() as u16);
    for w in &g.weights {
        out.f32(*w);
    }
    put_body(out, &g.body);
}

/// 写身体描述：`max_cells` + `Segment` 树（前序）+ 梯度规则。
///
/// ## 为什么前序而不是层序
///
/// 前序可以在**读到一半就知道下一个是什么**：读一个段、再读它的子段、
/// 子段读完才回来读兄弟。层序要额外存"这一层有几个"，而段树本来就只有
/// 几个节点（深度 ≤ 3、每段子节点 ≤ 3），多存一个计数字段纯属浪费。
///
/// ## 为什么不用 RLE
///
/// 旧的规则表是 `16 × 7 × 5 × 4 = 2240` 格，绝大多数是空的，所以要 RLE。
/// `Body` 的"基因"只有几十个数（段的参数 + 每格的细胞器位 + 几条梯度带），
/// 逐格写就是几十字节——**RLE 的收益还不如它自己的头部开销**。
fn put_body(out: &mut Out, body: &crate::morph::Body) {
    out.u8(body.max_cells);
    put_segment(out, &body.trunk);
    out.u16(body.gradient.bands.len() as u16);
    for band in &body.gradient.bands {
        out.f32(band.along.0);
        out.f32(band.along.1);
        out.f32(band.from_edge.0);
        out.f32(band.from_edge.1);
        out.u16(band.set);
        out.u16(band.clear);
    }
}

fn put_segment(out: &mut Out, seg: &crate::morph::Segment) {
    out.u8(seg.axis);
    out.u8(seg.repeat);
    out.u8(u8::from(seg.mirror));
    out.u16(seg.cells.len() as u16);
    for c in &seg.cells {
        out.i16(c.offset.0);
        out.i16(c.offset.1);
        out.u16(c.base);
    }
    out.u8(seg.children.len() as u8);
    for child in &seg.children {
        put_segment(out, child);
    }
}

fn put_organism(out: &mut Out, org: &Organism) {
    out.u32(org.id);
    out.i32(org.anchor.0);
    out.i32(org.anchor.1);
    out.i32(org.width);
    out.u32(org.energy);
    // v4：生物量。和 `energy` 分开存——它是"身体本身"的价值，
    // 尸体被吃时转移的主要是这一份（见 `life::STRUCTURE_SHARE`）。
    out.u32(org.structure);
    // v5：含水量。它是**积分状态**（离水之后慢慢掉），不是布局的纯函数，
    // 不存的话读档之后每一株植物都会瞬间"满水"，而画面上看不出来。
    out.u8(org.hydration);
    out.u32(org.age);
    out.i8(org.facing.x);
    out.i8(org.facing.y);
    out.u32(org.parent.unwrap_or(0));
    out.u32(org.generation);
    out.u16(org.cells.len() as u16);
    for c in &org.cells {
        // v2：`kind: u8` 换成 `organelles: u16`。
        //
        // 为什么必须换而不是并存：并存就有两份真相，而"哪一份是权威"
        // 会在 M2b 删掉 `kind` 的那一天变成一个必须重新回答的问题。
        // 现在换掉，`primary_cell_type()` 负责给渲染和统计一个代表类型。
        out.u16(c.organelles.0);
        out.i16(c.offset.0);
        out.i16(c.offset.1);
        out.u32(c.age);
        out.u8(u8::from(c.wet));
    }
    // v2：`last_out` 从"检查器用的缓存"变成"网络的递归状态"（`IN_PREV_*` 读它），
    // 所以必须存。不存的话读档后前几 tick 的行为和原件分叉。
    for v in &org.last_out {
        out.f32(*v);
    }
    out.u8(org.stuck);
    out.u8(org.stress);
    put_genome(out, &org.genome);
    for v in &org.energy_hist {
        out.u16(*v);
    }
    out.u8(org.hist_head);
}

// -- 解码 -------------------------------------------------------------------

/// 只解析头部。存档列表用它，不用把整个世界读进来。
pub fn read_header(bytes: &[u8]) -> Result<SaveHeader, SaveError> {
    if bytes.len() < HEADER_LEN {
        return Err(SaveError::Truncated);
    }
    if bytes[0..8] != MAGIC {
        return Err(SaveError::NotAVivarium);
    }
    let mut r = In::new(&bytes[8..]);
    let version = r.u16()?;
    if version > VERSION {
        return Err(SaveError::UnsupportedVersion(version));
    }
    let _reserved = r.u16()?;
    let width = r.u32()? as usize;
    let height = r.u32()? as usize;
    let tick = r.u64()?;
    if width == 0 || height == 0 || width > MAX_WORLD_SIDE || height > MAX_WORLD_SIDE {
        return Err(SaveError::TooLarge { width, height });
    }
    Ok(SaveHeader {
        version,
        width,
        height,
        tick,
    })
}

/// 解码成一个可直接继续跑的世界。
pub fn decode(bytes: &[u8]) -> Result<(World, LifeSim), SaveError> {
    let header = read_header(bytes)?;
    let mut world = World::new(header.width, header.height)
        .map_err(|_| SaveError::TooLarge {
            width: header.width,
            height: header.height,
        })?;
    world.set_tick(header.tick);

    let mut r = In::new(&bytes[HEADER_LEN..]);

    // 材质
    let cells = read_materials(&mut r, header.width * header.height)?;
    if !world.restore_cells(&cells) {
        return Err(SaveError::Corrupt("材质表长度对不上世界"));
    }

    // 场
    let source_count = r.u16()? as usize;
    let mut sources = Vec::with_capacity(source_count.min(crate::field::SOURCE_CAP));
    for _ in 0..source_count {
        sources.push(LightSource {
            x: r.i32()?,
            y: r.i32()?,
            intensity: r.f32()?,
        });
    }
    let temp_len = r.u32()? as usize;
    let expected_temp = header.width.div_ceil(crate::field::AIR_CELL)
        * header.height.div_ceil(crate::field::AIR_CELL);
    if temp_len != expected_temp {
        return Err(SaveError::Corrupt("温度表长度对不上世界"));
    }
    let mut temp = Vec::with_capacity(temp_len);
    for _ in 0..temp_len {
        temp.push(r.f32()?);
    }

    // 账与时钟
    let ledger = EnergyLedger {
        injected: r.u64()?,
        dissipated: r.u64()?,
    };
    let rng_state = (r.u64()?, r.u64()?);
    let next_id = r.u32()?;
    let food_growth = r.u8()? != 0;

    // 初代基因组
    let founder = if r.u8()? != 0 {
        Some(read_genome(&mut r)?)
    } else {
        None
    };

    // 有机体
    let count = r.u32()? as usize;
    let mut organisms = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        organisms.push(read_organism(&mut r, header.width as i32)?);
    }

    // 尾标记
    let tail = r.take(8)?;
    if tail != TRAILER {
        return Err(SaveError::Corrupt("尾标记缺失，文件可能被截断"));
    }

    let save = LifeSave {
        organisms,
        next_id,
        ledger,
        rng_state,
        founder,
        food_growth,
        temp,
        sources,
    };
    let sim = LifeSim::restore(&mut world, save);
    Ok((world, sim))
}

fn read_materials(r: &mut In<'_>, total: usize) -> Result<Vec<MaterialCell>, SaveError> {
    let runs = r.u32()? as usize;
    let mut cells = vec![MaterialCell::default(); total];
    let mut i = 0usize;
    for _ in 0..runs {
        let len = r.u16()? as usize;
        let kind = MaterialKind::from_u8(r.u8()?);
        let flags = r.u8()?;
        if i + len > total {
            return Err(SaveError::Corrupt("材质游程超出世界大小"));
        }
        for c in &mut cells[i..i + len] {
            *c = MaterialCell { kind, flags };
        }
        i += len;
    }
    if i != total {
        return Err(SaveError::Corrupt("材质游程没铺满世界"));
    }
    Ok(cells)
}

fn read_genome(r: &mut In<'_>) -> Result<Genome, SaveError> {
    let n = r.u16()? as usize;
    if n > crate::life::WEIGHT_COUNT {
        return Err(SaveError::Corrupt("权重个数超过网络规模"));
    }
    let mut weights = vec![0.0f32; crate::life::WEIGHT_COUNT];
    for w in weights.iter_mut().take(n) {
        *w = r.f32()?;
    }
    let body = read_body(r)?;
    Ok(Genome { weights, body })
}

fn read_body(r: &mut In<'_>) -> Result<crate::morph::Body, SaveError> {
    let max_cells = r.u8()?;
    if max_cells == 0 || max_cells > crate::morph::MAX_BODY_CELLS {
        return Err(SaveError::Corrupt("体型上限越界"));
    }
    let trunk = read_segment(r, 0)?;
    let n = r.u16()? as usize;
    if n > crate::morph::MAX_BANDS {
        return Err(SaveError::Corrupt("梯度带太多"));
    }
    let mut bands = Vec::with_capacity(n);
    for _ in 0..n {
        bands.push(crate::morph::Band {
            along: (r.f32()?, r.f32()?),
            from_edge: (r.f32()?, r.f32()?),
            set: r.u16()?,
            clear: r.u16()?,
        });
    }
    Ok(crate::morph::Body {
        trunk,
        gradient: crate::morph::Gradient { bands },
        max_cells,
    })
}

fn read_segment(r: &mut In<'_>, depth: u8) -> Result<crate::morph::Segment, SaveError> {
    if depth > crate::morph::MAX_SEGMENT_DEPTH {
        return Err(SaveError::Corrupt("段嵌套过深"));
    }
    let axis = r.u8()?;
    if axis > 3 {
        return Err(SaveError::Corrupt("主轴越界"));
    }
    let repeat = r.u8()?;
    let mirror = r.u8()? != 0;
    let n = r.u16()? as usize;
    if n > crate::morph::MAX_BODY_CELLS as usize {
        return Err(SaveError::Corrupt("段内格数超过上限"));
    }
    let mut cells = Vec::with_capacity(n);
    for _ in 0..n {
        cells.push(crate::morph::BodyCell {
            offset: (r.i16()?, r.i16()?),
            base: r.u16()?,
        });
    }
    let cn = r.u8()? as usize;
    if cn > 3 {
        return Err(SaveError::Corrupt("子段太多"));
    }
    let mut children = Vec::with_capacity(cn);
    for _ in 0..cn {
        children.push(read_segment(r, depth + 1)?);
    }
    Ok(crate::morph::Segment {
        cells,
        axis,
        repeat,
        mirror,
        children,
    })
}

fn read_organism(r: &mut In<'_>, world_width: i32) -> Result<Organism, SaveError> {
    let id = r.u32()?;
    let anchor = (r.i32()?, r.i32()?);
    // 存档里记着 width，但**不信任它**：世界可能被换过尺寸（将来的"调整大小"），
    // 而 `cell_at` 的环绕取模直接拿它当模数。用当前世界的宽度覆盖，
    // 是"读档之后身体还在同一处"的前提。
    let _saved_width = r.i32()?;
    let energy = r.u32()?;
    let structure = r.u32()?;
    let hydration = r.u8()?;
    let age = r.u32()?;
    let facing = Facing {
        x: r.i8()?,
        y: r.i8()?,
    };
    let parent = match r.u32()? {
        0 => None,
        p => Some(p),
    };
    let generation = r.u32()?;
    let cell_count = r.u16()? as usize;
    if cell_count > MAX_BODY_CELLS as usize {
        return Err(SaveError::Corrupt("身体格数超过上限"));
    }
    let mut cells = Vec::with_capacity(cell_count);
    for _ in 0..cell_count {
        let organelles = Organelles(r.u16()?);
        cells.push(Cell {
            // `kind` 是过渡期字段，由细胞器反推。因为 `from_cell_type`
            // 是一对一的，往返精确——但**只有从 `Cell::new` 造出来的细胞**
            // 才保证这一点。组合细胞（叶绿体 + 气囊）的代表类型是有损的，
            // 那是 M2b 之后才需要考虑的事。
            kind: organelles.primary_cell_type(),
            organelles,
            offset: (r.i16()?, r.i16()?),
            age: r.u32()?,
            wet: r.u8()? != 0,
        });
    }
    let mut last_out = [0.0f32; OUTPUTS];
    for v in last_out.iter_mut() {
        *v = r.f32()?;
    }
    let stuck = r.u8()?;
    let stress = r.u8()?;
    let genome = read_genome(r)?;
    let mut energy_hist = [0u16; ENERGY_HIST];
    for v in energy_hist.iter_mut() {
        *v = r.u16()?;
    }
    let hist_head = r.u8()?;
    Ok(Organism {
        id,
        anchor,
        width: world_width,
        cells,
        energy,
        structure,
        hydration,
        age,
        facing,
        genome,
        parent,
        generation,
        // 一 tick 的临时量，见模块文档"不存什么"。
        acted: false,
        last_inputs: [0.0; INPUTS],
        // `last_out` **不再是**临时量：它是 `IN_PREV_*` 的来源。
        last_out,
        energy_hist,
        hist_head,
        stuck,
        stress,
    })
}

// -- 文件 -------------------------------------------------------------------

/// 存档目录：程序目录下的 `saves/`。
///
/// 和 1.x 的"导出落程序目录"是同一条惯例——玩家不该为了找到自己的存档
/// 去翻 `%APPDATA%` 下面那串哈希目录名。
pub fn saves_dir() -> PathBuf {
    let base = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("saves")
}

/// 把世界写进 `path`。
///
/// 先写 `.tmp` 再改名：写一半被中断（磁盘满、进程被杀）留下的半截文件，
/// 如果直接顶着正式文件名，下次读档会得到一个"少了半条鱼"的世界，
/// 而玩家以为那是自己的存档坏了。改名在同一个目录里是原子的。
pub fn save_to_file(path: &Path, world: &World, sim: &LifeSim) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let bytes = encode(world, sim);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// 从 `path` 读一个世界。
pub fn load_from_file(path: &Path) -> Result<(World, LifeSim), SaveError> {
    let bytes = fs::read(path)?;
    decode(&bytes)
}

/// 扫一遍存档目录，按修改时间**从新到旧**排。
///
/// 新到旧是刻意的：玩家最常打开的就是刚存的那个。按名字排会把
/// `world-20261022` 排到 `world-2026` 前面，而那不是任何人想要的顺序。
///
/// 目录不存在时返回空表（而不是错误）：第一次打开"读取"面板时
/// 目录本来就不存在，那不是故障。
pub fn scan_dir(dir: &Path) -> Vec<SaveInfo> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        // 只读前 32 字节：列表要的只有尺寸和 tick，把整个世界读进来
        // 只为显示一行文字，在 2000² 的档上是几百毫秒 × N 个文件。
        let Ok(mut file) = fs::File::open(&path) else {
            continue;
        };
        let mut head = [0u8; 32];
        let mut got = 0;
        while got < head.len() {
            match file.read(&mut head[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => break,
            }
        }
        let Ok(header) = read_header(&head[..got]) else {
            // 坏档不进列表：一行点开就报错的条目，不如让它彻底消失，
            // 玩家至少不会以为"我存的东西还在"。
            continue;
        };
        out.push(SaveInfo {
            name: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string(),
            path,
            header,
            bytes: meta.len(),
            modified: meta.modified().ok(),
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.name.cmp(&b.name)));
    out
}

// -- 字节读写 ---------------------------------------------------------------

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn i8(&mut self, v: i8) {
        self.0.push(v as u8);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

struct In<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> In<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], SaveError> {
        let end = self.pos.checked_add(n).ok_or(SaveError::Truncated)?;
        if end > self.buf.len() {
            return Err(SaveError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, SaveError> {
        Ok(self.take(1)?[0])
    }
    fn i8(&mut self) -> Result<i8, SaveError> {
        Ok(self.u8()? as i8)
    }
    fn u16(&mut self) -> Result<u16, SaveError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn i16(&mut self) -> Result<i16, SaveError> {
        Ok(self.u16()? as i16)
    }
    fn u32(&mut self) -> Result<u32, SaveError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i32(&mut self) -> Result<i32, SaveError> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> Result<u64, SaveError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn f32(&mut self) -> Result<f32, SaveError> {
        Ok(f32::from_bits(self.u32()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::life_sim::stock_surface;
    use crate::rng::Rng;
    use crate::sim::MaterialSim;
    use crate::worldgen::sea_and_islands;

    /// 一个有地形、有鱼、有灯、跑过一阵的世界。
    ///
    /// 读档测试要的是"什么都有"，而不是"只有一格水"——漏存某个字段的
    /// 症状往往只在有生物时才显形。
    fn sample() -> (World, LifeSim) {
        let mut world = World::new(120, 90).unwrap();
        let mut rng = Rng::new(7);
        sea_and_islands(&mut world, &mut rng);
        let mut sim = LifeSim::new(&mut world);
        stock_surface(&mut world, &mut sim, 6, 400, true);
        sim.field_mut().add_source(30, 10);
        let mut mat = MaterialSim::new();
        mat.set_threads(1);
        for _ in 0..200 {
            mat.step(&mut world);
            sim.step(&mut world);
        }
        // 存档点两侧的光场必须一致：光不进存档（它是布局的纯函数），
        // 读档时会按材质重算一遍。这里先在原件上也刷一次，
        // 否则读回来的那份"更新鲜"，两边从第一 tick 就开始分叉。
        sim.refresh_field(&world);
        (world, sim)
    }

    fn run(world: &mut World, sim: &mut LifeSim, mat: &mut MaterialSim, ticks: usize) {
        for _ in 0..ticks {
            mat.step(world);
            sim.step(world);
        }
    }

    #[test]
    fn round_trip_preserves_every_field_that_matters() {
        let (world, sim) = sample();
        let bytes = encode(&world, &sim);
        let (w2, s2) = decode(&bytes).expect("刚写出来的档读不回来");

        assert_eq!(w2.width(), world.width());
        assert_eq!(w2.height(), world.height());
        assert_eq!(w2.tick(), world.tick());
        // 占格表也在哈希里，所以这一条同时钉住了"细胞归谁、是第几格、什么类型"。
        assert_eq!(w2.hash(), world.hash(), "材质或占格表没原样回来");
        assert_eq!(s2.organisms().len(), sim.organisms().len());
        assert_eq!(s2.ledger(), sim.ledger());
        assert_eq!(s2.next_id(), sim.next_id());
        assert_eq!(s2.rng_state(), sim.rng_state());
        assert_eq!(s2.food_growth(), sim.food_growth());
        assert_eq!(s2.field().sources().len(), sim.field().sources().len());
        assert_eq!(s2.field().temp(), sim.field().temp(), "温度场没原样回来");
        assert!(s2.founder().is_some(), "初代基因组丢了");
    }

    #[test]
    fn every_organism_survives_the_round_trip_cell_for_cell() {
        let (world, sim) = sample();
        let bytes = encode(&world, &sim);
        let (_, s2) = decode(&bytes).unwrap();

        for (a, b) in sim.organisms().iter().zip(s2.organisms()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.anchor, b.anchor);
            assert_eq!(a.energy, b.energy);
            // 生物量也要原样回来：它决定尸体被吃时转移多少能量，
            // 丢了它读档后的食腐收益会静默变少。
            assert_eq!(a.structure, b.structure, "生物量没回来");
            assert_eq!(a.age, b.age);
            assert_eq!(a.facing, b.facing);
            assert_eq!(a.parent, b.parent);
            assert_eq!(a.generation, b.generation);
            assert_eq!(a.hist_head, b.hist_head);
            assert_eq!(a.energy_hist, b.energy_hist);
            // v2：`last_out` 从"检查器用的缓存"变成"网络的递归状态"
            // （`IN_PREV_*` 读它），所以必须原样回来——否则读档后
            // 前几 tick 的行为和原件分叉。
            assert_eq!(a.last_out, b.last_out, "递归状态丢了");
            // v2：`stuck` 是 `IN_STUCK` 的来源，同样必须存。
            assert_eq!(a.stuck, b.stuck, "卡住的计数丢了");
            assert_eq!(a.cells.len(), b.cells.len());
            for (ca, cb) in a.cells.iter().zip(&b.cells) {
                assert_eq!(ca.kind, cb.kind);
                assert_eq!(ca.offset, cb.offset);
                assert_eq!(ca.age, cb.age);
                // `wet` 是"出生那一刻借了几格水"的账。丢了它，读档之后
                // 每一具尸体都会按"此刻泡不泡水"重新算，水位就开始漂
                // ——而画面看不出任何异常。
                assert_eq!(ca.wet, cb.wet, "湿债务丢了");
            }
            assert_eq!(a.genome.weights, b.genome.weights);
            assert_eq!(a.genome.body, b.genome.body, "身体描述没原样回来");
        }
    }

    #[test]
    fn a_loaded_world_keeps_running_identically() {
        let (mut world, mut sim) = sample();
        let bytes = encode(&world, &sim);

        // 两边都从"每一块都脏"出发。读档之后本来就全脏（材质被整体换掉了），
        // 而原件此刻是"部分块干净"——那不是存档丢了什么，而是材质层的
        // 跳块优化本来就有的等价性缺口（见 `sim.rs` 里 `DIRTY_HALO_X` 的注释）。
        // 这个测试要钉的是**生命层的状态有没有全部带回来**，所以把两边
        // 拉到同一个起点，而不是让一个已知缺口来干扰它。
        world.mark_all_dirty();

        let mut mat = MaterialSim::new();
        mat.set_threads(1);
        run(&mut world, &mut sim, &mut mat, 200);

        let (mut w2, mut s2) = decode(&bytes).unwrap();
        let mut mat2 = MaterialSim::new();
        mat2.set_threads(1);
        run(&mut w2, &mut s2, &mut mat2, 200);

        assert_eq!(w2.tick(), world.tick());
        assert_eq!(w2.hash(), world.hash(), "读档后的世界跑出了另一个结果");
        assert_eq!(s2.organisms().len(), sim.organisms().len());
        assert_eq!(s2.ledger(), sim.ledger(), "能量账分叉了");
        assert_eq!(s2.next_id(), sim.next_id());
    }

    #[test]
    fn materials_compress_because_the_world_is_lumpy() {
        // 只看材质段：这个样本里**故意没有生物**。有生物的话存档的
        // 绝大部分是基因组（每条 300 个权重 + 一张 2240 格的规则表），
        // 拿它去和"裸存材质"比大小，比的是两件不同的东西。
        let mut world = World::new(160, 120).unwrap();
        let mut rng = Rng::new(3);
        sea_and_islands(&mut world, &mut rng);
        let sim = LifeSim::new(&mut world);
        assert!(sim.organisms().is_empty());

        let bytes = encode(&world, &sim);
        // 逐格写是 2 字节 × 格数。RLE 之后必须明显更小，否则这套格式
        // 就只是在手写一个更脆的 `Vec<MaterialCell>` 转储。
        let raw = world.width() * world.height() * 2;
        assert!(
            bytes.len() * 2 < raw,
            "存档 {} 字节，逐格裸存 {} 字节，压缩没起作用",
            bytes.len(),
            raw
        );
    }

    #[test]
    fn rejects_things_that_are_not_archives() {
        assert!(matches!(
            decode(b"not a vivarium"),
            Err(SaveError::Truncated)
        ));

        let (world, sim) = sample();
        let mut bytes = encode(&world, &sim);
        bytes[0] = b'X';
        assert!(matches!(decode(&bytes), Err(SaveError::NotAVivarium)));

        // 版本比程序新：明确拒绝，而不是读出一堆乱码。
        let mut bytes = encode(&world, &sim);
        bytes[8] = (VERSION + 1) as u8;
        assert!(matches!(
            decode(&bytes),
            Err(SaveError::UnsupportedVersion(_))
        ));

        // 截断：尾标记缺失。这是最常见的坏档（磁盘满、进程被杀）。
        let mut bytes = encode(&world, &sim);
        bytes.truncate(bytes.len() - 4);
        assert!(decode(&bytes).is_err());

        // 尾标记被改：同样拒掉。
        let mut bytes = encode(&world, &sim);
        let n = bytes.len();
        bytes[n - 1] = b'?';
        assert!(matches!(decode(&bytes), Err(SaveError::Corrupt(_))));
    }

    #[test]
    fn a_truncated_archive_fails_before_it_allocates() {
        // 一个坏档可以把"温度格数"写成 40 亿。这里钉住的是
        // **先校验长度、再分配**，而不是让 `Vec::with_capacity` 去 OOM。
        let (world, sim) = sample();
        let mut bytes = encode(&world, &sim);
        bytes.truncate(HEADER_LEN + 4);
        assert!(decode(&bytes).is_err());
    }

    #[test]
    fn scan_dir_lists_archives_and_skips_junk() {
        let dir = std::env::temp_dir().join(format!(
            "vivarium-saves-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let (world, sim) = sample();
        save_to_file(&dir.join("alpha.vivarium"), &world, &sim).unwrap();
        save_to_file(&dir.join("beta.vivarium"), &world, &sim).unwrap();
        // 不是存档的东西不该出现在列表里。
        fs::write(dir.join("notes.txt"), b"hello").unwrap();
        fs::write(dir.join("broken.vivarium"), b"garbage").unwrap();

        let list = scan_dir(&dir);
        assert_eq!(list.len(), 2, "列表里混进了非存档：{list:?}");
        assert!(list.iter().all(|i| i.header.tick == world.tick()));
        assert!(list.iter().all(|i| i.header.width == world.width()));
        assert!(list.iter().all(|i| i.bytes > 0));
        let names: Vec<&str> = list.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta"));

        // 不存在的目录返回空表而不是报错：第一次打开"读取"面板时它本来就不在。
        assert!(scan_dir(&dir.join("nope")).is_empty());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn header_only_reads_do_not_need_the_whole_file() {
        let (world, sim) = sample();
        let bytes = encode(&world, &sim);
        // 列表只读前 32 字节，所以头部必须整个落在这个范围里。
        let head = read_header(&bytes[..32]).unwrap();
        assert_eq!(head.width, world.width());
        assert_eq!(head.height, world.height());
        assert_eq!(head.tick, world.tick());
        assert_eq!(head.version, VERSION);
    }
}
