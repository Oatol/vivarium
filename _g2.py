p = "crates/core/src/life_sim.rs"
s = open(p, encoding="utf-8").read()

# 1) 字段：累计"水面以上还了几格水"
old = '''    /// 本次落笔一共抹掉了几个生物。**松开时才写进控制台**——'''
if old not in s:
    old = '''    /// 累计下落次数。这是"重力有没有在工作"的直接证据。'''
    new = '''    /// 累计下落次数。这是"重力有没有在工作"的直接证据。'''
    assert old in s
else:
    raise SystemExit("anchor 冲突")

# 用 moved_total 那一组字段做锚点
old = '''    moved_total: [u64; 4],'''
new = '''    moved_total: [u64; 4],
    /// 累计有多少格**湿细胞在水面之上死亡**——也就是"水被从海里搬到岸上"
    /// 的总量。
    ///
    /// 这个数存在的原因是：**"水在变多还是变少"这件事一直看不见**。
    /// 实测 8000 tick 里 `Water` 格数掉了 8.7%，我第一反应是"漏了"，
    /// 其实是**被活着的身体借走了**（1000 个动物 × 几格）。
    /// 于是这里记两件事：搬了多少（这个数）、现在有多少水在水面以上
    /// （`water_above`）。**没有仪器就没有结论。**
    water_moved: u64,'''
assert old in s, "field"
s = s.replace(old, new, 1)

# 2) 初始化（Default 与 restore 两处）
assert s.count("            moved_total: [0; 4],") == 2, s.count("            moved_total: [0; 4],")
s = s.replace("            moved_total: [0; 4],", "            moved_total: [0; 4],\n            water_moved: 0,")

# 3) bury 里记数
old = '''            if cell.wet {'''
new = '''            // "水面以上的湿细胞死亡" = 水被搬到岸上了。记一笔，见 `water_moved`。
            if cell.wet && (y as i32) < self.moisture.sea_level() {
                self.water_moved += 1;
            }
            if cell.wet {'''
assert old in s, "bury"
s = s.replace(old, new, 1)

# 4) 两个访问器
old = '''    /// 累计下落次数。这是"重力有没有在工作"的直接证据。
    #[inline]
    pub fn fell_total(&self) -> u64 {
        self.fell_total
    }'''
new = '''    /// 累计下落次数。这是"重力有没有在工作"的直接证据。
    #[inline]
    pub fn fell_total(&self) -> u64 {
        self.fell_total
    }

    /// 累计有多少格湿细胞在**水面之上**死亡（= 水被从海里搬到岸上）。
    ///
    /// 和 [`Self::water_above`] 一起看：前者是流量，后者是存量。
    #[inline]
    pub fn water_moved(&self) -> u64 {
        self.water_moved
    }

    /// 现在有多少格水在**海平面以上**。
    ///
    /// 只在测量里调，不在 tick 循环里调——它要扫一遍世界。
    pub fn water_above(&self, world: &World) -> usize {
        let sea = self.moisture.sea_level();
        let mut n = 0;
        for y in 0..sea.max(0) as isize {
            for x in 0..world.width() as isize {
                if world.get(x, y) == MaterialKind::Water {
                    n += 1;
                }
            }
        }
        n
    }'''
assert old in s, "accessor"
s = s.replace(old, new, 1)
open(p, "w", encoding="utf-8").write(s)
print("ok")
