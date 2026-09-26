// Atlas uploads drawn as quads: each instance covers its destination
// rectangle and every fragment fetches its own texel from the storage
// buffer, so the write is a render pass rather than a buffer copy.

struct UploadInstance {
    @location(0) dst_origin: vec2<u32>,
    @location(1) extent: vec2<u32>,
    @location(2) target_size: vec2<u32>,
    // x: byte offset of the first row, y: row stride in bytes.
    @location(3) src: vec2<u32>,
};

struct UploadVsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) @interpolate(flat) dst_origin: vec2<u32>,
    @location(1) @interpolate(flat) src: vec2<u32>,
};

@group(0) @binding(0) var<storage, read> texels: array<u32>;

@vertex
fn upload_vs(@builtin(vertex_index) vid: u32, inst: UploadInstance) -> UploadVsOut {
    let corner = vec2<f32>(f32(vid & 1u), f32((vid >> 1u) & 1u));
    let px = vec2<f32>(inst.dst_origin) + corner * vec2<f32>(inst.extent);
    let size = vec2<f32>(inst.target_size);
    var out: UploadVsOut;
    out.pos = vec4<f32>(px.x / size.x * 2.0 - 1.0, 1.0 - px.y / size.y * 2.0, 0.0, 1.0);
    out.dst_origin = inst.dst_origin;
    out.src = inst.src;
    return out;
}

fn texel_byte_index(in: UploadVsOut, texel_bytes: u32) -> u32 {
    let p = vec2<u32>(in.pos.xy) - in.dst_origin;
    return in.src.x + p.y * in.src.y + p.x * texel_bytes;
}

@fragment
fn upload_r8_fs(in: UploadVsOut) -> @location(0) vec4<f32> {
    let i = texel_byte_index(in, 1u);
    let byte = (texels[i >> 2u] >> ((i & 3u) * 8u)) & 0xffu;
    return vec4<f32>(f32(byte) / 255.0, 0.0, 0.0, 0.0);
}

@fragment
fn upload_rgba8_fs(in: UploadVsOut) -> @location(0) vec4<f32> {
    return unpack4x8unorm(texels[texel_byte_index(in, 4u) >> 2u]);
}
