// 全屏四边形：把世界纹理贴到窗口上。
//
// 输入是一张**与世界同尺寸**的纹理（一个像素 = 一个格子），
// 输出是把它按原宽高比、最近邻放大后铺满窗口，多余的部分留黑边（letterbox）。
//
// 之所以不用顶点缓冲：四边形的六个顶点完全由 vertex_index 算得出来，
// 少一次缓冲绑定，也少一个可能对不齐的顶点布局。

struct Uniforms {
    // 裁剪空间下的缩放与平移。xy = 缩放，zw = 平移。
    // letterbox 就是靠这两个值把 [-1,1]² 的四边形压扁或压窄。
    scale: vec2<f32>,
    offset: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var world_tex: texture_2d<f32>;
@group(0) @binding(2) var world_samp: sampler;

struct VsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32) -> VsOut {
    // 两个三角形拼成一个覆盖 [-1, 1]² 的四边形。
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

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(world_tex, world_samp, in.uv);
}
