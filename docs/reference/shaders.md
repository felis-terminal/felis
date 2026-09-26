---
title: Post-process shaders
sidebar:
  order: 4
---

When `shader.post` is unset, the terminal renders directly to the swapchain without an offscreen post-processing step.
When configured, terminal cells render into an intermediate offscreen texture that is sampled during a fullscreen
post-process pass. Architectural design rationale is documented in [pipeline.md](../explanation/rendering/pipeline.md).

## Selecting a shader

```toml
[shader]
post = { builtin = "trail" }
```

The `post` setting takes a table defining the shader source:

| Value                   | Resolves to                                                                        |
| ----------------------- | ---------------------------------------------------------------------------------- |
| unset                   | No post-process pass (direct swapchain render)                                     |
| `{ builtin = "trail" }` | Bundled cursor-trail effect                                                        |
| `{ file = "…" }`        | Path to WGSL file (expands `~/`; relative paths resolve relative to `config.toml`) |

`trail` is the only builtin shader. Bare strings (`post = "trail"`) are rejected. felis does not scan ambient
directories; execution is strictly limited to the specified builtin or file path.

## Entry point and bindings

felis provides a fullscreen triangle vertex stage. The shader file must provide exactly one fragment entry point:

```wgsl
@fragment
fn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32>
```

`uv` represents fragment coordinates normalized to `[0.0, 1.0]` with the origin at the **top-left** (wgpu texture
convention).

Bind group 0 exposes three resources:

| Binding | Declaration                                               |
| ------- | --------------------------------------------------------- |
| 0       | `var<uniform> u: FelisPostUniforms`                       |
| 1       | `var backbuffer: texture_2d<f32>` (the rendered frame)    |
| 2       | `var backbuffer_sampler: sampler` (linear, clamp-to-edge) |

Variable identifiers can be customized; bind group index, binding slots, and data types are fixed. Shaders may omit
unused bindings.

Only WGSL source text is accepted; compilation and validation are performed via naga. SPIR-V and other shading languages
are not supported.

## Contract v1

Uniform struct fields must be declared in order from the top. Shaders may truncate the struct after the last field read;
WebGPU calculates uniform buffer sizes from the shader declaration. Fields must not be reordered or omitted internally.

```wgsl
struct FelisPostUniforms {
    resolution_px: vec2<f32>,
    cell_size_px: vec2<f32>,
    time_s: f32,
    time_delta_s: f32,
    frame: u32,
    contract_version: u32,
    cursor_rect: vec4<f32>,
    prev_cursor_rect: vec4<f32>,
    cursor_color: vec4<f32>,
    trail_color: vec4<f32>,
    trail_corners_x: vec4<f32>,
    trail_corners_y: vec4<f32>,
    cursor_change_time_s: f32,
    cursor_style: u32,
    cursor_visible: u32,
    focused: u32,
    mouse_pos: vec4<f32>,
    mouse_buttons: vec4<f32>,
    background: vec4<f32>,
    foreground: vec4<f32>,
    selection_bg: vec4<f32>,
    palette: array<vec4<f32>, 256>,
}
```

| Field                                      | Meaning                                                                                            |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| `resolution_px`                            | Framebuffer dimensions in physical pixels                                                          |
| `cell_size_px`                             | Terminal cell dimensions in physical pixels                                                        |
| `time_s`                                   | Elapsed seconds since window renderer startup                                                      |
| `time_delta_s`                             | Elapsed seconds since previous frame                                                               |
| `frame`                                    | Rendered frame sequence number (initial frame is 0)                                                |
| `contract_version`                         | Contract schema version (currently `1`)                                                            |
| `cursor_rect`                              | `(x, y, width, height)` in UV of the painted cursor geometry                                       |
| `prev_cursor_rect`                         | Cursor geometry prior to last movement                                                             |
| `cursor_color`                             | Effective cursor color in linear RGBA                                                              |
| `trail_color`                              | Trail color in linear RGBA (alpha indicates opacity)                                               |
| `trail_corners_x` / `trail_corners_y`      | Four eased trail corners (0: top-right, 1: bottom-right, 2: bottom-left, 3: top-left)              |
| `cursor_change_time_s`                     | Value of `time_s` during most recent cursor repositioning                                          |
| `cursor_style`                             | `0`: block, `1`: underline, `2`: bar                                                               |
| `cursor_visible`                           | `1` when cursor was painted this frame; `0` otherwise                                              |
| `focused`                                  | `1` when window holds keyboard focus; `0` otherwise                                                |
| `mouse_pos`                                | `xy`: pointer UV position; `zw`: UV position of last left click                                    |
| `mouse_buttons`                            | Pressed state of left, right, middle (1.0 or 0.0); the fourth component is reserved and always 0.0 |
| `background`, `foreground`, `selection_bg` | Active theme palette colors in linear RGBA                                                         |
| `palette`                                  | Resolved 256-color palette in linear RGBA                                                          |

Color values use linear RGBA matching the render target. Spatial coordinates and extents use normalized UV space with
top-left origin.

## Animation and the frame clock

By default, felis does not run a continuous redraw loop for shaders. Frames render on demand when terminal state changes
(such as cursor motions or pointer input). Shaders depending strictly on `time_s` stay stationary on idle windows.

`trail_corners_*` coordinates are eased on the CPU on their own frame clock, by per-corner exponential decay: animation
arms on cursor leaps of two or more cells and stops once all corners settle within half a pixel of target bounds.

For continuous animations independent of terminal input, configure the `animate` property:

```toml
[shader]
post = { file = "~/fx/water.wgsl" }
animate = "focused"
```

| `shader.animate`  | Behavior                                                       |
| ----------------- | -------------------------------------------------------------- |
| `never` (default) | Redraw only on terminal updates and cursor transitions         |
| `focused`         | Continuous 60 Hz redraw loop while window holds keyboard focus |

When `focused` is set, redraw halts immediately upon focus loss. Continuous redrawing maintains steady ~16 ms
`time_delta_s` intervals at the cost of constant GPU activity.

## Failure handling

A shader is validated via naga when the client loads it, at startup and again on live reload, not by
`felis config check`, which only reports a missing file. A shader that fails parsing, type checking, or lacks `fs_post`
is rejected with an `ERROR` log message, and felis disables post-processing while keeping the terminal window
functional. On live reload the same holds: a shader that fails validation leaves the window with no post-processing
until the next reload.

If a `file` target does not exist, a single `WARN` diagnostic is emitted ([config.md](config.md)) and the pass is
skipped.

## Reference implementation

The bundled cursor trail shader (`crates/felis-render-wgpu/src/trail.wgsl`) demonstrates standard entry points and
binding layouts.
