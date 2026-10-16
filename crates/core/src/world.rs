//! 世界网格：分块 + 脏标记 + 确定性哈希。
//!
//! ## 性能认知（SPEC 第 2.4 节）
//!
//! 落沙层的成本取决于**活跃格子比例**，不是网格大小。
//! 1024² 里如果只有 3% 的格子是动的，成本约等于 256² 全动。
//! 所以"大网格 + 稀疏活跃"是可行的——**前提是保证大部分区域真的静止**。
//! 分块脏标记就是为这个前提服务的：一块内所有格子一个 tick 都没变，下一 tick 整块跳过。
//!
//! ## 确定性
//!
//! [`World::hash`] 给出网格的确定性指纹。SPEC 的验收断言依赖它：
//! - 同种子同 tick ⇒ 哈希一致
//! - 切换 Pixel / Ascii 渲染器后 ⇒ 哈希不变

use std::fmt;

use crate::material::MaterialKind;

/// 分块边长。与 Noita 一致——这个尺寸在缓存局部性与调度粒度之间平衡得最好。
pub const CHUNK_SIZE: usize = 64;

/// 单格数据。
///
/// 保持 2 字节：`kind` 是 `repr(u8)` 枚举，`flags` 预留给"本 tick 已移动过"之类的标记。
/// 512² 的世界因此只占 512 KiB，整个网格能舒服地待在 L2 缓存附近。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct MaterialCell {
    /// 材质类型。
    pub kind: MaterialKind,
    /// 位标记，当前未使用。
    pub flags: u8,
}

impl MaterialCell {
    /// 新建一个只有材质类型的格子。
    #[inline]
    pub const fn new(kind: MaterialKind) -> Self {
        Self { kind, flags: 0 }
    }
}

/// 世界构造失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldError {
    /// 尺寸为 0。
    ZeroSize,
    /// 尺寸不是 [`CHUNK_SIZE`] 的整数倍。
    NotChunkAligned { width: usize, height: usize },
}

impl fmt::Display for WorldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroSize => write!(f, "world size must be non-zero"),
            Self::NotChunkAligned { width, height } => write!(
                f,
                "world size {width}x{height} must be a multiple of CHUNK_SIZE ({CHUNK_SIZE})"
            ),
        }
    }
}

impl std::error::Error for WorldError {}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 格子世界。
///
/// 布局是扁平的 `Vec<MaterialCell>`（行主序），而不是 `Vec<Vec<_>>`——
/// 连续内存对分块扫描和将来的 rayon 并行都很友好。
#[derive(Debug, Clone)]
pub struct World {
    width: usize,
    height: usize,
    chunk_cols: usize,
    chunk_rows: usize,
    cells: Vec<MaterialCell>,
    dirty: Vec<bool>,
    dirty_count: usize,
    tick: u64,
}

impl World {
    /// 创建空世界。尺寸必须是 [`CHUNK_SIZE`] 的整数倍。
    pub fn new(width: usize, height: usize) -> Result<Self, WorldError> {
        if width == 0 || height == 0 {
            return Err(WorldError::ZeroSize);
        }
        if width % CHUNK_SIZE != 0 || height % CHUNK_SIZE != 0 {
            return Err(WorldError::NotChunkAligned { width, height });
        }

        let chunk_cols = width / CHUNK_SIZE;
        let chunk_rows = height / CHUNK_SIZE;
        let chunk_count = chunk_cols * chunk_rows;

        Ok(Self {
            width,
            height,
            chunk_cols,
            chunk_rows,
            cells: vec![MaterialCell::default(); width * height],
            // 新建的世界整块都是脏的，第一个 tick 需要完整扫描一次。
            dirty: vec![true; chunk_count],
            dirty_count: chunk_count,
            tick: 0,
        })
    }

    /// 世界宽度（格）。
    #[inline]
    pub const fn width(&self) -> usize {
        self.width
    }

    /// 世界高度（格）。
    #[inline]
    pub const fn height(&self) -> usize {
        self.height
    }

    /// 已推进的 tick 数。
    #[inline]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// 分块列数。
    #[inline]
    pub const fn chunk_cols(&self) -> usize {
        self.chunk_cols
    }

    /// 分块行数。
    #[inline]
    pub const fn chunk_rows(&self) -> usize {
        self.chunk_rows
    }

    /// 分块总数。
    #[inline]
    pub const fn chunk_count(&self) -> usize {
        self.chunk_cols * self.chunk_rows
    }

