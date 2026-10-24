//! Post-process stage: user-replaceable fragment pass.
//!
//! [`PostUniforms`] grows by appending at the tail: WebGPU sizes uniform bindings from the
//! shader's declaration, so older shaders continue validating. Never reorder or resize fields.

use std::num::NonZeroU64;
use std::time::Instant;

use bytemuck::{Pod, Zeroable};

use crate::buffer_ring::{UniformRing, UploadMode};
use felis_protocol::messages::CursorStyle;
use wgpu::{
    AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBindingType, ColorTargetState, ColorWrites, CommandEncoder, Device, Extent3d,
    FilterMode, FragmentState, LoadOp, MultisampleState, Operations, PipelineLayoutDescriptor,
    PrimitiveState, Queue, RenderPipeline, RenderPipelineDescriptor, Sampler, SamplerBindingType,
    SamplerDescriptor, ShaderModule, ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp,
    Texture, TextureDescriptor, TextureDimension, TextureFormat, TextureSampleType, TextureUsages,
    TextureView, TextureViewDescriptor, TextureViewDimension, VertexState,
    naga::{
        AddressSpace, ImageClass, ImageDimension, Module, ResourceBinding, ScalarKind, TypeInner,
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    },
};

use felis_render_core::palette::ResolvedTheme;

/// Reported to the shader as `contract_version` so an effect can branch
/// on a field it is unsure of rather than fail to load.
pub const CONTRACT_VERSION: u32 = 2;

/// Fragment entry point every post-process shader must define.
pub const POST_ENTRY_POINT: &str = "fs_post";

pub const POST_VERTEX_SOURCE: &str = include_str!("post.wgsl");

/// What `shader.post = { builtin = "trail" }` loads. It goes through the same path as
/// a user file, so the built-in effect cannot use a contract the user
/// cannot.
pub const TRAIL_SHADER_SOURCE: &str = include_str!("trail.wgsl");

/// Rejection is not fatal: the caller keeps the previous stage (or
/// none) and surfaces the message.
#[derive(Debug, thiserror::Error)]
pub enum PostShaderError {
    #[error("parse WGSL: {0}")]
    Parse(String),
    #[error("validate WGSL: {0}")]
    Validate(String),
    #[error("missing `@fragment fn {POST_ENTRY_POINT}`")]
    MissingEntryPoint,
    #[error("binding: {0}")]
    Binding(String),
    #[error("device rejected the stage: {0}")]
    Device(String),
    #[error("an offscreen renderer has no post-process stage")]
    Offscreen,
}

/// CPU-side mirror of the WGSL `FelisPostUniforms` struct. Field order
/// is the contract (append-only, see the module doc).
#[repr(C)]
#[derive(Copy, Clone, Debug, Pod, Zeroable)]
pub struct PostUniforms {
    /// Framebuffer size in physical pixels.
    pub resolution_px: [f32; 2],
    /// Cell size in physical pixels.
    pub cell_size_px: [f32; 2],
    /// Seconds since the renderer started.
    pub time_s: f32,
    /// Seconds since the previous frame.
    pub time_delta_s: f32,
    /// Frames rendered since startup, first frame `0`.
    pub frame: u32,
    /// [`CONTRACT_VERSION`].
    pub contract_version: u32,
    /// Cursor rectangle as `(x, y, width, height)` in UV, origin
    /// top-left; the rectangle the cell pass painted.
    pub cursor_rect: [f32; 4],
    /// [`Self::cursor_rect`] before the most recent move; equal to it
    /// until the cursor has moved once.
    pub prev_cursor_rect: [f32; 4],
    /// Effective cursor color (linear RGBA).
    pub cursor_color: [f32; 4],
    /// Trail color (linear RGBA); alpha is the trail's opacity.
    pub trail_color: [f32; 4],
    /// X coordinates of the four eased trail corners, index order
    /// 0 top-right, 1 bottom-right, 2 bottom-left, 3 top-left.
    pub trail_corners_x: [f32; 4],
    /// Y coordinates of the same four corners.
    pub trail_corners_y: [f32; 4],
    /// Timestamp on `time_s`'s clock of the last cursor move.
    pub cursor_change_time_s: f32,
    /// [`CursorStyle`] cast to the shader contract's numbering
    /// (`docs/reference/shaders.md`); the const assert below pins it.
    pub cursor_style: u32,
    /// `1` when the cursor is currently painted (visible, focused, in
    /// bounds, at the live bottom), else `0`.
    pub cursor_visible: u32,
    /// `1` while the window holds keyboard focus.
    pub focused: u32,
    /// `xy` pointer position in UV, `zw` position of the last left
    /// press. Components outside `[0, 1]` mean the pointer is outside
    /// the window.
    pub mouse_pos: [f32; 4],
    /// Pressed state of left / right / middle, `1.0` or `0.0`. The
    /// fourth component is reserved by the v1 contract and always `0.0`.
    pub mouse_buttons: [f32; 4],
    /// Theme background (linear RGBA).
    pub background: [f32; 4],
    /// Theme foreground (linear RGBA).
    pub foreground: [f32; 4],
    /// Selection background (linear RGBA).
    pub selection_bg: [f32; 4],
    /// The effective 256-color palette (linear RGBA).
    pub palette: [[f32; 4]; 256],
    /// Whole seconds of [`Self::time_s`], exact long after `time_s` has
    /// lost sub-millisecond precision.
    pub time_whole_s: u32,
    /// The fractional second, in `[0, 1)`.
    pub time_fract_s: f32,
    /// `time_s` at the last focus gain or loss; `0` before any.
    pub focus_change_time_s: f32,
    /// This stage's 0-based position in the chain.
    pub stage_index: u32,
    /// `time_s` when this stage's animation last started; `0` before any.
    pub animation_start_s: f32,
    /// Linear progress of this stage's animation: `0` before any start,
    /// holding `1` after its end.
    pub animation_progress: f32,
    /// WGSL `_reserved`: pads the struct to the 16-byte multiple WGSL rounds
    /// it to; always 0.
    pub reserved: [u32; 2],
}

