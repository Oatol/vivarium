//! 世界生成。
//!
//! ## 这一层为什么在 core 里
//!
//! SPEC 第 2.0 节定下：世界是一张**横板侧视剖面**，有唯一的重力轴（+Y 向下）。
//! 默认地貌是「海岸线 + 浅海」——一条平缓的岸，一半陆地一半海。
//!
//! **母题换过一次**（v0.30）。旧版是"海床漂移 + 几座钟形岛"，岛必须从
//! 海底堆到水面以上，实测剖面最陡 17 格/列——而移动是四向离散的，
//! 相邻高差 > 1 就是一道墙。现在用 fBm 高度场 + 热力侵蚀，坡度上界 2。
//!
//! 这个决定管着后面的一切：沙往下掉、气往上浮、光自上而下衰减、
//! 生物默认水生（游泳不必对抗重力）。
//!
//! M0 期间这段生成逻辑被复制了三份（`bins/sim`、`bins/game`、
//! `crates/render` 的预览测试），已经开始漂移。收进 core 是为了让它只剩一份。
//!
//! ## 确定性
//!
//! 全部随机走 [`Rng`]，同种子 ⇒ 同一世界。这是 SPEC 可复现性断言的地形入口。

use crate::material::MaterialKind;
use crate::rng::Rng;
use crate::world::World;

/// 天空占世界高度的百分比。水面线在它下面。
///
/// 四分之一：够给光梯度、气候层和将来的飞行生物留位置，
/// 又不至于让画面上半张全是空的。水体才是主舞台。
///
/// 它同时是**陆地能抬多高的上限**（陆地不能顶出世界），所以调低它
/// 会连带压矮地形——见 `by_sky`。
const SKY_PERCENT: usize = 25;

/// 目标是让多少百分比的列露出水面。
///
/// **这是"目标"而不是"抽奖"**：高度场生成完之后按分位数切一刀，
/// 于是每个种子都恰好有这么多列在岸上。早先按固定倍率随机的版本，
/// 有些种子一张图全是水（"海洋 + 岛屿"名不副实），有些又全是大山。
const LAND_TARGET_PERCENT: usize = 50;

/// 高度场的基频：整个宽度上大约有几个起伏。
///
/// **它买的是"形状多样性"，代价是"起伏幅度"。** 坡度正比于
/// `BASE_FREQ × scale`，而 `scale` 是被坡度上限倒推出来的——所以基频越高，
/// 地形特征越多、每个特征越矮。1.6 的实测结果：512² 里陆地抬 107 格、
/// 海深 88 格；调到 2.2 就只剩 78 / 64。
///
/// 这个交易能用一句话说清，不是一组试出来的魔法数。
const BASE_FREQ: f32 = 1.6;

/// fBm 的八度数。
///
/// 3 个：大轮廓（海岸线走向）+ 中等起伏（海湾、半岛）+ 小尺度抖动（碎石岸）。
/// 再多的话高频分量会贡献陡坡，而热力侵蚀要花很多轮去磨平它。
const OCTAVES: usize = 3;

/// 每层八度的振幅衰减。
///
/// 0.45 而不是 0.5：三层八度对**坡度**的贡献大致正比于 `振幅 × 频率`
/// （频率每层翻倍），0.5 意味着三层贡献相等，最大坡度会顶到 2.5 以上，
/// 全靠侵蚀去磨。0.45 让第三层只占前两层的八成，侵蚀的活轻一点。
const PERSISTENCE: f32 = 0.45;

/// 相邻两列允许的最大高差（格）。
///
/// **这是这一版地形的核心约束，不是观感参数。** 移动是四向离散的，
/// 所以"相邻高差 > 1"就是一道必须靠纯垂直越过的墙；而 `pick_direction`
/// 允许斜向之后，"水平 1 + 垂直 1"正好是 45°，也就是坡度 1。
/// 坡度 2 是给"斜向 + 一格余量"留的余地，再陡就又是墙了。
const MAX_SLOPE: isize = 2;

/// 热力侵蚀最多跑多少轮。收敛就提前退出。
const EROSION_PASSES: usize = 512;

/// 每多少列放一团残骸。数值越大团块越稀疏。
const REMAINS_CLUMPS_PER_COLUMNS: usize = 6;

/// 放气泡所需的最小水柱高度（格）。
const MIN_BUBBLE_ROOM: usize = 10;

/// 生成结果：地表形状 + 两条基准线。
///
/// 相机取景、统计面板、测试断言都要用它，所以生成函数直接返回，
/// 而不是让调用方自己再扫一遍网格猜。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldLayout {
    /// 水面线的 y。`y < waterline` 是天空。
    pub waterline: usize,
    /// 海床的**基准**线 y。真实海床在它上下起伏，所以它不是材质分界。
    pub seabed: usize,
    /// 每列的地表 y。`surface[x] < waterline` 表示这一列露出水面。
    pub surface: Vec<isize>,
}

impl WorldLayout {
    /// 露出水面的列数，也就是"陆地宽度"。
    ///
    /// **判据是 `<= 水面线`，不是 `<`。** 地表正好落在水面线上的列
    /// 一滴水都没有（材质生成里只有 `top > waterline` 才灌水），
    /// 所以它是一块**海滩**，不是海。用小世界时这个区别很显眼：
    /// 64 格高的世界里垂直分辨率只有十几格，`round()` 会把一大片列
    /// 拍到水面线上——按 `<` 数会把它们全算成海，于是"陆地占比 0.12"
    /// 而画面上明明一半是地。
    pub fn land_columns(&self) -> usize {
        let wl = self.waterline as isize;
        self.surface.iter().filter(|&&y| y <= wl).count()
    }

