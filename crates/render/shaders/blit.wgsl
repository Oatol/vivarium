// 全屏四边形：把世界纹理贴到窗口上，并按格内 uv 画细胞形状。
//
// 纹理里存的**不是颜色，是语义**：每个 u32 是一个格子的身份——
// 材质 id，或者 `0x8000_0000 | organism << 13 | organelles`。
// 颜色、形状、膜的轮廓光全在这里算。
//
// 为什么必须这样：一个细胞占一格。如果纹理里存的是压平后的 RGB，
// 那么"把细胞画成圆形"就永远做不到——一格只有一个像素，
// 一个像素画不出圆。把语义送上来，一格在屏幕上占 8 个像素时
// 才有地方画那颗圆。
//
// 顺带的好处：改外观不用碰 CPU 端，而 CPU 端的脏块检测也不用
// 知道"细胞长什么样"——它只比语义键。

struct Scene {
    // 裁剪空间下的缩放与平移（letterbox）。xy = 缩放，zw = 平移。
    scale: vec2<f32>,
    offset: vec2<f32>,
    // 视口：xy = 左上角的格坐标，zw = 覆盖多少格。
    view: vec4<f32>,
    // 世界尺寸（格）。
    world: vec2<f32>,
    // 一格在屏幕上占多少像素。用来算抗锯齿宽度。
    px_per_cell: f32,
    // 显示滤镜：0 材质 / 1 温度 / 2 光照 / 3 物种。
    // 必须与 `ViewMode::as_u32` 一致。
    view_mode: u32,
    // 0..15 材质，16..25 细胞器。只用 rgb，a 空着。
    //
    // 中间那一段空隙不是浪费：材质和细胞是两套编号，共用一个数组时必须
    // 划清界限。**这个偏移原本是 8，而材质涨到 9 种时 `Algae = 8` 正好
    // 撞上细胞的起点**——后写的细胞配色盖掉了先写的材质配色，
    // 藻被画成了嘴的橙红色。
    //
    // v0.41：细胞那一半从"7 种类型"变成"10 种细胞器"，长度 24 → 32。
    palette: array<vec4<f32>, 32>,
};

// 细胞器在 `palette` 里的起始下标。必须与 `palette.rs` 的同名常数一致。
const CELL_PALETTE_BASE: u32 = 16u;

// 有机体编号在视觉键里的起始位。必须与 `pixel.rs` 的 `ORGANISM_SHIFT` 一致。
const ORGANISM_SHIFT: u32 = 13u;
// 编号位宽 18 位。
const ORGANISM_MASK: u32 = 0x3FFFFu;

// 细胞器位。必须与 `organelle.rs` 的常量一致。
const DIGEST_BIT: u32 = 1u << 6u;
const ARMOR_BIT: u32 = 1u << 7u;
const GAS_BIT: u32 = 1u << 8u;
const EYE_BIT: u32 = 1u << 9u;
const MITO_BIT: u32 = 1u << 10u;
const BUILDER_BIT: u32 = 1u << 11u;
const ROOT_BIT: u32 = 1u << 12u;

// 混色时"每多一份功能暗多少"，以及最暗压到哪儿。
// 必须与 `palette.rs` 的 `SHADE_PER_EXTRA` / `SHADE_FLOOR` 一致。
const SHADE_PER_EXTRA: f32 = 0.10;
const SHADE_FLOOR: f32 = 0.62;

// 粗网格的边长。必须与 `field::AIR_CELL` 一致。
const AIR_CELL: f32 = 4.0;

// 滤镜编号。必须与 `ViewMode::as_u32` 一致。
const VIEW_MATERIAL: u32 = 0u;
const VIEW_TEMPERATURE: u32 = 1u;
const VIEW_LIGHT: u32 = 2u;
const VIEW_SPECIES: u32 = 3u;

