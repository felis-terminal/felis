// Vertex stage of the post-process contract. felis owns this half;
// the user module supplies only `fs_post`, so a shader author never
// has to reproduce the fullscreen geometry or the UV convention.

struct PostVertex {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

// One oversized triangle rather than a two-triangle quad: the quad's
// shared diagonal makes GPUs shade the seam pixels twice, and the
// clipped surplus costs nothing.
@vertex
fn post_vs(@builtin(vertex_index) vertex_index: u32) -> PostVertex {
    let x = f32(i32(vertex_index) / 2) * 4.0 - 1.0;
    let y = f32(i32(vertex_index) & 1) * 4.0 - 1.0;
    var out: PostVertex;
    out.clip_pos = vec4<f32>(x, y, 0.0, 1.0);
    // UV origin is top-left (the wgpu texture convention), so y flips
    // against clip space.
    out.uv = vec2<f32>((x + 1.0) * 0.5, (1.0 - y) * 0.5);
    return out;
}
