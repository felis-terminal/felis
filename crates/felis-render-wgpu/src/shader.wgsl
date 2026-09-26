// Two-pass shader for the cell renderer.
//
// Both passes share a unit-quad layout: the vertex shader synthesizes
// the four corners from `@builtin(vertex_index)` so we ship one draw
// call per pass with an instance buffer and no vertex buffer of our
// own. The strip order (vid 0..3 → TL, TR, BL, BR) matches a wgpu
// `TriangleStrip` topology.
//
// `Viewport` carries the framebuffer size in physical pixels so the
// instance origins (also in physical pixels) can be projected to clip
// space here rather than on the CPU. Keeping the uniform tiny (one
// vec2) means it can be re-uploaded on every frame without measurable
// cost.

struct Viewport {
    size_px: vec2<f32>,
    // Letterbox origin for same-user mirroring (see
    // docs/explanation/architecture/session-lifecycle.md): a constant pixel offset
    // applied to every instance so a grid smaller than the window renders
    // centered (or bottom-anchored when clipped) without touching any
    // CPU-side origin math.
    origin_px: vec2<f32>,
};

@group(0) @binding(0) var<uniform> viewport: Viewport;

fn corner_for_vertex(vid: u32) -> vec2<f32> {
    return vec2<f32>(f32(vid & 1u), f32((vid >> 1u) & 1u));
}

fn project(pos_px: vec2<f32>) -> vec4<f32> {
    let shifted = pos_px + viewport.origin_px;
    let clip = vec2<f32>(
        shifted.x / viewport.size_px.x * 2.0 - 1.0,
        1.0 - shifted.y / viewport.size_px.y * 2.0,
    );
    return vec4<f32>(clip, 0.0, 1.0);
}

// ---- Background pass ------------------------------------------------

struct BgInstance {
    @location(0) origin_px: vec2<f32>,
    @location(1) size_px: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct BgVsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn bg_vs(@builtin(vertex_index) vid: u32, inst: BgInstance) -> BgVsOut {
    let corner = corner_for_vertex(vid);
    var out: BgVsOut;
    out.clip_pos = project(inst.origin_px + corner * inst.size_px);
    out.color = inst.color;
    return out;
}

@fragment
fn bg_fs(in: BgVsOut) -> @location(0) vec4<f32> {
    return in.color;
}

// ---- Foreground (glyph) pass ----------------------------------------

struct FgInstance {
    @location(0) origin_px: vec2<f32>,
    @location(1) size_px: vec2<f32>,
    @location(2) uv_min: vec2<f32>,
    @location(3) uv_max: vec2<f32>,
    @location(4) color: vec4<f32>,
    // 1.0 → sample the RGBA color atlas (emoji); 0.0 → the coverage
    // atlas tinted by `color`.
    @location(5) is_color: f32,
};

struct FgVsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) is_color: f32,
};

@group(1) @binding(0) var atlas_tex: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;
// Color-emoji atlas (Rgba8UnormSrgb). Shares the sampler above.
@group(1) @binding(2) var color_atlas_tex: texture_2d<f32>;

@vertex
fn fg_vs(@builtin(vertex_index) vid: u32, inst: FgInstance) -> FgVsOut {
    let corner = corner_for_vertex(vid);
    var out: FgVsOut;
    out.clip_pos = project(inst.origin_px + corner * inst.size_px);
    out.uv = mix(inst.uv_min, inst.uv_max, corner);
    out.color = inst.color;
    out.is_color = inst.is_color;
    return out;
}

@fragment
fn fg_fs(in: FgVsOut) -> @location(0) vec4<f32> {
    if (in.is_color > 0.5) {
        // Color emoji: the atlas holds straight (non-premultiplied)
        // sRGB RGBA, so the sampler returns linear color. Composite the
        // glyph's own color — modulated only by the instance alpha so a
        // dimmed cell can fade it — never the foreground tint. This is
        // what stops a button-tile emoji (⏸/⏺) collapsing to a solid
        // foreground square.
        let c = textureSample(color_atlas_tex, atlas_sampler, in.uv);
        return vec4<f32>(c.rgb, c.a * in.color.a);
    }
    // Mask glyph: the atlas stores 8-bit alpha in the red channel. Tint
    // by the instance color and modulate the alpha so the bg pass shows
    // through anti-aliased pixels.
    let coverage = textureSample(atlas_tex, atlas_sampler, in.uv).r;
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}