@group(0) @binding(0) var<uniform> u: Scene;
@group(0) @binding(1) var world_tex: texture_2d<u32>;
// 场纹理：r = 光照（0..1），g = 温度 / 1200（0..1）。尺寸是粗网格。
@group(0) @binding(2) var field_tex: texture_2d<f32>;

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    // 两个三角形拼成一个覆盖 [-1, 1]² 的四边形。
    // 顶点由 vertex_index 算出来，不需要顶点缓冲。
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>(-1.0,  1.0),
        vec2<f32>( 1.0, -1.0),
        vec2<f32>( 1.0,  1.0),
    );

    let p = corners[vertex_index];

    var out: VsOut;
    out.clip_pos = vec4<f32>(p * u.scale + u.offset, 0.0, 1.0);
    // 纹理的第 0 行是世界的**顶部**，而裁剪空间的 y = +1 也在顶部，
    // 所以这里要把 y 翻过来，否则整个世界会上下颠倒。
    out.uv = vec2<f32>((p.x + 1.0) * 0.5, (1.0 - p.y) * 0.5);
    return out;
}

// 圆角矩形的有符号距离：内部为负，边界为 0。
fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) - r;
}

fn load_key(c: vec2<i32>) -> u32 {
    if (c.x < 0 || c.y < 0 || c.x >= i32(u.world.x) || c.y >= i32(u.world.y)) {
        return 0u;
    }
    return textureLoad(world_tex, c, 0).r;
}

fn is_cell(k: u32) -> bool {
    return (k & 0x80000000u) != 0u;
}

fn organism_of(k: u32) -> u32 {
    return (k >> ORGANISM_SHIFT) & ORGANISM_MASK;
}

// 一格的细胞器位掩码。低 13 位，最高位是"这是细胞"的标志，所以要先剥掉。
fn organelles_of(k: u32) -> u32 {
    return k & 0x1FFFu;
}

// 细胞器的混色。**必须与 `palette::organelle_rgb` 逐项一致。**
//
//   颜色 = (各基色的加权平均) × (1 - 0.10 × (份数 - 1))
//
// 权重：有等级的三个（叶绿体 / 鞭毛 / 刺）按等级算，开关类算 1 份，
// 线粒体**不参与**——它几乎每格都有，掺进来会把所有颜色拉白，
// 装甲 / 根 / 气囊就分不出来了。它只画一个白点（见 `fs_main`）。
fn organelle_color(bits: u32) -> vec3<f32> {
    var sum = vec3<f32>(0.0);
    var total = 0.0;

    let chloro = f32((bits >> 0u) & 3u);
    let flag = f32((bits >> 2u) & 3u);
    let sting = f32((bits >> 4u) & 3u);
    if (chloro > 0.0) {
        sum += u.palette[CELL_PALETTE_BASE + 0u].rgb * chloro;
        total += chloro;
    }
    if (flag > 0.0) {
        sum += u.palette[CELL_PALETTE_BASE + 1u].rgb * flag;
        total += flag;
    }
    if (sting > 0.0) {
        sum += u.palette[CELL_PALETTE_BASE + 2u].rgb * sting;
        total += sting;
    }
    if ((bits & DIGEST_BIT) != 0u) {
        sum += u.palette[CELL_PALETTE_BASE + 3u].rgb;
        total += 1.0;
    }
    if ((bits & ARMOR_BIT) != 0u) {
        sum += u.palette[CELL_PALETTE_BASE + 4u].rgb;
        total += 1.0;
    }
    if ((bits & GAS_BIT) != 0u) {
        sum += u.palette[CELL_PALETTE_BASE + 5u].rgb;
        total += 1.0;
    }
    if ((bits & EYE_BIT) != 0u) {
        sum += u.palette[CELL_PALETTE_BASE + 6u].rgb;
        total += 1.0;
    }
    if ((bits & BUILDER_BIT) != 0u) {
        sum += u.palette[CELL_PALETTE_BASE + 8u].rgb;
        total += 1.0;
    }
    if ((bits & ROOT_BIT) != 0u) {
        sum += u.palette[CELL_PALETTE_BASE + 9u].rgb;
        total += 1.0;
    }
    if (total <= 0.0) {
        // 只有线粒体的空细胞：中性灰，比背景亮，看得见但毫不抢眼。
        return vec3<f32>(0.376, 0.392, 0.424);
    }
    let shade = max(1.0 - SHADE_PER_EXTRA * (total - 1.0), SHADE_FLOOR);
    return (sum / total) * shade;
}