    /// 最高的地表（最小的 y）。空世界返回水面线。
    pub fn highest(&self) -> isize {
        self.surface
            .iter()
            .copied()
            .min()
            .unwrap_or(self.waterline as isize)
    }

    /// 最高那一列的 x。空世界返回 0。
    pub fn peak_x(&self) -> usize {
        let mut best = 0usize;
        for x in 1..self.surface.len() {
            if self.surface[x] < self.surface[best] {
                best = x;
            }
        }
        best
    }

    /// 最长的一段连续陆地，返回 `[lo, hi)`；一滴陆地都没有时返回 `None`。
    ///
    /// 为什么不是"最高的那一列"：v0.30 的地形是一片**海岸**而不是几座孤岛，
    /// 最高点往往只是内陆一个不起眼的小丘，而玩家想看的是海岸线。
    pub fn main_land(&self) -> Option<(usize, usize)> {
        let wl = self.waterline as isize;
        let mut best: Option<(usize, usize)> = None;
        let mut start: Option<usize> = None;
        for x in 0..=self.surface.len() {
            let land = x < self.surface.len() && self.surface[x] <= wl;
            match (land, start) {
                (true, None) => start = Some(x),
                (false, Some(lo)) => {
                    if best.map_or(true, |(a, b)| x - lo > b - a) {
                        best = Some((lo, x));
                    }
                    start = None;
                }
                _ => {}
            }
        }
        best
    }

    /// 最长那段陆地的中心列。没有陆地时退回最高那一列。
    pub fn main_land_center(&self) -> usize {
        self.main_land()
            .map(|(lo, hi)| lo + (hi - lo) / 2)
            .unwrap_or_else(|| self.peak_x())
    }

    /// 相机取景建议：返回视野左沿的 x。
    ///
    /// 对准最长那段陆地的中心。把相机停在开阔水面上，屏幕上就只剩一片蓝色
    /// ——ASCII 视图尤其明显。
    pub fn focus_left(&self, view_width: usize) -> usize {
        self.main_land_center().saturating_sub(view_width / 2)
    }

    /// 相机取景建议：返回视野上沿的 y。
    ///
    /// 把视野中心放在「最高岛顶」与「水面线」之间**偏下**的位置，
    /// 这样天空、岛屿轮廓和水体边界能同时入画。
    /// 按世界几何中心取景只会得到一整屏水——水体占了大半张图。
    pub fn focus_top(&self, view_height: usize) -> usize {
        let highest = self.highest().clamp(0, self.waterline as isize) as usize;
        let center = highest + (self.waterline.saturating_sub(highest)) * 3 / 4;
        center.saturating_sub(view_height / 2)
    }
}

