---
title: Post-process shaders
sidebar:
  order: 4
---

When `shader.post` is unset, the terminal renders directly to the swapchain without an offscreen post-processing step.
When configured, terminal cells render into an intermediate offscreen texture that a chain of fullscreen post-process
stages samples. Architectural design rationale is documented in [pipeline.md](../explanation/rendering/pipeline.md).

## Selecting shaders

```toml
[shader]
post = { builtin = "trail" }
```

`post` takes one stage table, or an array of up to 16 of them:

```toml
[shader]
post = [
  { builtin = "trail" },
  { file = "~/fx/flash.wgsl", params = { strength = 0.6 }, animation = { on = ["bell"], duration_ms = 300 } },
]
```

Each stage names its shader with exactly one selector:

| Selector                | Resolves to                                                                        |
| ----------------------- | ---------------------------------------------------------------------------------- |
| `{ builtin = "trail" }` | Bundled cursor-trail effect                                                        |
| `{ file = "…" }`        | Path to WGSL file (expands `~/`; relative paths resolve relative to `config.toml`) |

`trail` is the only builtin shader. Bare strings (`post = "trail"`) are rejected. felis does not scan ambient
directories; execution is strictly limited to the specified builtin or file path.

A stage table also takes two optional keys:

| Key         | Value                                              | Section                             |
| ----------- | -------------------------------------------------- | ----------------------------------- |
| `params`    | Table of WGSL `override` values by name            | [Params](#params)                   |
| `animation` | `{ on = [<event>, …], duration_ms = <1..=10000> }` | [Event animation](#event-animation) |

## Chaining stages

Stages run in array order. Stage 0 samples the cell pass through binding 1; every later stage samples the previous
stage's output through the same binding, and the last stage writes the frame. Entries past the sixteenth are ignored
with a warning. A stage that fails to load is left out and the rest still run, in order
([Failure handling](#failure-handling)); `stage_index` counts the stages that loaded.

## Entry point and bindings

felis provides a fullscreen triangle vertex stage. The shader file must provide exactly one fragment entry point:

```wgsl
@fragment
fn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32>
```

`uv` represents fragment coordinates normalized to `[0.0, 1.0]` with the origin at the **top-left** (wgpu texture
convention).

Only WGSL source text is accepted; compilation and validation are performed via naga. SPIR-V and other shading languages
are not supported.

Bind group 0 exposes four resources:

| Binding | Declaration                                                              |
| ------- | ------------------------------------------------------------------------ |
| 0       | `var<uniform> u: FelisPostUniforms`                                      |
| 1       | `var backbuffer: texture_2d<f32>` (the cell pass, or the previous stage) |
| 2       | `var backbuffer_sampler: sampler` (linear, clamp-to-edge)                |
| 3       | `var history: texture_2d<f32>` (this stage's own output, previous frame) |

Variable identifiers can be customized; bind group index, binding slots, and data types are fixed. Shaders may omit
unused bindings. A resource global at any other group or binding, or of another type or address space, refuses the
stage. The uniform struct may be no larger than the contract's.

### History

A stage that declares binding 3 reads its own output from the previous frame it rendered, sampled with binding 2. The
texture is allocated only for a stage that declares it. It reads as zero on the first frame, after a resize, and after
the stage is reloaded. The history advances only on frames that are drawn: nothing in the contract requests a frame, so
an effect that decays over time should scale by `time_delta_s` rather than by frame count.

## Params

A stage's `params` table sets the module's WGSL pipeline-overridable constants by name:

```wgsl
override strength: f32 = 0.5;
```

```toml
post = { file = "~/fx/flash.wgsl", params = { strength = 0.6 } }
```

Each value is checked against the override's declared type when the stage loads:

| Override type | Accepted TOML value                    |
| ------------- | -------------------------------------- |
| `bool`        | Boolean                                |
| `i32`         | Integer within `i32`                   |
| `u32`         | Integer within `0..=4294967295`        |
| `f32`         | Integer or float, finite, within `f32` |

Any other value (a float for an integer override, a string, a table, an array, a value out of range), a name the module
declares no `override` for, an override without a default that `params` leaves unset, and an override expression that
fails to evaluate with the given values each refuse the stage. An override declared with `@id(n)` is still set by its
identifier. `f16` overrides cannot be set. A changed value takes effect on config reload, which rebuilds the stage.

## Event animation

A stage's `animation` table plays a one-shot animation when an event fires:

```toml
post = { file = "~/fx/flash.wgsl", animation = { on = ["bell", "user:flash"], duration_ms = 300 } }
```

| `on` entry    | Fires on                                                                                   |
| ------------- | ------------------------------------------------------------------------------------------ |
| `bell`        | A bell from the attached session                                                           |
| `focus_in`    | The window gaining keyboard focus                                                          |
| `focus_out`   | The window losing keyboard focus                                                           |
| `left_press`  | A left mouse press inside the window, whether or not the program reports the mouse         |
| `user:<name>` | A `shader_event` binding with that `name` ([keybindings.md](keybindings.md#binding-kinds)) |

A `<name>` is 1 to 63 of `A-Z`, `a-z`, `0-9`, `_`, `.`, `-`. A matching event sets `animation_start_s` to the current
`time_s`, and `animation_progress` then runs linearly from 0 to 1 over `duration_ms`. felis draws frames at 60 Hz until
the run ends, draws one final frame at progress 1, and stops; progress holds at 1 until the next event. An event during
a run restarts it. The run plays whether or not the window has focus. Easing is the shader's: apply a curve to
`animation_progress`.

`duration_ms` is required and accepted in `1..=10000`, and `on` needs at least one entry. A missing or out-of-range
`duration_ms`, an empty `on`, or an unknown event leaves the stage loaded without animation, with a warning.

## Contract v2

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
    time_whole_s: u32,
    time_fract_s: f32,
    focus_change_time_s: f32,
    stage_index: u32,
    animation_start_s: f32,
    animation_progress: f32,
    _reserved: vec2<u32>,
}
```

| Field                                      | Meaning                                                                                            |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| `resolution_px`                            | Framebuffer dimensions in physical pixels                                                          |
| `cell_size_px`                             | Terminal cell dimensions in physical pixels                                                        |
| `time_s`                                   | Elapsed seconds since window renderer startup, unaffected by stage reloads                         |
| `time_delta_s`                             | Elapsed seconds since previous frame                                                               |
| `frame`                                    | Rendered frame sequence number (initial frame is 0)                                                |
| `contract_version`                         | Contract schema version (currently `2`)                                                            |
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
| `time_whole_s`                             | Whole seconds of `time_s`, exact where `time_s` has lost sub-millisecond precision                 |
| `time_fract_s`                             | Fractional second of `time_s`, in `[0, 1)`                                                         |
| `focus_change_time_s`                      | Value of `time_s` at the last focus gain or loss; `0` before any                                   |
| `stage_index`                              | This stage's 0-based position among the stages that loaded                                         |
| `animation_start_s`                        | Value of `time_s` when this stage's animation last started; `0` before any                         |
| `animation_progress`                       | Linear progress of this stage's animation: `0` before any start, holding `1` after its end         |
| `_reserved`                                | Pads the struct to its 16-byte size; always `0`                                                    |

Color values use linear RGBA matching the render target. Spatial coordinates and extents use normalized UV space with
top-left origin.

## Animation and the frame clock

By default, felis does not run a continuous redraw loop for shaders. Frames render on demand when terminal state changes
(such as cursor motions or pointer input). Shaders depending strictly on `time_s` stay stationary on idle windows.

`trail_corners_*` coordinates are eased on the CPU on their own frame clock, by per-corner exponential decay: animation
arms on cursor leaps of two or more cells and stops once all corners settle within half a pixel of target bounds.

A stage's [event animation](#event-animation) adds frames until its run ends. For continuous animations independent of
terminal input, configure the `animate` property:

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

A stage's shader is validated via naga when the client loads it, at startup and again on live reload, not by
`felis config check`. A shader that fails parsing or type checking, lacks `fs_post`, declares a resource outside the
binding table, or is refused by its `params` is rejected with an `ERROR` log message naming the stage. So is a shader
the GPU device then refuses when the stage is created, such as one that needs a device feature felis does not enable
(`enable f16;`) or whose `fs_post` inputs or output do not match the fixed vertex stage and color target. The chain runs
without that stage, and the window stays functional; with no stage left, there is no post-process pass. On live reload
the same holds until the next reload.

`felis config check` and the load log report what the config alone shows, each as a `WARN` diagnostic
([config.md](config.md)) that skips the stage: a `file` target that does not exist, an unknown `builtin` or selector,
and a `params` value that is not a boolean, integer, or float. An invalid `animation` table warns and leaves the stage
loaded without animation.

## Reference implementation

The bundled cursor trail shader (`crates/felis-render-wgpu/src/trail.wgsl`) demonstrates standard entry points and
binding layouts.