fn same_organism(a: u32, org: u32) -> bool {
    return is_cell(a) && organism_of(a) == org;
}

// ---------------------------------------------------------------------------
// 场：光照与温度
// ---------------------------------------------------------------------------

fn field_at(c: vec2<i32>) -> vec2<f32> {
    let n = vec2<i32>(textureDimensions(field_tex, 0));
    let p = clamp(c, vec2<i32>(0), n - vec2<i32>(1));
    return textureLoad(field_tex, p, 0).rg;
}

// 双线性采样。场是**粗**的（4 格一格），不插值的话滤镜上是一层 4 像素的
// 马赛克；而温度本来就该是连续的。四次 textureLoad 换掉一个采样器，
// 顺带也不用管 `Rg32Float` 不可过滤这件事。
fn field_smooth(world_pos: vec2<f32>) -> vec2<f32> {
    let g = world_pos / AIR_CELL - vec2<f32>(0.5);
    let base = vec2<i32>(floor(g));
    let f = g - floor(g);
    let a = field_at(base);
    let b = field_at(base + vec2<i32>(1, 0));
    let c = field_at(base + vec2<i32>(0, 1));
    let d = field_at(base + vec2<i32>(1, 1));
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}

// 热色带。色标照搬 TPT 的 `heatTable`——那套颜色在深色底上已经被
// 验证过二十年，而"从蓝到红"这种朴素渐变在中间段全是脏褐色。
//
// 返回值是**线性**的：色标本身是按 sRGB 写的，而着色器输出线性值。
// 这里用 `c*c` 近似 sRGB 解码（真解码是 `pow(c, 2.4)`）。
// 差多少：0.5 → 0.25 vs 0.218，肉眼在热力图上分辨不出来，
// 而它省掉每个像素三次 `pow`——那是一整帧里最贵的一条指令。
fn heat_ramp(t: f32) -> vec3<f32> {
    let x = clamp(t, 0.0, 1.0);
    var c: vec3<f32>;
    if (x < 0.05) {
        c = mix(vec3<f32>(0.169, 0.0, 1.0), vec3<f32>(0.0, 0.753, 1.0), x / 0.05);
    } else if (x < 0.08) {
        c = mix(vec3<f32>(0.0, 0.753, 1.0), vec3<f32>(0.0, 1.0, 0.922), (x - 0.05) / 0.03);
    } else if (x < 0.19) {
        c = mix(vec3<f32>(0.0, 1.0, 0.922), vec3<f32>(0.0, 1.0, 0.078), (x - 0.08) / 0.11);
    } else if (x < 0.25) {
        c = mix(vec3<f32>(0.0, 1.0, 0.078), vec3<f32>(0.294, 1.0, 0.0), (x - 0.19) / 0.06);
    } else if (x < 0.37) {
        c = mix(vec3<f32>(0.294, 1.0, 0.0), vec3<f32>(0.784, 1.0, 0.0), (x - 0.25) / 0.12);
    } else if (x < 0.45) {
        c = mix(vec3<f32>(0.784, 1.0, 0.0), vec3<f32>(1.0, 0.863, 0.0), (x - 0.37) / 0.08);
    } else if (x < 0.71) {
        c = mix(vec3<f32>(1.0, 0.863, 0.0), vec3<f32>(1.0, 0.0, 0.0), (x - 0.45) / 0.26);
    } else {
        c = mix(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(1.0, 0.0, 0.863), (x - 0.71) / 0.29);
    }
    return c * c;
}