/// 按「海岸线 + 浅海」铺一遍初始地貌，返回地表形状。
///
/// ## 母题换过一次（v0.30）
///
/// 旧版是"海床漂移 + 几座高斯钟形岛"。它能跑，但岛必须从海底一路堆到
/// 水面以上，而水深是固定的——于是**唯一能调的就是宽度**。实测半宽 42 列时
/// 剖面最陡 **17 格/列**，84 列里 38 列超过 1 格/列。而移动是四向离散的，
/// 相邻高差 > 1 就是一道必须靠纯垂直越过的墙：鱼学不会，就沿着崖壁一直蹭。
/// 玩家看到的是"水里的生物只朝一个方向游，撞在石壁上"。
///
/// 现在换成 fBm 高度场 + 热力侵蚀：
///
/// 1. **高度场**：三个八度的值噪声，`BASE_FREQ = 3`；
/// 2. **海岸线**：按分位数切一刀，保证恰好 `LAND_TARGET_PERCENT` 的列在岸上
///    ——陆地占比是一个**目标**，不是一次抽奖；
/// 3. **热力侵蚀**：逐列把超过 `MAX_SLOPE` 的高差搬给邻居，迭代到收敛。
///    这一步把"平缓"从感觉变成可断言的东西（`max_slope <= 2`）。
///
/// 剩下的（气泡、残骸、岛顶的火）与旧版一致。
pub fn sea_and_islands(world: &mut World, rng: &mut Rng) -> WorldLayout {
    let w = world.width();
    let h = world.height();

    let waterline = (h * SKY_PERCENT / 100).clamp(2, h.saturating_sub(2));

    // --- 1. 高度场 ---
    let noise_seed = rng.next_u64();
    let mut elev = vec![0f32; w];
    for (x, slot) in elev.iter_mut().enumerate() {
        *slot = fbm(x as f32 / w as f32 * BASE_FREQ, noise_seed);
    }

    // --- 2. 归一化 + 按分位数切海岸线 ---
    //
    // **先归一化再切。** fBm 的实际取值范围随种子变化很大（实测 64² 的
    // 某个种子只用了 0.2 的幅度），而后面所有映射都假设"0..1"。
    // 不归一化的话，同一个 `scale` 在不同种子上会给出天差地别的起伏——
    // 症状是"有些种子整张图是一块平地"，而且看不出是哪一步错了。
    let (emin, emax) = elev
        .iter()
        .copied()
        .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
    let span = (emax - emin).max(1e-6);
    for e in elev.iter_mut() {
        *e = (*e - emin) / span;
    }

    // 不用"平均值"：fBm 的分布是近似对称的，但一座高山的尾部会把平均值
    // 拽偏，于是同一个 `LAND_TARGET_PERCENT` 在不同种子上给出不同的陆地占比。
    // 分位数是**定义**，不是估计。
    let mut sorted = elev.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let cut_idx = (w * (100 - LAND_TARGET_PERCENT) / 100).min(w - 1);
    let cut = sorted[cut_idx];

    // --- 3. 高度 → 纵坐标：**一个**比例尺，两侧共用 ---
    //
    // 早先陆地和海各用一个比例尺（各自归一化到 `land_rise` / `sea_depth`），
    // 结果海岸线处有一个**折角**：同一格高差，陆侧按 92/0.55 放大、
    // 海侧按 143/0.45 放大。实测最陡的 4 格/列就出现在那条线上，
    // 而不是在深海——**折角才是问题，深度不是。**
    //
    // 共用一个 `scale` 之后映射在 `cut` 处 C¹ 连续，坡度上界只剩
    // "高度场的导数 × scale"。于是 `scale` 可以直接从约束倒推：
    //
    // 1. 坡度：`scale ≤ MAX_SLOPE / d(elev)/dx`；
    // 2. 陆地不能顶出世界：`scale ≤ (waterline − 2) / 0.55`（0.55 是
    //    噪声高于分位线那一半的跨度，取上界）。
    //
    // 取两者较小值。这条式子把"平缓"变成了一个**算出来的**数，
    // 而不是一组试出来的魔法常数。
    let d_elev = 2.46 * BASE_FREQ / w as f32;
    // 用**乐观**的坡度上界来定比例尺，让侵蚀去收拾剩下的。
    //
    // `d_elev` 是高度场导数的**最坏情况**，而真实剖面上达到最坏值的只是
    // 很短的几段。按最坏值定比例尺会把地形压得过矮（实测 512² 里海只有
    // 70 格深，生物数从 692 掉到 234）。按 3 倍定，超出的部分都是短段，
    // 侵蚀几轮就能磨到 2 以内——而长斜坡本来就不会超。
    let by_slope = 3.0 * span / d_elev;
    // 陆地那半最高能抬到水面线之上 `waterline − 2` 格。
    let by_sky = (waterline as f32 - 2.0) / (1.0 - cut).max(1e-6);
    // 海那半最深能到世界底部之上 2 格。
    let by_sea = (h as f32 - waterline as f32 - 2.0) / cut.max(1e-6);
    let scale = by_slope.min(by_sky).min(by_sea).max(4.0);

    let mut surface: Vec<isize> = elev
        .iter()
        .map(|&e| waterline as isize - ((e - cut) * scale).round() as isize)
        .collect();
    let sea_depth = surface
        .iter()
        .copied()
        .max()
        .unwrap_or(waterline as isize)
        .max(waterline as isize) as usize
        - waterline;
    let seabed = waterline + sea_depth;

    // --- 3. 热力侵蚀 ---
    erode(&mut surface, MAX_SLOPE, EROSION_PASSES);
    // 侵蚀可能把某一列推到边界外（极端种子里会有），夹回来。
    // 夹回来会重新制造陡坡，所以夹完再跑一遍——两遍就够，实测没有种子需要第三遍。
    for s in surface.iter_mut() {
        *s = (*s).clamp(1, h as isize - 2);
    }
    erode(&mut surface, MAX_SLOPE, EROSION_PASSES);
    for s in surface.iter_mut() {
        *s = (*s).clamp(1, h as isize - 2);
    }

    // --- 4. 铺材质 ---
    //
    // 逐列自地表往下分层：水 → 沙 → 石。
    //
    // 不先铺一层"平坦的基岩再往上盖水"——海床是起伏的，那样会在海床下凹处
    // 把水灌进基岩里，而且残骸会漂在水中间。（两个都真的发生过。）
    let mut rock_top = vec![0usize; w];
    for x in 0..w {
        let top = surface[x].clamp(0, h as isize) as usize;

        // 水体：水面线 → 地表（只有地表在水面之下才有水柱）
        if top > waterline {
            world.fill_rect(
                x as isize,
                waterline as isize,
                1,
                top - waterline,
                MaterialKind::Water,
            );
        }

        // 沙层厚度按"离水面多远"走，不再是一个固定的随机区间。
        //
        // 旧版每列都是 3~8 格沙，于是山顶和海滩的材质一模一样，看不出
        // "这里是被水冲刷过的"。现在：海滩最厚（冲积），海床中等（沉积），
        // 高处最薄（风化和剥蚀把细粒带走了，露出基岩）。
        let rel = top as isize - waterline as isize;
        let (sand_lo, sand_hi) = if rel > 12 {
            (2, 6) // 高处：薄
        } else if rel > 0 {
            (4, 9) // 浅海：沉积
        } else if rel > -6 {
            (6, 12) // 海滩：冲积最厚
        } else {
            (3, 8) // 内陆
        };
        let sand = rng.gen_range_usize(sand_lo, sand_hi).min(h - top);
        if sand > 0 {
            world.fill_rect(x as isize, top as isize, 1, sand, MaterialKind::Sand);
        }

        // 其下全是基岩
        let rock = top + sand;
        rock_top[x] = rock;
        if rock < h {
            world.fill_rect(x as isize, rock as isize, 1, h - rock, MaterialKind::Stone);
        }
    }

    // 气泡：水体里悬着的几团气（M1 之后它们会真的上浮）。
    //
    // 2×2 的方块要**整块**落在水面线之下、两列地表之上。
    // 早先只按本列的 surface 算范围，结果方块的下半行会从相邻列的地表冒出来，
    // 变成"天上飘着气"；只按本列算还会把气泡塞进浅滩边上那几格水洼里。
    for _ in 0..(w / 6) {
        let x = rng.gen_range_usize(0, w.saturating_sub(1).max(1));
        let right = surface.get(x + 1).copied().unwrap_or(h as isize);
        let floor = surface[x].min(right) as usize;
        let ceiling = waterline + 3;
        // 水太浅就别放了——几格深的水里塞一个 2×2 方块，
        // 那一小段水柱会被占掉一大半，看起来像水底在冒泡。
        if floor < ceiling + MIN_BUBBLE_ROOM {
            continue;
        }
        let y = rng.gen_range_usize(ceiling, floor - 2);
        world.fill_rect(x as isize, y as isize, 2, 2, MaterialKind::Gas);
    }

    // 沉积层里的残骸——细胞死亡后的产物，M2 起它会真的出现。
    //
    // 做成**透镜状团块**，而不是逐列单点撒。单点撒出来是一串孤立的珠子，
    // 既不像有机沉积，也让"团块整体沉降"这件事在 M1 里根本看不出来。
    // 透镜是水平拉长的椭圆：宽 7~17 格、厚 3~7 格，埋在海床基岩里。
    //
    // 判定用整数而不是浮点：`(dx/hw)² + (dy/hh)² ≤ 1` 两边同乘 `hw²·hh²`
    // 就只剩整数乘法。世界生成必须是逐位可复现的，能不用浮点就不用。
    for _ in 0..(w / REMAINS_CLUMPS_PER_COLUMNS) {
        let cx = rng.gen_range_usize(0, w);
        // 团块中心从基岩顶往下 4 格起，保证整个团块埋在石头里。
        let lo = rock_top[cx] + 4;
        if lo >= h {
            continue;
        }
        let cy = rng.gen_range_usize(lo, h);
        let hw = rng.gen_range_usize(3, 9) as isize; // 半宽 3~8
        let hh = rng.gen_range_usize(1, 4) as isize; // 半高 1~3
        let (hw2, hh2) = (hw * hw, hh * hh);
        for dy in -hh..=hh {
            for dx in -hw..=hw {
                if dx * dx * hh2 + dy * dy * hw2 > hw2 * hh2 {
                    continue;
                }
                let (px, py) = (cx as isize + dx, cy as isize + dy);
                if !world.in_bounds(px, py) {
                    continue;
                }
                // **只替换基岩**。海床是起伏的，团块边缘会伸到沙层或水里去，
                // 在那里造残骸就等于凭空往水里塞东西。
                if world.get(px, py) == MaterialKind::Stone {
                    world.set(px, py, MaterialKind::Remains);
                    // 天然碎屑带能量（`flags = 25`），这一条有两个作用：
                    // 1. 它们是可吃的食物，生命层建账时会把这份能量计入注入；
                    // 2. `flags != 0` 让材质层的分解规则跳过它们——
                    //    否则没有生物的世界也会一直"腐烂"，M1 的静止契约直接失效。
                    world.set_flags(px, py, 25);
                }
            }
        }
    }

    // 一小撮火，落在最高的那根山脊上方的空中。
    // M1 起它会自己蔓延、被水浇灭、十几 tick 后烧完——正好用来确认反应表接对了。
    let mut peak_x = 0usize;
    for x in 1..w {
        if surface[x] < surface[peak_x] {
            peak_x = x;
        }
    }
    if surface[peak_x] < waterline as isize {
        world.fill_rect(
            (peak_x as isize - 3).max(0),
            (surface[peak_x] - 3).max(1),
            6,
            3,
            MaterialKind::Fire,
        );
    }

    WorldLayout {
        waterline,
        seabed,
        surface,
    }
}

