// Custom Bevy 0.20 WESL 2D sprite shader for `example_10_bevy20_full_companion`.
import bevy_sprite_render::sprite_mesh::functions;

@vertex
fn vs_companion_pulse(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    let x = f32(i32(vertex_index & 1u) * 2 - 1);
    let y = f32(i32((vertex_index >> 1u) & 1u) * 2 - 1);
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_companion_pulse(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let wave = 0.5 + 0.5 * sin(frag_coord.x * 0.05 + frag_coord.y * 0.05);
    return vec4<f32>(0.18 + 0.45 * wave, 0.72, 0.96, 1.0);
}