// `docs/reference/shaders.md` documents these numbers; a `CursorStyle`
// reorder must fail here, not in user shaders reading `cursor_style`.
const _: () = {
    assert!(CursorStyle::Block as u32 == 0);
    assert!(CursorStyle::Underline as u32 == 1);
    assert!(CursorStyle::Bar as u32 == 2);
};

impl Default for PostUniforms {
    fn default() -> Self {
        Self {
            resolution_px: [1.0, 1.0],
            cell_size_px: [1.0, 1.0],
            time_s: 0.0,
            time_delta_s: 0.0,
            frame: 0,
            contract_version: CONTRACT_VERSION,
            cursor_rect: [0.0; 4],
            prev_cursor_rect: [0.0; 4],
            cursor_color: [0.0; 4],
            trail_color: [0.0; 4],
            trail_corners_x: [0.0; 4],
            trail_corners_y: [0.0; 4],
            cursor_change_time_s: 0.0,
            cursor_style: 0,
            cursor_visible: 0,
            focused: 1,
            mouse_pos: OUTSIDE_WINDOW,
            mouse_buttons: [0.0; 4],
            background: [0.0; 4],
            foreground: [0.0; 4],
            selection_bg: [0.0; 4],
            palette: [[0.0; 4]; 256],
            time_whole_s: 0,
            time_fract_s: 0.0,
            focus_change_time_s: 0.0,
            stage_index: 0,
            animation_start_s: 0.0,
            animation_progress: 0.0,
            reserved: [0; 2],
        }
    }
}

/// One origin for every stage, owned by the renderer: a stage rebuilt by a
/// live reload keeps the chain's timeline instead of restarting its effects
/// at zero.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PostClock {
    origin: Instant,
    last_frame: Instant,
    frame: u32,
    focus_changed_at: Option<Instant>,
}

/// The largest `f32` below `1.0`: a subsecond of 999 999 999 ns rounds up
/// to exactly `1.0` in `f32`, outside the documented `[0, 1)`.
const BELOW_ONE: f32 = 1.0 - f32::EPSILON / 2.0;

impl PostClock {
    pub(crate) const fn new(origin: Instant) -> Self {
        Self {
            origin,
            last_frame: origin,
            frame: 0,
            focus_changed_at: None,
        }
    }

    pub(crate) const fn focus_changed(&mut self, at: Instant) {
        self.focus_changed_at = Some(at);
    }

    /// Seconds from the origin on `time_s`'s scale.
    pub(crate) fn offset_s(&self, at: Instant) -> f32 {
        at.saturating_duration_since(self.origin).as_secs_f32()
    }

    pub(crate) fn tick(&mut self, now: Instant) -> FrameTime {
        let since = now.saturating_duration_since(self.origin);
        let time = FrameTime {
            now,
            time_s: since.as_secs_f32(),
            time_whole_s: u32::try_from(since.as_secs()).unwrap_or(u32::MAX),
            time_fract_s: (f64::from(since.subsec_nanos()) / 1e9) as f32,
            time_delta_s: now.saturating_duration_since(self.last_frame).as_secs_f32(),
            frame: self.frame,
            focus_change_time_s: self.focus_changed_at.map_or(0.0, |at| self.offset_s(at)),
        };
        self.last_frame = now;
        self.frame = self.frame.wrapping_add(1);
        FrameTime {
            time_fract_s: time.time_fract_s.min(BELOW_ONE),
            ..time
        }
    }
}