/// 从**已有**的网格反推地表形状。
///
/// 读档之后没有"生成结果"可拿：存档里只有材质，没有当初那条水面线和海床基线。
/// 但相机取景（[`WorldLayout::focus_left`]）和统计面板要它们，所以只能扫一遍。
///
/// ## 判据
///
/// 每列最上面那一格**固体**（石或沙）就是地表。水、气、残骸、尸体都不算——
/// 它们是"落在地表上"的东西。把漂过的尸体当地表，水面线会随着它上下跳，
/// 而那正好是相机取景最敏感的量。
///
/// `seabed` 取各地表的**中位数**——注意它不等于生成时那条基线，见下面
/// "它恢复不了什么"一节。中位数而不是平均值：一座岛会把平均值拽偏几十格，
/// 而中位数几乎不受影响。
///
/// ## 它恢复不了什么
///
/// 生成时的 `seabed` 是"海床漂移与岛屿抬升都归零的那条线"，而材质里
/// 只留下"这一列的地表最终在哪"。岛会把大片列抬高——实测一个 256² 的
/// 种子，中位地表比基线高 55 格（几座岛的坡脚连起来盖住了大半张图）。
///
/// 所以这里给的是**中位地表**，语义是"地面通常在哪儿"，只用于界面文案
/// 与取景兜底，**不参与任何模拟判据**。将来谁要拿它当"海底在哪"用，
/// 会得到一张错的水深图——所以这件事写在文档里，而不是留给读代码的人猜。
///
/// 一滴水都没有的世界里，`waterline` 退化成最高地表——那样
/// [`WorldLayout::focus_top`] 会把视野对准那座最高的山，而不是对准世界顶部。
pub fn scan(world: &World) -> WorldLayout {
    let (w, h) = (world.width(), world.height());
    let mut surface = vec![h as isize; w];
    let mut top_water: Option<usize> = None;

    for (x, slot) in surface.iter_mut().enumerate() {
        for y in 0..h {
            match world.get_unchecked(x, y) {
                MaterialKind::Stone | MaterialKind::Sand => {
                    *slot = y as isize;
                    break;
                }
                // `usize::MAX` 当"还没有见过水"的哨兵：第一格水一定比它小。
                // 用 `unwrap_or(usize::MAX)` 而不是 `is_none_or`，后者要
                // Rust 1.82，而这个 workspace 声明的是 1.80。
                MaterialKind::Water if top_water.unwrap_or(usize::MAX) > y => {
                    top_water = Some(y);
                }
                _ => {}
            }
        }
    }

    let highest = surface.iter().copied().min().unwrap_or(0).max(0) as usize;
    let waterline = top_water.unwrap_or(highest).min(h);

    let mut sorted = surface.clone();
    sorted.sort_unstable();
    let seabed = sorted
        .get(sorted.len() / 2)
        .copied()
        .unwrap_or(h as isize)
        .clamp(waterline as isize, h as isize) as usize;

    WorldLayout {
        waterline,
        seabed,
        surface,
    }
}

