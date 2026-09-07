// Fullscreen triangle + a deliberately expensive fragment shader.
//
// The loop bound comes from a uniform so the shader compiler cannot unroll it away, and the
// accumulator feeds back into itself so no iteration can be dead-code eliminated. The final
// value is written to the render target, so the whole chain is live.

struct Params {
    iters: u32,
    seed: f32,
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<uniform> params: Params;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i32(vi) / 2) * 4.0 - 1.0;
    let y = f32(i32(vi) & 1) * 4.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    var acc = vec4<f32>(pos.x * 0.001, pos.y * 0.001, params.seed, 1.0);
    for (var i: u32 = 0u; i < params.iters; i = i + 1u) {
        let k = f32(i) * 0.017 + params.seed;
        acc = fract(sin(acc * 12.9898 + vec4<f32>(k)) * 43758.5453);
        acc = acc * 0.997 + vec4<f32>(0.0015);
    }
    return acc;
}