/// [`PostClock`]'s reading for one frame, shared by every stage in it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameTime {
    pub(crate) now: Instant,
    pub(crate) time_s: f32,
    pub(crate) time_whole_s: u32,
    pub(crate) time_fract_s: f32,
    pub(crate) time_delta_s: f32,
    pub(crate) frame: u32,
    pub(crate) focus_change_time_s: f32,
}

/// Out of `[0, 1]` on both axes, per the contract.
const OUTSIDE_WINDOW: [f32; 4] = [-1.0, -1.0, -1.0, -1.0];

/// Trail geometry, eased on the CPU rather than in the shader so the
/// host knows when the animation is over and can stop redrawing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrailState {
    pub cursor_rect: [f32; 4],
    pub prev_cursor_rect: [f32; 4],
    pub corners_x: [f32; 4],
    pub corners_y: [f32; 4],
    pub seconds_since_change: f32,
}

impl Default for TrailState {
    fn default() -> Self {
        Self {
            cursor_rect: [0.0; 4],
            prev_cursor_rect: [0.0; 4],
            corners_x: [0.0; 4],
            corners_y: [0.0; 4],
            seconds_since_change: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MouseState {
    /// Pointer position in UV, or outside `[0, 1]` when the pointer
    /// left the window.
    pub pos_uv: [f32; 2],
    /// Same convention.
    pub last_press_uv: [f32; 2],
    pub buttons: [f32; 4],
}

impl Default for MouseState {
    fn default() -> Self {
        Self {
            pos_uv: [-1.0, -1.0],
            last_press_uv: [-1.0, -1.0],
            buttons: [0.0; 4],
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct PostFrame<'a> {
    pub cell_size_px: [f32; 2],
    pub theme: &'a ResolvedTheme,
    pub cursor_color: [f32; 4],
    pub cursor_style: CursorStyle,
    pub cursor_visible: bool,
    pub focused: bool,
}

/// One entry of the chain the client resolved from `shader.post`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostStageSource {
    /// What the logs call the stage (a path, or the builtin's name).
    pub label: String,
    pub wgsl: String,
}

/// What every stage of a chain shares.
pub(crate) struct PostShared {
    bind_group_layout: BindGroupLayout,
    vertex_module: ShaderModule,
    sampler: Sampler,
    format: TextureFormat,
    upload_mode: UploadMode,
}

impl PostShared {
    pub(crate) fn new(device: &Device, format: TextureFormat, upload_mode: UploadMode) -> Self {
        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("felis post bgl"),
            entries: &post_bind_group_layout_entries(),
        });
        let vertex_module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("felis post vertex"),
            source: ShaderSource::Wgsl(POST_VERTEX_SOURCE.into()),
        });
        // Clamped, not repeating: an effect that samples a neighborhood
        // near an edge must not wrap the opposite side of the screen in.
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("felis post sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..Default::default()
        });
        Self {
            bind_group_layout,
            vertex_module,
            sampler,
            format,
            upload_mode,
        }
    }

    pub(crate) const fn format(&self) -> TextureFormat {
        self.format
    }
}

/// The views one stage reads and writes this frame.
#[derive(Clone, Copy)]
pub(crate) struct StageIo<'a> {
    pub(crate) input: &'a TextureView,
    pub(crate) output: &'a TextureView,
    pub(crate) resolution_px: [f32; 2],
}

pub(crate) struct PostStage {
    pipeline: RenderPipeline,
    uniforms_ring: UniformRing,
    uniforms: PostUniforms,
}