/// 一维值噪声：整数格点取随机值，中间用 smoothstep 插值。
///
/// 为什么不用正弦叠加：正弦是周期的，会看出重复；为什么不用浮点哈希：
/// 世界生成必须逐位可复现，而整数哈希在每一层都精确。
fn value_noise(x: f32, seed: u64) -> f32 {
    let i = x.floor();
    let f = x - i;
    let a = hash01(i as i64, seed);
    let b = hash01(i as i64 + 1, seed);
    // smoothstep：两端一阶导为 0，所以格点处没有折角。
    let t = f * f * (3.0 - 2.0 * f);
    a + (b - a) * t
}

/// 整数 → `[0, 1)` 的确定性哈希（splitmix64 的尾巴）。
fn hash01(x: i64, seed: u64) -> f32 {
    let mut h = (x as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(seed);
    h ^= h >> 30;
    h = h.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94D0_49BB_1331_11EB);
    h ^= h >> 31;
    // 取高 24 位：低位的分布不如高位均匀。
    (h >> 40) as f32 / (1u64 << 24) as f32
}

/// 分形布朗运动：多个八度的值噪声叠加。
fn fbm(x: f32, seed: u64) -> f32 {
    let mut sum = 0.0f32;
    let mut amp = 1.0f32;
    let mut freq = 1.0f32;
    let mut norm = 0.0f32;
    for o in 0..OCTAVES {
        // 每层换一个种子，否则三层的格点会重合，叠出来还是同一个形状。
        let layer_seed = seed ^ ((o as u64 + 1).wrapping_mul(0x2545_F491_4F6C_DD1D));
        sum += value_noise(x * freq, layer_seed) * amp;
        norm += amp;
        amp *= PERSISTENCE;
        freq *= 2.0;
    }
    sum / norm
}

