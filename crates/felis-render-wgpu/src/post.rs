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
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    },
};

use crate::palette::ResolvedTheme;

/// Reported to the shader as `contract_version` so an effect can branch
/// on a field it is unsure of rather than fail to load.
pub const CONTRACT_VERSION: u32 = 1;

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
        }
    }
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
pub struct PostFrame<'a> {
    pub cell_size_px: [f32; 2],
    pub theme: &'a ResolvedTheme,
    pub cursor_color: [f32; 4],
    pub cursor_style: CursorStyle,
    pub cursor_visible: bool,
    pub focused: bool,
}

pub struct PostStage {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    uniforms_ring: UniformRing,
    sampler: Sampler,
    target: Texture,
    target_view: TextureView,
    uniforms: PostUniforms,
    frame_counter: u32,
    start: Instant,
    last_frame: Instant,
}

impl PostStage {
    /// Validation runs before the device sees the source: wgpu turns a
    /// shader-compilation failure into a panic on the device error
    /// scope, so a bad user file has to be caught here to stay
    /// recoverable.
    pub(crate) fn new(
        device: &Device,
        upload_mode: UploadMode,
        format: TextureFormat,
        width: u32,
        height: u32,
        fragment_wgsl: &str,
    ) -> Result<Self, PostShaderError> {
        validate_post_shader(fragment_wgsl)?;

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("felis post bgl"),
            entries: &post_bind_group_layout_entries(),
        });
        let vertex_module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("felis post vertex"),
            source: ShaderSource::Wgsl(POST_VERTEX_SOURCE.into()),
        });
        let fragment_module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("felis post fragment"),
            source: ShaderSource::Wgsl(fragment_wgsl.into()),
        });
        let pipeline = build_post_pipeline(
            device,
            &bind_group_layout,
            &vertex_module,
            &fragment_module,
            format,
        );

        let uniforms_ring = UniformRing::new("felis post uniforms", upload_mode);
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
        let (target, target_view) = create_target(device, format, width, height);

        let now = Instant::now();
        Ok(Self {
            pipeline,
            bind_group_layout,
            uniforms_ring,
            sampler,
            target,
            target_view,
            uniforms: PostUniforms::default(),
            frame_counter: 0,
            start: now,
            last_frame: now,
        })
    }

    pub(crate) fn submitted(&mut self, index: &wgpu::SubmissionIndex) {
        self.uniforms_ring.submitted(index);
    }

    pub const fn target_view(&self) -> &TextureView {
        &self.target_view
    }

    pub fn resize(&mut self, device: &Device, format: TextureFormat, width: u32, height: u32) {
        if self.target.width() == width.max(1) && self.target.height() == height.max(1) {
            return;
        }
        let (target, target_view) = create_target(device, format, width, height);
        self.uniforms_ring.invalidate();
        self.target = target;
        self.target_view = target_view;
    }

    pub(crate) fn encode(
        &mut self,
        encoder: &mut CommandEncoder,
        device: &Device,
        queue: &Queue,
        output: &TextureView,
        frame: PostFrame<'_>,
        trail: TrailState,
        mouse: MouseState,
    ) {
        let now = Instant::now();
        let time_s = now.duration_since(self.start).as_secs_f32();
        self.uniforms.time_delta_s = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        self.uniforms.time_s = time_s;
        self.uniforms.frame = self.frame_counter;
        self.frame_counter = self.frame_counter.wrapping_add(1);
        self.uniforms.contract_version = CONTRACT_VERSION;
        self.uniforms.resolution_px = [self.target.width() as f32, self.target.height() as f32];
        self.uniforms.cell_size_px = frame.cell_size_px;
        self.uniforms.cursor_rect = trail.cursor_rect;
        self.uniforms.prev_cursor_rect = trail.prev_cursor_rect;
        self.uniforms.trail_corners_x = trail.corners_x;
        self.uniforms.trail_corners_y = trail.corners_y;
        // The client reports an age, not a timestamp, so the two
        // processes' clocks stay out of the contract.
        self.uniforms.cursor_change_time_s = time_s - trail.seconds_since_change;
        self.uniforms.cursor_color = frame.cursor_color;
        self.uniforms.trail_color = frame.cursor_color;
        self.uniforms.cursor_style = frame.cursor_style as u32;
        self.uniforms.cursor_visible = u32::from(frame.cursor_visible);
        self.uniforms.focused = u32::from(frame.focused);
        self.uniforms.mouse_pos = [
            mouse.pos_uv[0],
            mouse.pos_uv[1],
            mouse.last_press_uv[0],
            mouse.last_press_uv[1],
        ];
        self.uniforms.mouse_buttons = mouse.buttons;
        self.uniforms.background = frame.theme.bg;
        self.uniforms.foreground = frame.theme.fg;
        self.uniforms.selection_bg = crate::palette::SELECTION_BG;
        self.uniforms.palette = frame.theme.palette;
        self.uniforms_ring
            .write(device, queue, bytemuck::bytes_of(&self.uniforms));
        let (layout, view, sampler) = (&self.bind_group_layout, &self.target_view, &self.sampler);
        self.uniforms_ring
            .prepare_bind_group(|buf| create_bind_group(device, layout, buf, view, sampler));
        let Some(bind_group) = self.uniforms_ring.bind_group() else {
            return;
        };

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("felis post pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: output,
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

/// Device-side compilation reports a missing entry point as a
/// pipeline-creation panic rather than a value, hence the explicit
/// entry-point check.
pub fn validate_post_shader(source: &str) -> Result<(), PostShaderError> {
    let module = wgsl::parse_str(source).map_err(|e| PostShaderError::Parse(e.to_string()))?;
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .map_err(|e| PostShaderError::Validate(e.to_string()))?;
    let has_entry = module
        .entry_points
        .iter()
        .any(|ep| ep.name == POST_ENTRY_POINT && ep.stage == wgpu::naga::ShaderStage::Fragment);
    if has_entry {
        Ok(())
    } else {
        Err(PostShaderError::MissingEntryPoint)
    }
}

fn build_post_pipeline(
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

fn create_target(
    device: &Device,
    format: TextureFormat,
    width: u32,
    height: u32,
) -> (Texture, TextureView) {
    let texture = device.create_texture(&TextureDescriptor {
        label: Some("felis post target"),
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

    use wgpu::naga::{StructMember, TypeInner};

    use super::*;

    /// Paired positionally with the members parsed out of `trail.wgsl`,
    /// so a retype on either side fails the prefix test.
    const CONTRACT_WGSL_TYPES: [&str; 22] = [
        "vec2<f32>",
        "vec2<f32>",
        "f32",
        "f32",
        "u32",
        "u32",
        "vec4<f32>",
        "vec4<f32>",
        "vec4<f32>",
        "vec4<f32>",
        "vec4<f32>",
        "vec4<f32>",
        "f32",
        "u32",
        "u32",
        "u32",
        "vec4<f32>",
        "vec4<f32>",
        "vec4<f32>",
        "vec4<f32>",
        "vec4<f32>",
        "array<vec4<f32>, 256>",
    ];

    fn uniform_members(source: &str) -> Vec<StructMember> {
        let module = wgsl::parse_str(source).expect("WGSL parse");
        module
            .types
            .iter()
            .find_map(|(_, ty)| match (&ty.name, &ty.inner) {
                (Some(name), TypeInner::Struct { members, .. }) if name == "FelisPostUniforms" => {
                    Some(members.clone())
                }
                _ => None,
            })
            .expect("source declares FelisPostUniforms")
    }

    /// A shader declaring only the first `fields` members of the
    /// contract.
    fn prefix_shader(full: &[StructMember], fields: usize) -> String {
        use std::fmt::Write as _;

        let mut src = String::from("struct FelisPostUniforms {\n");
        for (member, ty) in full.iter().zip(CONTRACT_WGSL_TYPES).take(fields) {
            let name = member.name.as_deref().expect("contract fields are named");
            writeln!(src, "    {name}: {ty},").expect("writing to a String cannot fail");
        }
        src.push_str("}\n@group(0) @binding(0) var<uniform> u: FelisPostUniforms;\n");
        // Reads field 0 so the uniform is not an unused global.
        src.push_str(
            "@fragment\nfn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {\n    \
             return vec4<f32>(u.resolution_px, uv);\n}\n",
        );
        src
    }

    /// REQ-714: the uniform contract is append-only, so a shader that
    /// declares only a prefix of `FelisPostUniforms` sees every declared
    /// field at the full struct's offset. Checked at every prefix length
    /// because the `f32` / `u32` runs only misalign when last.
    #[test]
    fn a_shader_declaring_a_prefix_of_the_uniform_struct_keeps_the_offsets() {
        let full = uniform_members(TRAIL_SHADER_SOURCE);
        assert_eq!(
            full.len(),
            CONTRACT_WGSL_TYPES.len(),
            "the WGSL type list must cover every contract field",
        );
        for fields in 1..=full.len() {
            let source = prefix_shader(&full, fields);
            validate_post_shader(&source)
                .unwrap_or_else(|e| panic!("prefix of {fields} fields must still load: {e}"));
            let prefix = uniform_members(&source);
            assert_eq!(prefix.len(), fields);
            for (declared, contract) in prefix.iter().zip(&full) {
                assert_eq!(declared.name, contract.name);
                assert_eq!(
                    declared.offset, contract.offset,
                    "prefix of {fields} fields moved {:?}",
                    contract.name,
                );
            }
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
        let err = validate_post_shader("@fragment fn fs_post( {").unwrap_err();
        assert!(matches!(err, PostShaderError::Parse(_)));
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

    #[test]
    fn uniform_contract_offsets_agree_between_rust_and_wgsl() {
        // The literal offsets are the v1 contract; append-only, so an
        // entry must never change.
        let contract = [
            ("resolution_px", 0, offset_of!(PostUniforms, resolution_px)),
            ("cell_size_px", 8, offset_of!(PostUniforms, cell_size_px)),
            ("time_s", 16, offset_of!(PostUniforms, time_s)),
            ("time_delta_s", 20, offset_of!(PostUniforms, time_delta_s)),
            ("frame", 24, offset_of!(PostUniforms, frame)),
            (
                "contract_version",
                28,
                offset_of!(PostUniforms, contract_version),
            ),
            ("cursor_rect", 32, offset_of!(PostUniforms, cursor_rect)),
            (
                "prev_cursor_rect",
                48,
                offset_of!(PostUniforms, prev_cursor_rect),
            ),
            ("cursor_color", 64, offset_of!(PostUniforms, cursor_color)),
            ("trail_color", 80, offset_of!(PostUniforms, trail_color)),
            (
                "trail_corners_x",
                96,
                offset_of!(PostUniforms, trail_corners_x),
            ),
            (
                "trail_corners_y",
                112,
                offset_of!(PostUniforms, trail_corners_y),
            ),
            (
                "cursor_change_time_s",
                128,
                offset_of!(PostUniforms, cursor_change_time_s),
            ),
            ("cursor_style", 132, offset_of!(PostUniforms, cursor_style)),
            (
                "cursor_visible",
                136,
                offset_of!(PostUniforms, cursor_visible),
            ),
            ("focused", 140, offset_of!(PostUniforms, focused)),
            ("mouse_pos", 144, offset_of!(PostUniforms, mouse_pos)),
            (
                "mouse_buttons",
                160,
                offset_of!(PostUniforms, mouse_buttons),
            ),
            ("background", 176, offset_of!(PostUniforms, background)),
            ("foreground", 192, offset_of!(PostUniforms, foreground)),
            ("selection_bg", 208, offset_of!(PostUniforms, selection_bg)),
            ("palette", 224, offset_of!(PostUniforms, palette)),
        ];
        for (name, contract_offset, rust_offset) in contract {
            assert_eq!(rust_offset, contract_offset, "PostUniforms::{name}");
        }
        assert_eq!(size_of::<PostUniforms>() % 16, 0);

        let module = wgsl::parse_str(TRAIL_SHADER_SOURCE).expect("WGSL parse");
        let members = module
            .types
            .iter()
            .find_map(|(_, ty)| match (&ty.name, &ty.inner) {
                (Some(name), TypeInner::Struct { members, .. }) if name == "FelisPostUniforms" => {
                    Some(members.clone())
                }
                _ => None,
            })
            .expect("trail.wgsl declares FelisPostUniforms");
        assert_eq!(members.len(), contract.len(), "WGSL field count");
        for (member, (name, contract_offset, _)) in members.iter().zip(contract) {
            assert_eq!(member.name.as_deref(), Some(name));
            assert_eq!(
                usize::try_from(member.offset).expect("offset fits usize"),
                contract_offset,
                "FelisPostUniforms.{name}",
            );
        }
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