impl PostStage {
    /// The device is the final gate: the checks before it cannot know the
    /// device's enabled features or match the fragment interface against
    /// the fixed vertex stage, and an uncaptured device error panics, so
    /// the module and pipeline are created inside a validation scope.
    pub(crate) fn new(
        device: &Device,
        shared: &PostShared,
        source: &PostStageSource,
    ) -> Result<Self, PostShaderError> {
        validate_post_shader(&source.wgsl)?;
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let fragment_module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("felis post fragment"),
            source: ShaderSource::Wgsl(source.wgsl.as_str().into()),
        });
        let pipeline = build_post_pipeline(
            device,
            &shared.bind_group_layout,
            &shared.vertex_module,
            &fragment_module,
            shared.format,
        );
        if let Some(error) = crate::warmup::block_on(scope.pop()) {
            return Err(PostShaderError::Device(error.to_string()));
        }
        Ok(Self {
            pipeline,
            uniforms_ring: UniformRing::new("felis post uniforms", shared.upload_mode),
            uniforms: PostUniforms::default(),
        })
    }

    pub(crate) fn submitted(&mut self, index: &wgpu::SubmissionIndex) {
        self.uniforms_ring.submitted(index);
    }

    /// After a view the bind group holds was replaced.
    pub(crate) fn invalidate_bind_group(&mut self) {
        self.uniforms_ring.invalidate();
    }

    pub(crate) fn encode(
        &mut self,
        encoder: &mut CommandEncoder,
        device: &Device,
        queue: &Queue,
        shared: &PostShared,
        io: StageIo<'_>,
        inputs: StageInputs<'_>,
    ) {
        self.uniforms.write_frame(io.resolution_px, &inputs);
        self.uniforms_ring
            .write(device, queue, bytemuck::bytes_of(&self.uniforms));
        let (layout, sampler) = (&shared.bind_group_layout, &shared.sampler);
        self.uniforms_ring
            .prepare_bind_group(|buf| create_bind_group(device, layout, buf, io.input, sampler));
        let Some(bind_group) = self.uniforms_ring.bind_group() else {
            return;
        };

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("felis post pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: io.output,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    // Not `Clear`: the fullscreen triangle writes every
                    // pixel.
                    load: LoadOp::Load,
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// What one stage's uniforms are built from, beyond its own state.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StageInputs<'a> {
    pub(crate) time: FrameTime,
    pub(crate) frame: PostFrame<'a>,
    pub(crate) trail: TrailState,
    pub(crate) mouse: MouseState,
    pub(crate) stage_index: u32,
}

impl PostUniforms {
    fn write_frame(&mut self, resolution_px: [f32; 2], inputs: &StageInputs<'_>) {
        let StageInputs {
            time,
            frame,
            trail,
            mouse,
            stage_index,
        } = *inputs;
        self.resolution_px = resolution_px;
        self.time_s = time.time_s;
        self.time_delta_s = time.time_delta_s;
        self.frame = time.frame;
        self.contract_version = CONTRACT_VERSION;
        self.cell_size_px = frame.cell_size_px;
        self.cursor_rect = trail.cursor_rect;
        self.prev_cursor_rect = trail.prev_cursor_rect;
        self.trail_corners_x = trail.corners_x;
        self.trail_corners_y = trail.corners_y;
        // The client reports an age, not a timestamp, so the two
        // processes' clocks stay out of the contract.
        self.cursor_change_time_s = time.time_s - trail.seconds_since_change;
        self.cursor_color = frame.cursor_color;
        self.trail_color = frame.cursor_color;
        self.cursor_style = frame.cursor_style as u32;
        self.cursor_visible = u32::from(frame.cursor_visible);
        self.focused = u32::from(frame.focused);
        self.mouse_pos = [
            mouse.pos_uv[0],
            mouse.pos_uv[1],
            mouse.last_press_uv[0],
            mouse.last_press_uv[1],
        ];
        self.mouse_buttons = mouse.buttons;
        self.background = frame.theme.bg;
        self.foreground = frame.theme.fg;
        self.selection_bg = felis_render_core::palette::SELECTION_BG;
        self.palette = frame.theme.palette;
        self.time_whole_s = time.time_whole_s;
        self.time_fract_s = time.time_fract_s;
        self.focus_change_time_s = time.focus_change_time_s;
        self.stage_index = stage_index;
    }
}

/// `@group(0)` of the post pass: uniform block (0), offscreen
/// backbuffer (1), sampler (2).
#[must_use]
pub const fn post_bind_group_layout_entries() -> [BindGroupLayoutEntry; 3] {
    [
        BindGroupLayoutEntry {
            binding: 0,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Buffer {
                ty: BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: NonZeroU64::new(size_of::<PostUniforms>() as u64),
            },
            count: None,
        },
        BindGroupLayoutEntry {
            binding: 1,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Texture {
                sample_type: TextureSampleType::Float { filterable: true },
                view_dimension: TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        BindGroupLayoutEntry {
            binding: 2,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Sampler(SamplerBindingType::Filtering),
            count: None,
        },
    ]
}

/// Needs no device, so a missing entry point is reported by name rather
/// than as the device's pipeline-creation error.
pub fn validate_post_shader(source: &str) -> Result<(), PostShaderError> {
    let module = wgsl::parse_str(source).map_err(|e| PostShaderError::Parse(e.to_string()))?;
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .map_err(|e| PostShaderError::Validate(e.to_string()))?;
    let has_entry = module
        .entry_points
        .iter()
        .any(|ep| ep.name == POST_ENTRY_POINT && ep.stage == wgpu::naga::ShaderStage::Fragment);
    if !has_entry {
        return Err(PostShaderError::MissingEntryPoint);
    }
    check_bindings(&module)
}

/// naga validates a module on its own, never against a pipeline layout, so
/// a resource that disagrees with [`post_bind_group_layout_entries`] is
/// otherwise reported only as the device's pipeline error, which does not
/// name the binding.
fn check_bindings(module: &Module) -> Result<(), PostShaderError> {
    for (_, var) in module.global_variables.iter() {
        if matches!(var.space, AddressSpace::Private | AddressSpace::WorkGroup) {
            continue;
        }
        let name = var.name.as_deref().unwrap_or("<unnamed>");
        let Some(ResourceBinding { group: 0, binding }) = var.binding else {
            return Err(PostShaderError::Binding(format!(
                "`{name}` is not in `@group(0)`"
            )));
        };
        let inner = &module.types[var.ty].inner;
        let matches_layout = match binding {
            0 => {
                var.space == AddressSpace::Uniform
                    && matches!(inner, TypeInner::Struct { span, .. }
                        if *span as usize <= size_of::<PostUniforms>())
            }
            1 => var.space == AddressSpace::Handle && is_sampled_float_2d(inner),
            2 => {
                var.space == AddressSpace::Handle
                    && matches!(inner, TypeInner::Sampler { comparison: false })
            }
            _ => {
                return Err(PostShaderError::Binding(format!(
                    "`{name}` uses `@binding({binding})`, which the post-process layout does not have"
                )));
            }
        };
        if !matches_layout {
            return Err(PostShaderError::Binding(format!(
                "`{name}` at `@binding({binding})` must be {}",
                BINDING_DECLARATIONS[binding as usize]
            )));
        }
    }
    Ok(())
}

const BINDING_DECLARATIONS: [&str; 3] = [
    "a `var<uniform>` struct no larger than `FelisPostUniforms`",
    "a `texture_2d<f32>`",
    "a filtering `sampler`",
];

const fn is_sampled_float_2d(inner: &TypeInner) -> bool {
    matches!(
        inner,
        TypeInner::Image {
            dim: ImageDimension::D2,
            arrayed: false,
            class: ImageClass::Sampled {
                kind: ScalarKind::Float,
                multi: false,
            },
        }
    )
}

pub(crate) fn build_post_pipeline(
    device: &Device,
    bind_group_layout: &BindGroupLayout,
    vertex_module: &ShaderModule,
    fragment_module: &ShaderModule,
    format: TextureFormat,
) -> RenderPipeline {
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("felis post pipeline layout"),
        bind_group_layouts: &[Some(bind_group_layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("felis post pipeline"),
        layout: Some(&layout),
        vertex: VertexState {
            module: vertex_module,
            entry_point: Some("post_vs"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        fragment: Some(FragmentState {
            module: fragment_module,
            entry_point: Some(POST_ENTRY_POINT),
            targets: &[Some(ColorTargetState {
                format,
                // The shader composites against the backbuffer it
                // samples, so fixed-function blending has nothing to do.
                blend: Some(BlendState::REPLACE),
                write_mask: ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        primitive: PrimitiveState::default(),
        depth_stencil: None,
        multisample: MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

pub(crate) fn create_target(
    device: &Device,
    format: TextureFormat,
    width: u32,
    height: u32,
) -> (Texture, TextureView) {
    let texture = device.create_texture(&TextureDescriptor {
        label: Some("felis post texture"),
        size: Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&TextureViewDescriptor::default());
    (texture, view)
}

fn create_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    uniform_buffer: &Buffer,
    target_view: &TextureView,
    sampler: &Sampler,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("felis post bg"),
        layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            },
            BindGroupEntry {
                binding: 1,
                resource: BindingResource::TextureView(target_view),
            },
            BindGroupEntry {
                binding: 2,
                resource: BindingResource::Sampler(sampler),
            },
        ],
    })
}

#[cfg(test)]
mod tests {
    use std::mem::offset_of;

    use wgpu::naga::StructMember;

    use super::*;

    /// The contract as `(field, WGSL type, byte offset)`. Append-only: an
    /// existing row must never change.
    const CONTRACT: [(&str, &str, usize); 29] = [
        ("resolution_px", "vec2<f32>", 0),
        ("cell_size_px", "vec2<f32>", 8),
        ("time_s", "f32", 16),
        ("time_delta_s", "f32", 20),
        ("frame", "u32", 24),
        ("contract_version", "u32", 28),
        ("cursor_rect", "vec4<f32>", 32),
        ("prev_cursor_rect", "vec4<f32>", 48),
        ("cursor_color", "vec4<f32>", 64),
        ("trail_color", "vec4<f32>", 80),
        ("trail_corners_x", "vec4<f32>", 96),
        ("trail_corners_y", "vec4<f32>", 112),
        ("cursor_change_time_s", "f32", 128),
        ("cursor_style", "u32", 132),
        ("cursor_visible", "u32", 136),
        ("focused", "u32", 140),
        ("mouse_pos", "vec4<f32>", 144),
        ("mouse_buttons", "vec4<f32>", 160),
        ("background", "vec4<f32>", 176),
        ("foreground", "vec4<f32>", 192),
        ("selection_bg", "vec4<f32>", 208),
        ("palette", "array<vec4<f32>, 256>", 224),
        ("time_whole_s", "u32", 4320),
        ("time_fract_s", "f32", 4324),
        ("focus_change_time_s", "f32", 4328),
        ("stage_index", "u32", 4332),
        ("animation_start_s", "f32", 4336),
        ("animation_progress", "f32", 4340),
        ("_reserved", "vec2<u32>", 4344),
    ];

    const CONTRACT_SIZE: usize = 4352;

    fn uniform_members(source: &str) -> Vec<StructMember> {
        uniform_struct(source).0
    }

    fn uniform_struct(source: &str) -> (Vec<StructMember>, u32) {
        let module = wgsl::parse_str(source).expect("WGSL parse");
        module
            .types
            .iter()
            .find_map(|(_, ty)| match (&ty.name, &ty.inner) {
                (Some(name), TypeInner::Struct { members, span })
                    if name == "FelisPostUniforms" =>
                {
                    Some((members.clone(), *span))
                }
                _ => None,
            })
            .expect("source declares FelisPostUniforms")
    }

    /// A shader declaring the first `fields` members of the contract and
    /// returning `body`.
    fn contract_shader(fields: usize, body: &str) -> String {
        use std::fmt::Write as _;

        let mut src = String::from("struct FelisPostUniforms {\n");
        for (name, ty, _) in CONTRACT.iter().take(fields) {
            writeln!(src, "    {name}: {ty},").expect("writing to a String cannot fail");
        }
        src.push_str("}\n@group(0) @binding(0) var<uniform> u: FelisPostUniforms;\n");
        write!(
            src,
            "@fragment\nfn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {{\n    \
             return {body};\n}}\n"
        )
        .expect("writing to a String cannot fail");
        src
    }

    /// REQ-714: the uniform contract is append-only, so a shader that
    /// declares only a prefix of `FelisPostUniforms` sees every declared
    /// field at the full struct's offset. Checked at every prefix length
    /// because the `f32` / `u32` runs only misalign when last.
    #[test]
    fn a_shader_declaring_a_prefix_of_the_uniform_struct_keeps_the_offsets() {
        for fields in 1..=CONTRACT.len() {
            // Reads field 0 so the uniform is not an unused global.
            let source = contract_shader(fields, "vec4<f32>(u.resolution_px, uv)");
            validate_post_shader(&source)
                .unwrap_or_else(|e| panic!("prefix of {fields} fields must still load: {e}"));
            let prefix = uniform_members(&source);
            assert_eq!(prefix.len(), fields);
            for (declared, (name, _, offset)) in prefix.iter().zip(CONTRACT) {
                assert_eq!(declared.name.as_deref(), Some(name));
                assert_eq!(
                    declared.offset as usize, offset,
                    "prefix of {fields} fields moved {name}",
                );
            }
        }
    }

    #[test]
    fn a_shader_declaring_the_full_v2_struct_loads_and_reads_the_animation_progress() {
        let source = contract_shader(
            CONTRACT.len(),
            "vec4<f32>(uv, u.animation_progress, f32(u.stage_index))",
        );
        validate_post_shader(&source).expect("the full v2 struct loads");
        let (_, span) = uniform_struct(&source);
        assert_eq!(span as usize, CONTRACT_SIZE);
    }

    #[test]
    fn the_bundled_trail_declares_a_prefix_of_the_contract() {
        let members = uniform_members(TRAIL_SHADER_SOURCE);
        assert!(members.len() <= CONTRACT.len());
        for (member, (name, _, offset)) in members.iter().zip(CONTRACT) {
            assert_eq!(member.name.as_deref(), Some(name));
            assert_eq!(member.offset as usize, offset, "trail.wgsl moved {name}");
        }
    }

    #[test]
    fn vertex_source_parses_and_validates() {
        let module = wgsl::parse_str(POST_VERTEX_SOURCE).expect("WGSL parse");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("WGSL validate");
    }

    #[test]
    fn bundled_trail_passes_the_same_gate_a_user_shader_does() {
        validate_post_shader(TRAIL_SHADER_SOURCE).expect("bundled trail is a valid user shader");
    }

    #[test]
    fn a_shader_without_the_entry_point_is_refused() {
        let src = "@fragment fn other(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> \
                   { return vec4<f32>(uv, 0.0, 1.0); }";
        assert!(matches!(
            validate_post_shader(src),
            Err(PostShaderError::MissingEntryPoint)
        ));
    }

    #[test]
    fn a_shader_that_does_not_parse_is_refused_with_the_message() {
        let src = "@fragment fn fs_post( {";
        let naga_message = wgsl::parse_str(src).unwrap_err().to_string();
        let Err(PostShaderError::Parse(message)) = validate_post_shader(src) else {
            panic!("an unparseable shader must be refused as a parse error");
        };
        assert_eq!(message, naga_message);
    }

    #[test]
    fn a_shader_that_parses_but_fails_validation_is_refused() {
        // A runtime-sized array in the uniform address space parses but
        // fails the Validator.
        let src = "@group(0) @binding(0) var<uniform> u: array<f32>; @fragment fn fs_post() -> \
                   @location(0) vec4<f32> { return vec4<f32>(u[0], 0.0, 0.0, 1.0); }";
        // Validate(_) only: accepting Parse too would let this fixture
        // rot into a parse error without losing the test.
        assert!(matches!(
            validate_post_shader(src),
            Err(PostShaderError::Validate(_))
        ));
    }

    fn shader_with_globals(globals: &str) -> String {
        format!(
            "{globals}\n@fragment fn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> \
             {{ return vec4<f32>(uv, 0.0, 1.0); }}"
        )
    }

    /// The negative control for the noop-device chain tests: the device
    /// does compare a module against the layout, so a clean frame there
    /// means the bindings agreed.
    #[cfg(not(windows))]
    #[test]
    fn the_noop_device_rejects_a_pipeline_whose_bindings_disagree_with_the_layout() {
        let (device, _queue, mode) = crate::test_gpu::noop_device();
        let shared = PostShared::new(&device, TextureFormat::Rgba8UnormSrgb, mode);
        let src = shader_with_globals("@group(0) @binding(1) var s: sampler;").replace(
            "return vec4<f32>(uv, 0.0, 1.0);",
            "_ = s; return vec4<f32>(uv, 0.0, 1.0);",
        );
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let module = device.create_shader_module(ShaderModuleDescriptor {
            label: None,
            source: ShaderSource::Wgsl(src.into()),
        });
        let _pipeline = build_post_pipeline(
            &device,
            &shared.bind_group_layout,
            &shared.vertex_module,
            &module,
            shared.format,
        );
        assert!(crate::warmup::block_on(scope.pop()).is_some());
    }

    #[test]
    fn the_documented_bindings_pass_the_binding_check() {
        let src = shader_with_globals(
            "struct U { resolution_px: vec2<f32> }\n\
             @group(0) @binding(0) var<uniform> u: U;\n\
             @group(0) @binding(1) var backbuffer: texture_2d<f32>;\n\
             @group(0) @binding(2) var backbuffer_sampler: sampler;\n\
             var<private> scratch: f32;",
        );
        validate_post_shader(&src).expect("the documented layout loads");
    }

    #[test]
    fn a_resource_at_the_wrong_type_for_its_binding_is_refused() {
        for globals in [
            "@group(0) @binding(1) var s: sampler;",
            "@group(0) @binding(2) var t: texture_2d<f32>;",
            "@group(0) @binding(2) var s: sampler_comparison;",
            "@group(0) @binding(1) var t: texture_2d<u32>;",
            "@group(0) @binding(1) var t: texture_2d_array<f32>;",
            "@group(0) @binding(1) var t: texture_multisampled_2d<f32>;",
            "@group(0) @binding(0) var<uniform> u: vec4<f32>;",
            "struct U { x: f32 }\n@group(0) @binding(0) var<storage> u: U;",
        ] {
            assert!(
                matches!(
                    validate_post_shader(&shader_with_globals(globals)),
                    Err(PostShaderError::Binding(_))
                ),
                "{globals}",
            );
        }
    }

    #[test]
    fn a_resource_outside_group_0_or_its_bindings_is_refused() {
        for globals in [
            "@group(1) @binding(0) var t: texture_2d<f32>;",
            "@group(0) @binding(7) var t: texture_2d<f32>;",
        ] {
            assert!(
                matches!(
                    validate_post_shader(&shader_with_globals(globals)),
                    Err(PostShaderError::Binding(_))
                ),
                "{globals}",
            );
        }
    }

    #[test]
    fn a_uniform_struct_larger_than_the_contract_is_refused() {
        let globals = format!(
            "struct U {{ big: array<vec4<f32>, {}> }}\n\
             @group(0) @binding(0) var<uniform> u: U;",
            size_of::<PostUniforms>() / 16 + 1
        );
        assert!(matches!(
            validate_post_shader(&shader_with_globals(&globals)),
            Err(PostShaderError::Binding(_))
        ));
    }

    #[test]
    fn uniform_contract_offsets_agree_between_rust_and_wgsl() {
        let rust = [
            offset_of!(PostUniforms, resolution_px),
            offset_of!(PostUniforms, cell_size_px),
            offset_of!(PostUniforms, time_s),
            offset_of!(PostUniforms, time_delta_s),
            offset_of!(PostUniforms, frame),
            offset_of!(PostUniforms, contract_version),
            offset_of!(PostUniforms, cursor_rect),
            offset_of!(PostUniforms, prev_cursor_rect),
            offset_of!(PostUniforms, cursor_color),
            offset_of!(PostUniforms, trail_color),
            offset_of!(PostUniforms, trail_corners_x),
            offset_of!(PostUniforms, trail_corners_y),
            offset_of!(PostUniforms, cursor_change_time_s),
            offset_of!(PostUniforms, cursor_style),
            offset_of!(PostUniforms, cursor_visible),
            offset_of!(PostUniforms, focused),
            offset_of!(PostUniforms, mouse_pos),
            offset_of!(PostUniforms, mouse_buttons),
            offset_of!(PostUniforms, background),
            offset_of!(PostUniforms, foreground),
            offset_of!(PostUniforms, selection_bg),
            offset_of!(PostUniforms, palette),
            offset_of!(PostUniforms, time_whole_s),
            offset_of!(PostUniforms, time_fract_s),
            offset_of!(PostUniforms, focus_change_time_s),
            offset_of!(PostUniforms, stage_index),
            offset_of!(PostUniforms, animation_start_s),
            offset_of!(PostUniforms, animation_progress),
            offset_of!(PostUniforms, reserved),
        ];
        for ((name, _, contract_offset), rust_offset) in CONTRACT.iter().zip(rust) {
            assert_eq!(rust_offset, *contract_offset, "PostUniforms::{name}");
        }
        assert_eq!(size_of::<PostUniforms>(), CONTRACT_SIZE);

        let (members, span) = uniform_struct(&contract_shader(CONTRACT.len(), "vec4<f32>(uv, uv)"));
        assert_eq!(span as usize, size_of::<PostUniforms>());
        for (member, (name, _, offset)) in members.iter().zip(CONTRACT) {
            assert_eq!(member.offset as usize, offset, "FelisPostUniforms.{name}");
        }
    }

    #[test]
    fn the_clock_splits_time_into_exact_whole_seconds_and_a_fraction_below_one() {
        let origin = Instant::now();
        let mut clock = PostClock::new(origin);
        let at = origin + std::time::Duration::new(100_000, 999_999_999);
        let time = clock.tick(at);
        assert_eq!(time.time_whole_s, 100_000);
        assert!(time.time_fract_s < 1.0, "{}", time.time_fract_s);
        assert!(time.time_fract_s > 0.999, "{}", time.time_fract_s);

        let time = clock.tick(origin + std::time::Duration::new(3, 250_000_000));
        assert_eq!((time.time_whole_s, time.time_fract_s), (3, 0.25));
    }

    #[test]
    fn the_clock_counts_frames_and_their_spacing() {
        let origin = Instant::now();
        let mut clock = PostClock::new(origin);
        let first = clock.tick(origin + std::time::Duration::from_millis(500));
        assert_eq!((first.frame, first.time_delta_s), (0, 0.5));
        let second = clock.tick(origin + std::time::Duration::from_millis(750));
        assert_eq!((second.frame, second.time_delta_s), (1, 0.25));
        assert_eq!(second.time_s, 0.75);
    }

    #[test]
    fn the_focus_change_time_is_zero_until_focus_changes_then_on_the_time_scale() {
        let origin = Instant::now();
        let mut clock = PostClock::new(origin);
        assert_eq!(clock.tick(origin).focus_change_time_s, 0.0);
        clock.focus_changed(origin + std::time::Duration::from_secs(2));
        let time = clock.tick(origin + std::time::Duration::from_secs(5));
        assert_eq!(time.focus_change_time_s, 2.0);
    }

    #[test]
    fn a_frame_writes_the_v2_fields_and_the_contract_version() {
        let theme = ResolvedTheme::new(&felis_render_core::palette::Theme::default());
        let origin = Instant::now();
        let mut clock = PostClock::new(origin);
        clock.focus_changed(origin + std::time::Duration::from_secs(1));
        let mut uniforms = PostUniforms::default();
        uniforms.write_frame(
            [640.0, 480.0],
            &StageInputs {
                time: clock.tick(origin + std::time::Duration::from_millis(7_500)),
                frame: PostFrame {
                    cell_size_px: [8.0, 16.0],
                    theme: &theme,
                    cursor_color: [1.0; 4],
                    cursor_style: CursorStyle::Bar,
                    cursor_visible: true,
                    focused: true,
                },
                trail: TrailState::default(),
                mouse: MouseState::default(),
                stage_index: 3,
            },
        );
        assert_eq!(uniforms.contract_version, 2);
        assert_eq!((uniforms.time_whole_s, uniforms.time_fract_s), (7, 0.5));
        assert_eq!(uniforms.focus_change_time_s, 1.0);
        assert_eq!(uniforms.stage_index, 3);
        assert_eq!(uniforms.reserved, [0; 2]);
    }

    #[test]
    fn post_bgl_pins_the_uniform_minimum_size() {
        let entries = post_bind_group_layout_entries();
        assert_eq!(entries.len(), 3);
        match entries[0].ty {
            BindingType::Buffer {
                min_binding_size, ..
            } => assert_eq!(
                min_binding_size.map(NonZeroU64::get),
                Some(size_of::<PostUniforms>() as u64),
            ),
            _ => panic!("expected the uniform buffer at binding 0"),
        }
    }
}
