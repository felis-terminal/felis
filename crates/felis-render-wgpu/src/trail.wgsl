// Bundled cursor-trail shader — the reserved `shader.post = "trail"`.
//
// It is an ordinary user shader: felis loads it through the same
// contract, entry point, and bind layout a file from the user's
// `shaders/` directory goes through, so the bundled effect cannot
// drift ahead of what a user can write. Copy this file as the
// starting point for your own.

// Contract v1. Declare the fields you read, in this order, up to the
// last one you need; a shader written against an older (shorter)
// prefix keeps validating against a newer felis, which only appends.
struct FelisPostUniforms {
    resolution_px: vec2<f32>,
    cell_size_px: vec2<f32>,
    time_s: f32,
    time_delta_s: f32,
    frame: u32,
    contract_version: u32,
    // (x, y, width, height) in UV, origin top-left.
    cursor_rect: vec4<f32>,
    prev_cursor_rect: vec4<f32>,
    cursor_color: vec4<f32>,
    trail_color: vec4<f32>,
    // Trail quad corners, eased on the CPU toward `cursor_rect`.
    // Index order: 0 top-right, 1 bottom-right, 2 bottom-left,
    // 3 top-left.
    trail_corners_x: vec4<f32>,
    trail_corners_y: vec4<f32>,
    cursor_change_time_s: f32,
    cursor_style: u32,
    cursor_visible: u32,
    focused: u32,
    // xy = pointer position in UV (outside [0,1] when the pointer is
    // outside the window), zw = position of the last left press.
    mouse_pos: vec4<f32>,
    // Pressed state of left / right / middle / back, 1.0 or 0.0.
    mouse_buttons: vec4<f32>,
    background: vec4<f32>,
    foreground: vec4<f32>,
    selection_bg: vec4<f32>,
    palette: array<vec4<f32>, 256>,
}

@group(0) @binding(0) var<uniform> u: FelisPostUniforms;
@group(0) @binding(1) var backbuffer: texture_2d<f32>;
@group(0) @binding(2) var backbuffer_sampler: sampler;

// Signed area of the triangle (a, b, p); its sign says which side of
// the directed edge a→b the point lies on.
fn edge_sign(a: vec2<f32>, b: vec2<f32>, p: vec2<f32>) -> f32 {
    return (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
}

fn in_triangle(a: vec2<f32>, b: vec2<f32>, c: vec2<f32>, p: vec2<f32>) -> bool {
    let d0 = edge_sign(a, b, p);
    let d1 = edge_sign(b, c, p);
    let d2 = edge_sign(c, a, p);
    let any_neg = d0 < 0.0 || d1 < 0.0 || d2 < 0.0;
    let any_pos = d0 > 0.0 || d1 > 0.0 || d2 > 0.0;
    // Consistent sign (zeros included) means inside, whichever winding
    // the eased corners happen to have this frame.
    return !(any_neg && any_pos);
}

@fragment
fn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    let base = textureSample(backbuffer, backbuffer_sampler, uv);
    if u.cursor_visible == 0u {
        return base;
    }

    let c0 = vec2<f32>(u.trail_corners_x[0], u.trail_corners_y[0]);
    let c1 = vec2<f32>(u.trail_corners_x[1], u.trail_corners_y[1]);
    let c2 = vec2<f32>(u.trail_corners_x[2], u.trail_corners_y[2]);
    let c3 = vec2<f32>(u.trail_corners_x[3], u.trail_corners_y[3]);
    // Split into two triangles rather than testing four half-planes:
    // mid-flight the eased corners form a non-convex quad, which a
    // half-plane test would render as a bow tie.
    let inside = in_triangle(c0, c1, c2, uv) || in_triangle(c0, c2, c3, uv);
    if !inside {
        return base;
    }

    // The cell pass already painted the cursor itself; smearing the
    // trail color over it would make the caret the trail's brightest
    // point instead of its head.
    let lo = u.cursor_rect.xy;
    let hi = lo + u.cursor_rect.zw;
    if uv.x >= lo.x && uv.x <= hi.x && uv.y >= lo.y && uv.y <= hi.y {
        return base;
    }

    let alpha = clamp(u.trail_color.a, 0.0, 1.0);
    return vec4<f32>(mix(base.rgb, u.trail_color.rgb, alpha), base.a);
}
