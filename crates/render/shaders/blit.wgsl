// 全屏四边形：把世界纹理贴到窗口上，并按格内 uv 画细胞形状。
//
// 纹理里存的**不是颜色，是语义**：每个 u32 是一个格子的身份——
// 材质 id，或者 `0x8000_0000 | organism << 8 | cell_type`。
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
    _pad: f32,
    // 0..6 材质，8..14 细胞类型。只用 rgb，a 空着。
    palette: array<vec4<f32>, 16>,
};

@group(0) @binding(0) var<uniform> u: Scene;
@group(0) @binding(1) var world_tex: texture_2d<u32>;

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
    return (k >> 8u) & 0x7FFFFFu;
}

fn same_organism(a: u32, org: u32) -> bool {
    return is_cell(a) && organism_of(a) == org;
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

    let kind = key & 0xFFu;
    let base = u.palette[8u + kind].rgb;

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
    c = min(c, vec3<f32>(1.0));
    return vec4<f32>(mix(bg, c, body), 1.0);
}