// ---- Decoration pass (SGR underlines / strikethrough / overline) ----
//
// Procedural line decorations, per docs/explanation/rendering/pipeline.md
// ("Two passes, procedural decorations"): straight lines (single /
// double underline, strikethrough, overline) are solid quads, while the
// curly / dotted / dashed underline shapes are computed in the fragment
// shader (a sine for curly, a modulo for dotted / dashed) rather than
// stored as atlas sprites. `pos` carries the grid-space pixel position
// (linearly interpolated, so continuous across cell quads) which gives
// the pattern phase a seamless run — a dashed underline spanning several
// cells reads as one dashed line, not a per-cell restart.

struct DecoInstance {
    @location(0) origin_px: vec2<f32>,
    @location(1) size_px: vec2<f32>,
    @location(2) color: vec4<f32>,
    // 0 solid, 1 dotted, 2 dashed, 3 curly.
    @location(3) kind: f32,
    @location(4) thickness_px: f32,
    // Pattern period (dotted / dashed) or sine wavelength (curly), px.
    @location(5) period_px: f32,
};

struct DecoVsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) @interpolate(flat) kind: f32,
    @location(2) @interpolate(flat) thickness_px: f32,
    @location(3) @interpolate(flat) period_px: f32,
    // Curly-wave center line and amplitude, both grid-space px.
    @location(4) @interpolate(flat) band_center_y: f32,
    @location(5) @interpolate(flat) amplitude: f32,
    // Grid-space pixel position of this fragment (perspective-correct
    // linear), used for the pattern phase and the curly distance test.
    @location(6) pos: vec2<f32>,
};

@vertex
fn deco_vs(@builtin(vertex_index) vid: u32, inst: DecoInstance) -> DecoVsOut {
    let corner = corner_for_vertex(vid);
    let vpos = inst.origin_px + corner * inst.size_px;
    var out: DecoVsOut;
    out.clip_pos = project(vpos);
    out.color = inst.color;
    out.kind = inst.kind;
    out.thickness_px = inst.thickness_px;
    out.period_px = inst.period_px;
    out.band_center_y = inst.origin_px.y + inst.size_px.y * 0.5;
    // The straight-line kinds ship a quad exactly `thickness` tall, so
    // this is zero for them; the curly kind ships a taller band and the
    // wave swings ± this within it.
    out.amplitude = max((inst.size_px.y - inst.thickness_px) * 0.5, 0.0);
    out.pos = vpos;
    return out;
}

@fragment
fn deco_fs(in: DecoVsOut) -> @location(0) vec4<f32> {
    var coverage = 1.0;
    let half_t = in.thickness_px * 0.5;
    if (in.kind < 0.5) {
        // Solid: the quad is exactly the line rectangle.
        coverage = 1.0;
    } else if (in.kind < 1.5) {
        // Dotted: on for the first half of each period.
        let phase = fract(in.pos.x / max(in.period_px, 1.0));
        coverage = select(0.0, 1.0, phase < 0.5);
    } else if (in.kind < 2.5) {
        // Dashed: on for the first two-thirds of each period.
        let phase = fract(in.pos.x / max(in.period_px, 1.0));
        coverage = select(0.0, 1.0, phase < 0.66);
    } else {
        // Curly: coverage from the distance to a sine wave, softened by
        // ~1 px so the wave edges antialias.
        let two_pi = 6.2831853;
        let wave_y = in.band_center_y
            + in.amplitude * sin(in.pos.x / max(in.period_px, 1.0) * two_pi);
        let dist = abs(in.pos.y - wave_y);
        coverage = 1.0 - smoothstep(half_t - 0.75, half_t + 0.75, dist);
    }
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}

// ---- Image pass (Kitty graphics) -------------------------------------

struct ImgInstance {
    @location(0) origin_px: vec2<f32>,
    @location(1) size_px: vec2<f32>,
    @location(2) uv_min: vec2<f32>,
    @location(3) uv_max: vec2<f32>,
    @location(4) opacity: f32,
};

struct ImgVsOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) opacity: f32,
};

@group(1) @binding(0) var image_tex: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

@vertex
fn image_vs(@builtin(vertex_index) vid: u32, inst: ImgInstance) -> ImgVsOut {
    let corner = corner_for_vertex(vid);
    var out: ImgVsOut;
    out.clip_pos = project(inst.origin_px + corner * inst.size_px);
    out.uv = mix(inst.uv_min, inst.uv_max, corner);
    out.opacity = inst.opacity;
    return out;
}

@fragment
fn image_fs(in: ImgVsOut) -> @location(0) vec4<f32> {
    // Pixel format on the GPU side is `Rgba8UnormSrgb`, so the
    // sampler returns linear-space RGBA. The fragment writes back
    // into the sRGB surface — wgpu re-encodes on write — so per-
    // pixel maths is in linear and gamma stays correct end-to-end.
    let sample = textureSample(image_tex, image_sampler, in.uv);
    return vec4<f32>(sample.rgb, sample.a * in.opacity);
}