// 光色带：黑 → 深蓝 → 青 → 暖白。同样返回线性值。
fn light_ramp(l: f32) -> vec3<f32> {
    let x = clamp(l, 0.0, 1.0);
    var c: vec3<f32>;
    if (x < 0.15) {
        c = mix(vec3<f32>(0.0), vec3<f32>(0.03, 0.08, 0.24), x / 0.15);
    } else if (x < 0.40) {
        c = mix(vec3<f32>(0.03, 0.08, 0.24), vec3<f32>(0.08, 0.35, 0.55), (x - 0.15) / 0.25);
    } else if (x < 0.70) {
        c = mix(vec3<f32>(0.08, 0.35, 0.55), vec3<f32>(0.35, 0.75, 0.78), (x - 0.40) / 0.30);
    } else {
        c = mix(vec3<f32>(0.35, 0.75, 0.78), vec3<f32>(1.0, 1.0, 0.94), (x - 0.70) / 0.30);
    }
    return c * c;
}

fn hsv2rgb(h: f32, s: f32, v: f32) -> vec3<f32> {
    let k = vec3<f32>(5.0, 3.0, 1.0);
    let p = abs(fract(vec3<f32>(h) + k / 6.0) * 6.0 - vec3<f32>(3.0));
    return v * mix(vec3<f32>(1.0), clamp(p - vec3<f32>(1.0), vec3<f32>(0.0), vec3<f32>(1.0)), s);
}