/// 热力侵蚀：逐列把超过 `max_slope` 的高差搬给邻居，迭代到收敛。
///
/// 注意 `surface` 是**纵坐标**（+Y 向下），所以"左边更低"意味着
/// `surface[x-1] > surface[x]`。
///
/// 每一轮用**快照**算增量再统一应用，不是就地改。就地改会让遍历顺序
/// 决定结果：从右往左扫和从左往右扫会得到不同的地形，而这种不对称
/// 在画面上表现为"所有坡都朝一个方向"——看起来完全像"噪声生成写错了"。
///
/// 收敛性：每次把超出量减半（整数向上取整，所以至少减 1），
/// 超出量单调下降，必然在有限轮内停。
fn erode(surface: &mut [isize], max_slope: isize, passes: usize) {
    let n = surface.len();
    if n < 3 {
        return;
    }
    let mut delta = vec![0isize; n];
    for _ in 0..passes {
        delta.iter_mut().for_each(|d| *d = 0);
        let mut moved = false;
        for x in 0..n {
            if x > 0 {
                let d = surface[x - 1] - surface[x];
                if d > max_slope {
                    let t = (d - max_slope + 1) / 2;
                    delta[x - 1] -= t;
                    delta[x] += t;
                    moved = true;
                }
            }
            if x + 1 < n {
                let d = surface[x + 1] - surface[x];
                if d > max_slope {
                    let t = (d - max_slope + 1) / 2;
                    delta[x + 1] -= t;
                    delta[x] += t;
                    moved = true;
                }
            }
        }
        if !moved {
            break;
        }
        for (s, d) in surface.iter_mut().zip(delta.iter()) {
            *s += d;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: usize = 256;
    const H: usize = 256;
    const SEED: u64 = 2026_1016;

    fn build(w: usize, h: usize, seed: u64) -> (World, WorldLayout) {
        let mut world = World::new(w, h).expect("世界尺寸必须是 64 的倍数");
        let mut rng = Rng::new(seed);
        let layout = sea_and_islands(&mut world, &mut rng);
        (world, layout)
    }

    #[test]
    fn same_seed_same_world() {
        let (a, la) = build(W, H, SEED);
        let (b, lb) = build(W, H, SEED);
        assert_eq!(a.hash(), b.hash(), "同种子必须给出同一个世界");
        assert_eq!(la, lb, "地表形状也要一致");
    }

    #[test]
    fn different_seed_different_world() {
        let (a, _) = build(W, H, SEED);
        let (b, _) = build(W, H, SEED + 1);
        assert_ne!(a.hash(), b.hash(), "换种子应当换一张图");
    }

    #[test]
    fn scan_recovers_the_layout_that_generation_reported() {
        // 读档路径没有"生成结果"可拿，只能从材质反推。这里钉住的是
        // **反推出来的东西和生成时说的是同一件事**——否则读档之后
        // 相机会跳到一个和存档前完全不同的位置。
        let (world, generated) = build(W, H, SEED);
        let scanned = scan(&world);
        assert_eq!(scanned.waterline, generated.waterline, "水面线对不上");
        assert_eq!(scanned.surface.len(), generated.surface.len());
        // 地表允许差几格：生成时"地表"是设计线，扫出来的是沙层真实顶边，
        // 而沙层厚度是 3~8 格随机的。要求逐列相等等于要求沙层厚度为 0。
        let mut worst = 0isize;
        for (a, b) in scanned.surface.iter().zip(&generated.surface) {
            worst = worst.max((a - b).abs());
        }
        assert!(worst <= 12, "地表差得太多（最大 {worst} 格）");
        // **海床基线无法从材质里恢复**，这里不假装能。
        //
        // 生成时的 `seabed` 是"漂移和岛屿都归零的那条线"，而材质里只有
        // "这一列的地表最终在哪"。岛会把大片列抬高——这个种子里中位数
        // 比基线高 55 格，因为几座岛的坡脚连起来盖住了大半张图。
        // 所以 `scan` 给的是**中位地表**（"地面通常在哪儿"），不是基线。
        // 它只用于界面文案，不参与任何模拟判据——这一点必须说清楚，
        // 否则将来有人拿它当"海底在哪"用，会得到一张错的水深图。
        let (lo, hi) = (
            scanned.surface.iter().copied().min().unwrap_or(0),
            scanned.surface.iter().copied().max().unwrap_or(0),
        );
        assert!(
            (lo..=hi).contains(&(scanned.seabed as isize)),
            "中位地表 {} 落在所有地表 {lo}..={hi} 之外",
            scanned.seabed
        );
        assert!(scanned.seabed >= scanned.waterline);
    }

    #[test]
    fn scan_of_an_empty_world_does_not_panic_or_go_out_of_bounds() {
        // 一个"没有地形"的世界（新建时取消勾选"生成地形"）是合法的。
        // 扫描它不能 panic，也不能给出一个越界的取景建议。
        let world = World::new(100, 70).unwrap();
        let l = scan(&world);
        assert_eq!(l.surface, vec![70isize; 100]);
        assert_eq!(l.waterline, 70);
        assert!(l.focus_left(64) <= 100);
        assert!(l.focus_top(64) <= 70);
    }

    #[test]
    fn waterline_and_seabed_are_ordered_and_inside_the_world() {
        for (w, h) in [(64, 64), (256, 256), (512, 512)] {
            let (_, l) = build(w, h, SEED);
            assert!(l.waterline < l.seabed, "{w}x{h}: 水面线应当在海床之上");
            assert!(l.seabed < h, "{w}x{h}: 海床线应当在世界之内");
            assert_eq!(l.surface.len(), w, "{w}x{h}: 地表数组应当每列一个");
        }
    }

    #[test]
    fn each_column_is_layered_sky_water_sand_stone() {
        // 这条替代了早先"海床线以下一律是石"的断言。
        // 那条是错的：海床会起伏，所以材质分界跟着地表走，不是一条水平线。
        let (world, l) = build(W, H, SEED);
        for x in 0..W as isize {
            let top = l.surface[x as usize].clamp(0, H as isize) as usize;

            // 地表正上方：海面上是水，陆地上是空天。
            //
            // 陆地允许 Fire——那座演示用的火堆就摆在最高的岛顶上，
            // 是唯一一处刻意放在地表之上的东西。
            if top > 0 {
                let kind = world.get(x, top as isize - 1);
                let ok = if top > l.waterline {
                    kind == MaterialKind::Water
                } else {
                    matches!(kind, MaterialKind::Empty | MaterialKind::Fire)
                };
                assert!(
                    ok,
                    "x={x} 地表正上方是 {kind:?}（地表 y={top}，水面线 {}）",
                    l.waterline
                );
            }

            // 地表那一格是沙
            if top < H {
                assert_eq!(
                    world.get(x, top as isize),
                    MaterialKind::Sand,
                    "x={x} 地表那一格不是沙"
                );
            }

            // 地表往下 12 格比最厚的沙层（8）还深，一定已经进了基岩。
            // 允许 Remains——残骸本来就是埋在沉积层里的。
            if top + 12 < H {
                let kind = world.get(x, (top + 12) as isize);
                assert!(
                    matches!(kind, MaterialKind::Stone | MaterialKind::Remains),
                    "x={x} 地表下 12 格是 {kind:?}，本该是基岩"
                );
            }

            // 最底下一行一定在基岩里
            let bottom = world.get(x, (H - 1) as isize);
            assert!(
                matches!(bottom, MaterialKind::Stone | MaterialKind::Remains),
                "x={x} 世界底部是 {bottom:?}，本该是基岩"
            );
        }
    }

    #[test]
    fn sky_above_the_waterline_is_empty() {
        let (world, l) = build(W, H, SEED);
        for x in 0..W as isize {
            if l.surface[x as usize] > l.waterline as isize {
                assert_eq!(world.get(x, 0), MaterialKind::Empty, "x={x} 顶部不是空天");
            }
        }
    }

    #[test]
    fn water_fills_the_column_below_the_waterline() {
        let (world, l) = build(W, H, SEED);
        let mut checked = 0;
        for x in 0..W as isize {
            let top = l.surface[x as usize];
            if top <= l.waterline as isize + 2 {
                continue;
            }
            // 紧贴水面线下面一定是水——气泡最低也从水面线 +3 起。
            assert_eq!(
                world.get(x, l.waterline as isize + 1),
                MaterialKind::Water,
                "x={x} 水面线正下方不是水"
            );
            // 整根水柱里，非水的格子只能是气泡，而且不能太多。
            //
            // 一列最多被两个气泡盖到（原点只能落在 x-1 或 x），
            // 每个贡献两行，所以上限是 4 格；而水柱至少 MIN_BUBBLE_ROOM+3 格。
            let height = top - l.waterline as isize;
            let mut bubbles = 0isize;
            for y in (l.waterline as isize + 1)..top {
                let k = world.get(x, y);
                assert!(
                    matches!(k, MaterialKind::Water | MaterialKind::Gas),
                    "x={x} y={y} 水柱里混进了 {k:?}"
                );
                if k != MaterialKind::Water {
                    bubbles += 1;
                }
            }
            assert!(
                bubbles * 2 < height,
                "x={x} 水柱只有 {height} 格，气泡却占了 {bubbles} 格"
            );
            checked += 1;
        }
        // 阈值从"一半以上"降到"四分之一以上"：陆地占比 55% 之后，
        // 有水柱的列本来就只有四成左右，而其中还有一部分浅到不够 3 格。
        assert!(
            checked * 4 > W,
            "至少四分之一的列该有水柱，实际只有 {checked}"
        );
    }

    #[test]
    fn land_columns_are_solid_from_surface_down() {
        let (world, l) = build(W, H, SEED);
        for x in 0..W as isize {
            let top = l.surface[x as usize];
            if top < l.waterline as isize {
                // 露出水面的列：地表往下应当是沙或石，绝不能是水
                let kind = world.get(x, top + 1);
                assert!(
                    matches!(kind, MaterialKind::Sand | MaterialKind::Stone),
                    "x={x} 陆地下方是 {kind:?}，不该是水"
                );
                // 水面线高度上，这一列不该有水
                assert_ne!(
                    world.get(x, l.waterline as isize),
                    MaterialKind::Water,
                    "x={x} 已经露出水面，水面线上不该有水"
                );
            }
        }
    }

    #[test]
    fn every_seed_grows_land_above_the_waterline() {
        // 这是条**硬契约**，不是概率：第一座岛保证露出水面。
        // 早先按固定倍率随机时，有些种子一张图全是水，"海洋 + 岛屿"
        // 名不副实，预览图里连火都画不出来。
        for (w, h) in [(64, 64), (256, 256), (512, 512)] {
            for seed in 0..16u64 {
                let (_, l) = build(w, h, seed);
                assert!(
                    l.land_columns() > 0,
                    "{w}x{h} seed {seed}: 一张图全是水，没有岛露出水面"
                );
            }
        }
    }

    #[test]
    fn demo_world_uses_every_material() {
        // 预览图要能一眼看出调色板有没有配错，所以每种材质都得出现。
        //
        // 例外是**生命制造的**材质：尸体只能由细胞死亡产生，worldgen 永远
        // 不该产出它。用排除列表而不是"只查这几种"——将来新增地貌材质时，
        // 这条测试仍然会抓到"忘了铺进世界"。
        let life_made = [MaterialKind::Corpse];
        let (world, _) = build(512, 512, SEED);
        let census = world.census();
        for kind in MaterialKind::ALL {
            if life_made.contains(&kind) {
                continue;
            }
            assert!(
                census[kind.as_u8() as usize] > 0,
                "{} 在生成的世界里没出现",
                kind.name()
            );
        }
    }

    #[test]
    fn focus_top_puts_the_waterline_in_frame() {
        let (_, l) = build(512, 512, SEED);
        let vh = 40;
        let top = l.focus_top(vh);
        assert!(top <= l.waterline, "视野上沿应当在水面线之上");
        assert!(l.waterline < top + vh, "水面线应当落在视野之内");
    }

    #[test]
    fn focus_left_puts_land_in_frame() {
        // 判据从"最高的那一列在视野里"改成"视野里有陆地"。
        // 地形换成海岸线之后，最高点往往只是内陆一个不起眼的小丘，
        // 而玩家想看的是岸——钉住 peak_x 只会让这条测试在无关改动上变红。
        for seed in 0..24u64 {
            let (_, l) = build(512, 512, seed);
            let vw = 160;
            let left = l.focus_left(vw);
            let (lo, hi) = (left.min(512), (left + vw).min(512));
            let land = (lo..hi)
                .filter(|&x| l.surface[x] < l.waterline as isize)
                .count();
            assert!(
                land * 10 >= (hi - lo) * 3,
                "seed {seed}: 视野 [{lo},{hi}) 里只有 {land} 列陆地"
            );
        }
    }

    #[test]
    fn main_land_is_the_longest_run() {
        let l = WorldLayout {
            waterline: 10,
            seabed: 30,
            surface: vec![5, 5, 20, 5, 5, 5, 20, 20, 5],
        };
        assert_eq!(l.main_land(), Some((3, 6)));
        assert_eq!(l.main_land_center(), 4);

        // 全是水：没有陆地，退回最高那一列。
        let none = WorldLayout {
            waterline: 10,
            seabed: 30,
            surface: vec![20, 20, 20],
        };
        assert_eq!(none.main_land(), None);
        assert_eq!(none.main_land_center(), none.peak_x());

        // 首尾两列都是陆地，但**不该被接成一段**——世界是环面，
        // 而"最长的那片陆地"是给人看的，不是给寻路用的。
        let wrap = WorldLayout {
            waterline: 10,
            seabed: 30,
            surface: vec![5, 20, 5, 5],
        };
        assert_eq!(
            wrap.main_land(),
            Some((2, 4)),
            "首尾两列不该被接成一段（世界是环面，但「最长的那片陆地」是给人看的）"
        );
    }

    #[test]
    fn the_slope_is_never_steeper_than_two() {
        // **这是 v0.30 的核心验收。**
        //
        // 移动是四向离散的，所以相邻高差 > 1 就是一道必须靠纯垂直越过的墙；
        // 允许斜向之后，"水平 1 + 垂直 1"正好是 45°（坡度 1）。
        // 旧版地形的剖面最陡 17 格/列，于是鱼只会沿着崖壁蹭——
        // 玩家看到的是"水里的生物只朝一个方向游，撞在石壁上"。
        //
        // 跑 100 个种子 × 三种尺寸，一个都不许超。
        for (w, h) in [(64, 64), (256, 256), (512, 512)] {
            let mut worst = 0isize;
            for seed in 0..100u64 {
                let (_, l) = build(w, h, seed);
                for x in 1..w {
                    let d = (l.surface[x] - l.surface[x - 1]).abs();
                    worst = worst.max(d);
                }
            }
            assert!(
                worst <= MAX_SLOPE,
                "{w}x{h}: 100 个种子里最陡的坡是 {worst} 格/列"
            );
        }
    }

    #[test]
    fn land_and_water_split_the_world_roughly_in_half() {
        // 陆地占比是个**目标**（分位数切出来的），不是抽奖。
        // 侵蚀会把岸线往两边磨一点，所以给一个带宽。
        // 只测"垂直分辨率够用"的世界。64 格高的世界里陆地只有十几格预算，
        // `round()` 会把大片列拍到水面线上，占比失去意义——
        // 那不是地形生成的问题，是像素太少。
        for (w, h) in [(256, 256), (512, 512), (600, 450)] {
            let mut lo = 1.0f32;
            let mut hi = 0.0f32;
            let mut water_lo = 1.0f32;
            let mut water_hi = 0.0f32;
            for seed in 0..40u64 {
                let (world, l) = build(w, h, seed);
                let frac = l.land_columns() as f32 / w as f32;
                lo = lo.min(frac);
                hi = hi.max(frac);
                let census = world.census();
                let water =
                    census[MaterialKind::Water.as_u8() as usize] as f32 / (w * h) as f32;
                water_lo = water_lo.min(water);
                water_hi = water_hi.max(water);
            }
            assert!(
                lo >= 0.40 && hi <= 0.68,
                "{w}x{h}: 陆地占比 {lo:.2}..{hi:.2} 超出 0.40..0.68"
            );
            // 水格占总格数的比例**刻意很小**（实测 3%~7%）：512 高的世界里
            // 海只有 100 格深，其余全是基岩。这是坡度约束的直接代价——
            // 想深就得陡，而陡等于墙。所以这里只要求"有水"，不要求"很多水"。
            assert!(
                water_hi >= 0.02 && water_lo <= 0.15,
                "{w}x{h}: 水占比 {water_lo:.3}..{water_hi:.3} 不像一条浅海"
            );
        }
    }

    #[test]
    fn peak_x_is_the_highest_column() {
        let (_, l) = build(256, 256, SEED);
        let px = l.peak_x();
        for (x, &y) in l.surface.iter().enumerate() {
            assert!(y >= l.surface[px], "x={x} 比 peak_x={px} 还高");
        }
    }

    #[test]
    fn noise_is_deterministic_and_in_unit_range() {
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for i in 0..2000 {
            let v = fbm(i as f32 * 0.37, 12345);
            assert!((0.0..=1.0).contains(&v), "fbm 越界：{v}");
            lo = lo.min(v);
            hi = hi.max(v);
        }
        // 同一个输入必须给同一个值——世界生成的全部可复现性都压在这一条上。
        assert_eq!(fbm(3.7, 12345), fbm(3.7, 12345));
        assert_ne!(fbm(3.7, 12345), fbm(3.7, 12346), "换种子应当换形状");
        // 太窄说明噪声退化成常数，太宽说明分布不对。
        assert!(hi - lo > 0.3, "fbm 的取值范围只有 {lo}..{hi}");
    }

    #[test]
    fn erosion_flattens_a_cliff() {
        // 一道 40 格高的断崖。侵蚀之后相邻高差必须 <= MAX_SLOPE。
        let mut s = vec![50isize; 20];
        s.extend(std::iter::repeat(90isize).take(20));
        erode(&mut s, MAX_SLOPE, 4096);
        for x in 1..s.len() {
            let d = (s[x] - s[x - 1]).abs();
            assert!(d <= MAX_SLOPE, "x={x} 处还差 {d} 格");
        }
    }

    #[test]
    fn erosion_conserves_the_total_height() {
        // 侵蚀只是**搬运**，不是凭空削平。总量变了说明增量算错了方向——
        // 症状是"整块陆地慢慢沉进海里"，而画面上看起来只是"地形变缓了"。
        let mut s = vec![40isize; 10];
        s.extend(std::iter::repeat(120isize).take(10));
        s.extend(std::iter::repeat(60isize).take(10));
        let before: isize = s.iter().sum();
        erode(&mut s, MAX_SLOPE, 4096);
        let after: isize = s.iter().sum();
        assert_eq!(before, after, "侵蚀改了总高度");
    }

    #[test]
    fn erosion_does_not_prefer_a_direction() {
        // 就地改（而不是快照 + 统一应用）会让结果依赖遍历顺序，
        // 表现是"所有坡都朝一个方向"——看起来完全像噪声生成写错了。
        // 这里用一段对称的剖面，侵蚀之后必须仍然对称。
        let mut s: Vec<isize> = (0..21).map(|x| 40 + (x as isize - 10).abs() * 8).collect();
        erode(&mut s, MAX_SLOPE, 4096);
        for x in 0..10 {
            assert_eq!(s[x], s[20 - x], "x={x} 处左右不对称");
        }
    }
}