    /// 坐标是否落在世界内。接受 `isize` 以便邻域查询时直接传 `x - 1`。
    #[inline]
    pub fn in_bounds(&self, x: isize, y: isize) -> bool {
        x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height
    }

    /// 行主序索引。
    #[inline]
    const fn index(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    /// 读取格子。越界返回 [`MaterialKind::Empty`]。
    #[inline]
    pub fn get(&self, x: isize, y: isize) -> MaterialKind {
        if self.in_bounds(x, y) {
            self.cells[self.index(x as usize, y as usize)].kind
        } else {
            MaterialKind::Empty
        }
    }

    /// 读取格子（不越界检查，调用方保证坐标合法）。
    #[inline]
    pub fn get_unchecked(&self, x: usize, y: usize) -> MaterialKind {
        self.cells[self.index(x, y)].kind
    }

    /// 写入格子。值未变化时不标脏，这是脏标记有效的前提。
    ///
    /// 越界写入被静默忽略——画笔拖到边缘时不该 panic。
    pub fn set(&mut self, x: isize, y: isize, kind: MaterialKind) -> bool {
        if !self.in_bounds(x, y) {
            return false;
        }
        let idx = self.index(x as usize, y as usize);
        if self.cells[idx].kind == kind {
            return false;
        }
        self.cells[idx].kind = kind;
        self.mark_dirty(x as usize, y as usize);
        true
    }

    /// 把矩形区域刷成同一种材质。画笔工具的底层操作。
    ///
    /// 区域会被裁剪到世界边界内。
    pub fn fill_rect(&mut self, x: isize, y: isize, w: usize, h: usize, kind: MaterialKind) {
        for dy in 0..h as isize {
            for dx in 0..w as isize {
                self.set(x + dx, y + dy, kind);
            }
        }
    }

    /// 把整个世界刷成同一种材质。
    pub fn fill_all(&mut self, kind: MaterialKind) {
        for cell in &mut self.cells {
            cell.kind = kind;
        }
        self.dirty.iter_mut().for_each(|d| *d = true);
        self.dirty_count = self.chunk_count();
    }

    /// 标记某格所在的分块为脏。
    #[inline]
    pub fn mark_dirty(&mut self, x: usize, y: usize) {
        let cx = x / CHUNK_SIZE;
        let cy = y / CHUNK_SIZE;
        let ci = cy * self.chunk_cols + cx;
        if !self.dirty[ci] {
            self.dirty[ci] = true;
            self.dirty_count += 1;
        }
    }

    /// 分块是否为脏。
    #[inline]
    pub fn is_chunk_dirty(&self, cx: usize, cy: usize) -> bool {
        self.dirty[cy * self.chunk_cols + cx]
    }

    /// 当前脏块数量。用来观察"活跃格子比例"——这是落沙层的真实性能指标。
    #[inline]
    pub const fn dirty_chunk_count(&self) -> usize {
        self.dirty_count
    }

    /// 清空所有脏标记。材质层应当在每个 tick 的**开头**调用，
    /// 这样本 tick 产生的脏标记就留给下一个 tick 使用。
    pub fn clear_dirty(&mut self) {
        self.dirty.iter_mut().for_each(|d| *d = false);
        self.dirty_count = 0;
    }

    /// 推进一个 tick。
    ///
    /// M0 阶段这里只做两件事：清脏标记、tick 计数。
    /// 材质流动规则、生命层调度会在后续里程碑接入。
    pub fn step(&mut self) {
        self.clear_dirty();
        self.tick += 1;
    }

    /// 统计某种材质的格子数。
    pub fn count_of(&self, kind: MaterialKind) -> usize {
        self.cells.iter().filter(|c| c.kind == kind).count()
    }

    /// 一次性统计所有材质的格子数。
    pub fn census(&self) -> [usize; MaterialKind::COUNT] {
        let mut out = [0usize; MaterialKind::COUNT];
        for cell in &self.cells {
            out[cell.kind.as_u8() as usize] += 1;
        }
        out
    }

    /// 网格的确定性指纹（FNV-1a，64 位）。
    ///
    /// SPEC 的可复现性断言直接建立在这个函数上，所以它**必须**是稳定且平台无关的——
    /// 不要换成 `DefaultHasher`，它的输出在不同 Rust 版本间不保证一致。
    pub fn hash(&self) -> u64 {
        let mut h = FNV_OFFSET_BASIS;
        for cell in &self.cells {
            h ^= cell.kind.as_u8() as u64;
            h = h.wrapping_mul(FNV_PRIME);
            h ^= cell.flags as u64;
            h = h.wrapping_mul(FNV_PRIME);
        }
        h
    }

    /// 只读访问整张网格，供渲染器与快照使用。
    #[inline]
    pub fn cells(&self) -> &[MaterialCell] {
        &self.cells
    }
}

#[cfg(test)]
mod tests {
    use super::{World, WorldError, CHUNK_SIZE};
    use crate::material::MaterialKind;