// 物种色：黄金角散布色相，和 `palette::organism_rgb` 同一个公式。
// 相邻编号的颜色差得最开，"两条鱼贴在一起"也能一眼分出是两条。
fn species_color(id: u32) -> vec3<f32> {
    let hue = f32((id * 137u) % 360u) / 360.0;
    let c = hsv2rgb(hue, 0.70, 1.0);
    return c * c;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let bg = u.palette[0].rgb;

    let world_pos = u.view.xy + in.uv * u.view.zw;
    let coord = vec2<i32>(floor(world_pos));
    if (coord.x < 0 || coord.y < 0
        || coord.x >= i32(u.world.x) || coord.y >= i32(u.world.y)) {
        return vec4<f32>(bg, 1.0);
    }

    let key = textureLoad(world_tex, coord, 0).r;

    // --- 场滤镜：不看材质，整屏都是场 -----------------------------------
    //
    // 温度 / 光照两个滤镜**完全忽略**材质和细胞。这不是偷懒：它们的用途是
    // "把一层不可见的数据变成可看的图"，掺进材质颜色就分不清"这一格是热的"
    // 还是"这一格是沙子"。
    if (u.view_mode == VIEW_TEMPERATURE) {
        let f = field_smooth(world_pos);
        return vec4<f32>(heat_ramp(f.y), 1.0);
    }
    if (u.view_mode == VIEW_LIGHT) {
        let f = field_smooth(world_pos);
        return vec4<f32>(light_ramp(f.x), 1.0);
    }
    // --- 物种滤镜：每个个体一个色相，材质压成暗剪影 ---------------------
    if (u.view_mode == VIEW_SPECIES) {
        if (!is_cell(key)) {
            let id = key & 0xFFu;
            // 材质压到 12%：地形还在，但绝不抢戏。
            // 0 号是空位，压完几乎等于背景——这正是"没有东西"该有的样子。
            return vec4<f32>(u.palette[id].rgb * 0.12, 1.0);
        }
        let c = species_color(organism_of(key)) * 0.92;
        return vec4<f32>(c, 1.0);
    }

    // 材质：直接查表。地形不需要形状，方块就是它该有的样子。
    // 水额外按深度压暗——一片纯色的蓝看不出这是"有深度的水"，
    // 而深度正是这个世界最重要的一个维度（光从上面来）。
    if (!is_cell(key)) {
        let id = key & 0xFFu;
        var c = u.palette[id].rgb;
        if (id == 3u) {
            let depth = clamp(world_pos.y / max(u.world.y, 1.0), 0.0, 1.0);
            c = c * (1.0 - 0.55 * depth);
        }
        return vec4<f32>(c, 1.0);
    }

    let local = fract(world_pos) - vec2<f32>(0.5);

    // 抗锯齿宽度：半个屏幕像素换算成格。
    // 下限 0.01 是为了放大时保住边缘的锐度；上限 0.12 是为了缩到全景时
    // 不把整格都泡进渐变——那样格子边界会消失，生物看起来像一团雾。
    let aa = clamp(0.5 / max(u.px_per_cell, 1.0), 0.01, 0.12);

    // 同族的邻居不画膜。
    //
    // 这是"一条鱼"和"一串彩色糖果"之间的分界线。身体是相邻的格子，
    // 如果每格都画完整的圆角边框，看到的就是一堆互不相干的方块；
    // 把内部边界去掉，三个细胞才融合成一块有轮廓的组织，
    // 而轮廓只出现在身体的外缘。
    //
    // 查四个邻居而不是维护一张边界表：着色器本来就逐像素跑，
    // 四次 textureLoad 打在同一张极小的纹理上，缓存全中。
    let org = organism_of(key);
    let kl = load_key(coord + vec2<i32>(-1, 0));
    let kr = load_key(coord + vec2<i32>( 1, 0));
    let ku = load_key(coord + vec2<i32>(0, -1));
    let kd = load_key(coord + vec2<i32>(0,  1));
    let same_l = same_organism(kl, org);
    let same_r = same_organism(kr, org);
    let same_u = same_organism(ku, org);
    let same_d = same_organism(kd, org);

    // 有同族的那一侧，把边界推到格子外面去——那一侧就没有"边"了。
    let lo = vec2<f32>(
        select(-0.5, -1.0, same_l),
        select(-0.5, -1.0, same_u),
    );
    let hi = vec2<f32>(
        select( 0.5,  1.0, same_r),
        select( 0.5,  1.0, same_d),
    );
    let mid = (lo + hi) * 0.5;
    let half = (hi - lo) * 0.5;

    let d = sd_round_box(local - mid, half, 0.22);
    let body = 1.0 - smoothstep(-aa, aa, d);
    if (body <= 0.0) {
        return vec4<f32>(bg, 1.0);
    }

    let bits = organelles_of(key);
    let base = organelle_color(bits);

    // 明暗按**到格心的距离**算，不是按到身体边界的距离。
    //
    // 用身体边界算（也就是 `d`）会得到一整块均匀的亮，细胞数都数不出来；
    // 用格子的圆角方块距离算（`d_cell`）会在格子边界处归零，于是每两颗
    // 细胞之间出现一条暗带——那等于把融合好的身体又切回一颗颗糖。
    // 到格心的距离没有这两个毛病：每颗细胞一个亮点，边界处亮度不掉。
    let core = 1.0 - smoothstep(0.0, 0.35, length(local));
    // 膜的轮廓光贴着**身体**的边界，不是每格的边界。
    let rim = smoothstep(-0.09, -0.02, d) * body;

    // 基础亮度压到 0.72 而不是 0.5：细胞边界处 core 归零，如果基础值太低，
    // 两颗细胞相接的地方会暗到和水几乎同色，融合出来的身体就断成两截。
    var c = base * (0.72 + 0.33 * core) + base * 0.35 * rim;

    // 线粒体：**一个白点**，不参与混色。
    //
    // 它几乎每格都有，所以它不该改变"这格是什么颜色"——但它确实是个
    // 可观测的差别：没有线粒体的细胞所有主动功能只有 25% 效率，
    // 那是一种"活着的石头"，玩家该看得出它和正常细胞的区别。
    //
    // 点画在格心偏右上，避开 `core` 的亮点；半径按 `aa` 走，
    // 缩到全景时它自然消失（那时一格才 1 像素，本来也画不下）。
    if ((bits & MITO_BIT) != 0u) {
        let dp = length(local - vec2<f32>(0.16, -0.16)) - 0.10;
        let dot = 1.0 - smoothstep(-aa, aa, dp);
        c = mix(c, u.palette[CELL_PALETTE_BASE + 7u].rgb, dot * 0.9);
    }

    c = min(c, vec3<f32>(1.0));
    return vec4<f32>(mix(bg, c, body), 1.0);
}