    #[test]
    fn rejects_unaligned_size() {
        assert_eq!(
            World::new(100, 64).unwrap_err(),
            WorldError::NotChunkAligned {
                width: 100,
                height: 64
            }
        );
        assert_eq!(World::new(0, 0).unwrap_err(), WorldError::ZeroSize);
    }

    #[test]
    fn accepts_chunk_aligned_size() {
        let w = World::new(256, 512).unwrap();
        assert_eq!(w.chunk_cols(), 4);
        assert_eq!(w.chunk_rows(), 8);
        assert_eq!(w.chunk_count(), 32);
    }

    #[test]
    fn out_of_bounds_reads_are_empty() {
        let w = World::new(64, 64).unwrap();
        assert_eq!(w.get(-1, 0), MaterialKind::Empty);
        assert_eq!(w.get(0, -1), MaterialKind::Empty);
        assert_eq!(w.get(64, 0), MaterialKind::Empty);
        assert_eq!(w.get(0, 64), MaterialKind::Empty);
    }

    #[test]
    fn out_of_bounds_writes_are_ignored() {
        let mut w = World::new(64, 64).unwrap();
        assert!(!w.set(-1, 0, MaterialKind::Stone));
        assert!(!w.set(999, 999, MaterialKind::Stone));
        assert_eq!(w.hash(), World::new(64, 64).unwrap().hash());
    }

    #[test]
    fn redundant_write_does_not_dirty() {
        let mut w = World::new(64, 64).unwrap();
        w.clear_dirty();
        assert!(!w.set(10, 10, MaterialKind::Empty)); // 本来就是 Empty
        assert_eq!(w.dirty_chunk_count(), 0);
        assert!(w.set(10, 10, MaterialKind::Stone));
        assert_eq!(w.dirty_chunk_count(), 1);
    }

    #[test]
    fn dirty_marking_is_per_chunk() {
        let mut w = World::new(128, 128).unwrap();
        w.clear_dirty();
        // 同一块内写多次，只应标脏一次。
        w.set(0, 0, MaterialKind::Stone);
        w.set(1, 1, MaterialKind::Stone);
        w.set(63, 63, MaterialKind::Stone);
        assert_eq!(w.dirty_chunk_count(), 1);
        // 跨到相邻块，应变成两块。
        w.set(64, 0, MaterialKind::Stone);
        assert_eq!(w.dirty_chunk_count(), 2);
        assert!(w.is_chunk_dirty(1, 0));
    }

    #[test]
    fn same_seed_same_hash() {
        let mut a = World::new(256, 256).unwrap();
        let mut b = World::new(256, 256).unwrap();
        for (w, off) in [(&mut a, 0isize), (&mut b, 0isize)] {
            w.fill_rect(10 + off, 10, 32, 32, MaterialKind::Sand);
            w.fill_rect(100, 50, 16, 16, MaterialKind::Water);
            w.step();
        }
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn different_content_different_hash() {
        let mut a = World::new(64, 64).unwrap();
        let mut b = World::new(64, 64).unwrap();
        a.set(5, 5, MaterialKind::Sand);
        b.set(5, 5, MaterialKind::Water);
        assert_ne!(a.hash(), b.hash());
    }

    #[test]
    fn census_matches_manual_count() {
        let mut w = World::new(64, 64).unwrap();
        w.fill_rect(0, 0, 10, 10, MaterialKind::Stone);
        w.fill_rect(20, 20, 4, 4, MaterialKind::Water);
        let c = w.census();
        assert_eq!(c[MaterialKind::Stone.as_u8() as usize], 100);
        assert_eq!(c[MaterialKind::Water.as_u8() as usize], 16);
        assert_eq!(c[MaterialKind::Empty.as_u8() as usize], 64 * 64 - 116);
    }

    #[test]
    fn step_clears_dirty_and_advances_tick() {
        let mut w = World::new(64, 64).unwrap();
        w.set(1, 1, MaterialKind::Stone);
        assert_eq!(w.dirty_chunk_count(), 1);
        w.step();
        assert_eq!(w.dirty_chunk_count(), 0);
        assert_eq!(w.tick(), 1);
    }

    #[test]
    fn chunk_size_is_what_we_think_it_is() {
        assert_eq!(CHUNK_SIZE, 64);
    }
}
