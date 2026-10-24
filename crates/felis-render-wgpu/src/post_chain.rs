//! The chain of post-process stages `shader.post` names: stage 0 samples
//! the cell pass, each later stage samples the one before it, and the last
//! writes the swapchain.

use wgpu::{
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingResource, BindingType, BlendState, ColorTargetState, ColorWrites,
    CommandEncoder, Device, FragmentState, LoadOp, MultisampleState, Operations,
    PipelineLayoutDescriptor, PrimitiveState, Queue, RenderPipeline, RenderPipelineDescriptor,
    ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp, Texture, TextureFormat,
    TextureSampleType, TextureView, TextureViewDimension, VertexState,
};

use crate::buffer_ring::UploadMode;
use crate::post::{
    AnimationRun, PostShaderError, PostShared, PostStage, PostStageSource, StageInputs, StageIo,
    create_target,
};

/// Copies the final stage's own texture to the swapchain when that stage
/// keeps history. `textureLoad` rather than a sampler, so the copy is
/// texel-exact.
const BLIT_SOURCE: &str = "\
    @group(0) @binding(0) var source: texture_2d<f32>;\n\
    @fragment fn fs_blit(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {\n\
        return textureLoad(source, vec2<i32>(position.xy), 0);\n\
    }";

/// Where a stage reads from or writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slot {
    /// The offscreen texture the cell pass renders into.
    Cell,
    /// One of the intermediates the chain ping-pongs between.
    Scratch(usize),
    /// The output texture of the stage at this position, which keeps it
    /// as next frame's history.
    Own(usize),
    /// The frame being presented.
    Surface,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// `(input, output)` per stage.
    io: Vec<(Slot, Slot)>,
    /// The last stage renders into its own texture, which then needs one
    /// more pass to reach the surface.
    blit_last: bool,
}

/// `history[i]`: stage `i` declares binding 3.
pub(crate) fn plan(history: &[bool]) -> Plan {
    let len = history.len();
    let mut input = Slot::Cell;
    let io = history
        .iter()
        .enumerate()
        .map(|(position, &own)| {
            let output = if own {
                Slot::Own(position)
            } else if position + 1 == len {
                Slot::Surface
            } else if input == Slot::Scratch(0) {
                Slot::Scratch(1)
            } else {
                Slot::Scratch(0)
            };
            let io = (input, output);
            input = output;
            io
        })
        .collect();
    Plan {
        io,
        blit_last: history.last() == Some(&true),
    }
}

fn scratch_count(plan: &Plan) -> usize {
    plan.io
        .iter()
        .filter_map(|(_, output)| match output {
            Slot::Scratch(index) => Some(index + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// A stage the chain refused, reported by its position in the sources.
pub(crate) struct Refused<'a> {
    pub(crate) source_index: usize,
    pub(crate) label: &'a str,
    pub(crate) error: PostShaderError,
}

/// A history stage's output and the copy of it the next frame reads. New
/// textures start zeroed, so the first frame after a load or resize sees
/// a blank history.
struct HistoryPair {
    out: (Texture, TextureView),
    prev: (Texture, TextureView),
}

impl HistoryPair {
    fn new(device: &Device, format: TextureFormat, width: u32, height: u32) -> Self {
        Self {
            out: create_target(device, format, width, height),
            prev: create_target(device, format, width, height),
        }
    }
}

struct Blit {
    pipeline: RenderPipeline,
    layout: BindGroupLayout,
    bind_group: Option<BindGroup>,
}

impl Blit {
    fn new(device: &Device, shared: &PostShared) -> Self {
        let layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("felis post blit bgl"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::FRAGMENT,
                ty: BindingType::Texture {
                    sample_type: TextureSampleType::Float { filterable: true },
                    view_dimension: TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("felis post blit"),
            source: ShaderSource::Wgsl(BLIT_SOURCE.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("felis post blit pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("felis post blit pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: shared.vertex_module(),
                entry_point: Some("post_vs"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(FragmentState {
                module: &module,
                entry_point: Some("fs_blit"),
                targets: &[Some(ColorTargetState {
                    format: shared.format(),
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
        });
        Self {
            pipeline,
            layout,
            bind_group: None,
        }
    }

    fn encode(
        &mut self,
        encoder: &mut CommandEncoder,
        device: &Device,
        source: &TextureView,
        surface: &TextureView,
    ) {
        let layout = &self.layout;
        let bind_group = self.bind_group.get_or_insert_with(|| {
            device.create_bind_group(&BindGroupDescriptor {
                label: Some("felis post blit bg"),
                layout,
                entries: &[BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(source),
                }],
            })
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("felis post blit pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: surface,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
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
        pass.set_bind_group(0, &*bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

pub(crate) struct PostChain {
    shared: PostShared,
    stages: Vec<PostStage>,
    plan: Plan,
    cell: (Texture, TextureView),
    scratch: Vec<(Texture, TextureView)>,
    /// Indexed like `stages`; `Some` for a stage that declares history.
    history: Vec<Option<HistoryPair>>,
    blit: Option<Blit>,
}

impl PostChain {
    /// `None` when no stage loads; each refused stage is handed to
    /// `refused` and the rest still run, in order.
    pub(crate) fn new<'a>(
        device: &Device,
        format: TextureFormat,
        upload_mode: UploadMode,
        (width, height): (u32, u32),
        sources: &'a [PostStageSource],
        refused: &mut dyn FnMut(&Refused<'a>),
    ) -> Option<Self> {
        let shared = PostShared::new(device, format, upload_mode);
        let stages: Vec<PostStage> = sources
            .iter()
            .enumerate()
            .filter_map(|(source_index, source)| {
                PostStage::new(device, &shared, source_index, source)
                    .map_err(|error| {
                        refused(&Refused {
                            source_index,
                            label: &source.label,
                            error,
                        });
                    })
                    .ok()
            })
            .collect();
        if stages.is_empty() {
            return None;
        }
        let history_flags: Vec<bool> = stages.iter().map(PostStage::has_history).collect();
        let plan = plan(&history_flags);
        let cell = create_target(device, format, width, height);
        let scratch = (0..scratch_count(&plan))
            .map(|_| create_target(device, format, width, height))
            .collect();
        let history = history_flags
            .iter()
            .map(|&own| own.then(|| HistoryPair::new(device, format, width, height)))
            .collect();
        let blit = plan.blit_last.then(|| Blit::new(device, &shared));
        Some(Self {
            shared,
            stages,
            plan,
            cell,
            scratch,
            history,
            blit,
        })
    }

    pub(crate) const fn cell_target(&self) -> &TextureView {
        &self.cell.1
    }

    /// Every texture is recreated, so history restarts from zero.
    pub(crate) fn resize(&mut self, device: &Device, width: u32, height: u32) {
        if self.cell.0.width() == width.max(1) && self.cell.0.height() == height.max(1) {
            return;
        }
        let format = self.shared.format();
        self.cell = create_target(device, format, width, height);
        for texture in &mut self.scratch {
            *texture = create_target(device, format, width, height);
        }
        for pair in self.history.iter_mut().flatten() {
            *pair = HistoryPair::new(device, format, width, height);
        }
        for stage in &mut self.stages {
            stage.invalidate_bind_group();
        }
        if let Some(blit) = self.blit.as_mut() {
            blit.bind_group = None;
        }
    }

    /// `false` when the stage at `source_index` did not load.
    pub(crate) fn start_animation(&mut self, source_index: usize, run: AnimationRun) -> bool {
        self.stages
            .iter_mut()
            .find(|stage| stage.source_index() == source_index)
            .map(|stage| stage.start_animation(run))
            .is_some()
    }

    pub(crate) fn stop_animation(&mut self, source_index: usize) {
        if let Some(stage) = self
            .stages
            .iter_mut()
            .find(|stage| stage.source_index() == source_index)
        {
            stage.stop_animation();
        }
    }

    pub(crate) fn submitted(&mut self, index: &wgpu::SubmissionIndex) {
        for stage in &mut self.stages {
            stage.submitted(index);
        }
    }

    /// `inputs` is the frame's reading; each stage gets its own position.
    pub(crate) fn encode(
        &mut self,
        encoder: &mut CommandEncoder,
        device: &Device,
        queue: &Queue,
        surface: &TextureView,
        inputs: StageInputs<'_>,
    ) {
        let Self {
            shared,
            stages,
            plan,
            cell,
            scratch,
            history,
            blit,
        } = self;
        let resolution_px = [cell.0.width() as f32, cell.0.height() as f32];
        let view = |slot: Slot| match slot {
            Slot::Cell => Some(&cell.1),
            Slot::Scratch(index) => scratch.get(index).map(|texture| &texture.1),
            Slot::Own(position) => history
                .get(position)
                .and_then(Option::as_ref)
                .map(|pair| &pair.out.1),
            Slot::Surface => Some(surface),
        };
        for (position, (stage, &(input, output))) in stages.iter_mut().zip(&plan.io).enumerate() {
            let (Some(input), Some(output)) = (view(input), view(output)) else {
                continue;
            };
            let pair = history.get(position).and_then(Option::as_ref);
            stage.encode(
                encoder,
                device,
                queue,
                shared,
                StageIo {
                    input,
                    history: pair.map(|pair| &pair.prev.1),
                    output,
                    resolution_px,
                },
                StageInputs {
                    stage_index: u32::try_from(position).unwrap_or(u32::MAX),
                    ..inputs
                },
            );
            if let Some(pair) = pair {
                encoder.copy_texture_to_texture(
                    pair.out.0.as_image_copy(),
                    pair.prev.0.as_image_copy(),
                    pair.out.0.size(),
                );
            }
        }
        if let (Some(blit), Some(last)) = (blit.as_mut(), plan.io.len().checked_sub(1))
            && let Some(source) = view(Slot::Own(last))
        {
            blit.encode(encoder, device, source, surface);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    mod noop {
        use std::time::Instant;

        use felis_protocol::messages::CursorStyle;
        use felis_render_core::palette::{ResolvedTheme, Theme};

        use super::super::*;
        use crate::post::{MouseState, PostClock, PostFrame, TRAIL_SHADER_SOURCE, TrailState};
        use crate::test_gpu::noop_device;

        const FORMAT: TextureFormat = TextureFormat::Rgba8UnormSrgb;

        const PASSTHROUGH: &str = "\
            @group(0) @binding(1) var backbuffer: texture_2d<f32>;\n\
            @group(0) @binding(2) var backbuffer_sampler: sampler;\n\
            @fragment fn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {\n\
                return textureSample(backbuffer, backbuffer_sampler, uv);\n\
            }";

        fn source(label: &str, wgsl: &str) -> PostStageSource {
            PostStageSource {
                label: label.to_owned(),
                wgsl: wgsl.to_owned(),
                params: Vec::new(),
            }
        }

        /// Encodes and submits one frame inside a validation scope; `Some` is
        /// the device's complaint.
        fn encode_one_frame(
            device: &Device,
            queue: &Queue,
            chain: &mut PostChain,
        ) -> Option<wgpu::Error> {
            let theme = ResolvedTheme::new(&Theme::default());
            let (_surface, surface_view) = create_target(device, FORMAT, 16, 8);
            let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let mut encoder =
                device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            chain.encode(
                &mut encoder,
                device,
                queue,
                &surface_view,
                StageInputs {
                    time: PostClock::new(Instant::now()).tick(Instant::now()),
                    frame: PostFrame {
                        cell_size_px: [8.0, 16.0],
                        theme: &theme,
                        cursor_color: [1.0; 4],
                        cursor_style: CursorStyle::Block,
                        cursor_visible: true,
                        focused: true,
                    },
                    trail: TrailState::default(),
                    mouse: MouseState::default(),
                    stage_index: 0,
                },
            );
            let index = queue.submit([encoder.finish()]);
            chain.submitted(&index);
            crate::warmup::block_on(scope.pop())
        }

        #[test]
        fn a_three_stage_chain_encodes_a_frame_the_device_accepts() {
            let (device, queue, mode) = noop_device();
            let sources = [
                source("trail", TRAIL_SHADER_SOURCE),
                source("a", PASSTHROUGH),
                source("b", PASSTHROUGH),
            ];
            let mut chain = PostChain::new(&device, FORMAT, mode, (16, 8), &sources, &mut |r| {
                panic!("stage {} refused: {}", r.source_index, r.error)
            })
            .expect("every stage loads");
            assert_eq!(
                encode_one_frame(&device, &queue, &mut chain).map(|e| e.to_string()),
                None
            );
            chain.resize(&device, 32, 4);
            assert_eq!(
                encode_one_frame(&device, &queue, &mut chain).map(|e| e.to_string()),
                None
            );
        }

        const HISTORY_BLEND: &str = "\
            @group(0) @binding(1) var backbuffer: texture_2d<f32>;\n\
            @group(0) @binding(2) var backbuffer_sampler: sampler;\n\
            @group(0) @binding(3) var history: texture_2d<f32>;\n\
            @fragment fn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {\n\
                let now = textureSample(backbuffer, backbuffer_sampler, uv);\n\
                return max(now, textureSample(history, backbuffer_sampler, uv) * 0.9);\n\
            }";

        #[test]
        fn history_stages_mid_chain_and_last_encode_frames_the_device_accepts() {
            let (device, queue, mode) = noop_device();
            let sources = [
                source("afterglow", HISTORY_BLEND),
                source("a", PASSTHROUGH),
                source("trail", TRAIL_SHADER_SOURCE),
                source("motion blur", HISTORY_BLEND),
            ];
            let mut chain = PostChain::new(&device, FORMAT, mode, (16, 8), &sources, &mut |r| {
                panic!("stage {} refused: {}", r.source_index, r.error)
            })
            .expect("every stage loads");
            assert!(chain.blit.is_some());
            for _ in 0..2 {
                assert_eq!(
                    encode_one_frame(&device, &queue, &mut chain).map(|e| e.to_string()),
                    None
                );
            }
            chain.resize(&device, 32, 4);
            assert_eq!(
                encode_one_frame(&device, &queue, &mut chain).map(|e| e.to_string()),
                None
            );
        }

        #[test]
        fn a_chain_without_history_allocates_no_history_texture_or_blit() {
            let (device, _queue, mode) = noop_device();
            let sources = [source("a", PASSTHROUGH), source("b", PASSTHROUGH)];
            let chain = PostChain::new(&device, FORMAT, mode, (16, 8), &sources, &mut |_| {})
                .expect("loads");
            assert!(chain.history.iter().all(Option::is_none));
            assert!(chain.blit.is_none());
        }

        #[test]
        fn a_refused_stage_is_reported_and_the_rest_still_load() {
            let (device, _queue, mode) = noop_device();
            let sources = [
                source("a", PASSTHROUGH),
                source("broken", "@fragment fn fs_post( {"),
                source("b", PASSTHROUGH),
            ];
            let mut refused = Vec::new();
            let chain = PostChain::new(&device, FORMAT, mode, (16, 8), &sources, &mut |r| {
                refused.push((r.source_index, r.label.to_owned()));
            })
            .expect("two stages load");
            assert_eq!(refused, [(1, "broken".to_owned())]);
            assert_eq!(chain.stages.len(), 2);
            let run = AnimationRun {
                start: Instant::now(),
                duration: std::time::Duration::from_millis(300),
            };
            let mut chain = chain;
            assert!(
                chain.start_animation(2, run),
                "a loaded stage keeps its source index"
            );
            assert!(
                !chain.start_animation(1, run),
                "the refused stage takes no run"
            );
        }

        #[test]
        fn stages_the_device_refuses_are_dropped_and_the_rest_encode_a_frame() {
            let (device, queue, mode) = noop_device();
            let sources = [
                source("a", PASSTHROUGH),
                source(
                    "location mismatch",
                    "@fragment fn fs_post(@location(1) uv: vec2<f32>) -> @location(0) vec4<f32> {\n\
                         return vec4<f32>(uv, 0.0, 1.0);\n\
                     }",
                ),
                source(
                    "f16",
                    "enable f16;\n\
                     @fragment fn fs_post(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {\n\
                         let h = vec2<f16>(uv);\n\
                         return vec4<f32>(vec2<f32>(h), 0.0, 1.0);\n\
                     }",
                ),
                source("b", PASSTHROUGH),
            ];
            let mut refused = Vec::new();
            let mut chain = PostChain::new(&device, FORMAT, mode, (16, 8), &sources, &mut |r| {
                assert!(
                    matches!(r.error, PostShaderError::Device(_)),
                    "stage {} refused before the device: {}",
                    r.source_index,
                    r.error
                );
                refused.push(r.source_index);
            })
            .expect("two stages load");
            assert_eq!(refused, [1, 2]);
            assert_eq!(chain.stages.len(), 2);
            assert_eq!(
                encode_one_frame(&device, &queue, &mut chain).map(|e| e.to_string()),
                None
            );
        }

        #[test]
        fn stopping_a_stage_drops_only_its_run() {
            let (device, _queue, mode) = noop_device();
            let sources = [source("a", PASSTHROUGH), source("b", PASSTHROUGH)];
            let mut chain = PostChain::new(&device, FORMAT, mode, (16, 8), &sources, &mut |_| {})
                .expect("loads");
            let run = AnimationRun {
                start: Instant::now(),
                duration: std::time::Duration::from_millis(300),
            };
            assert!(chain.start_animation(0, run));
            assert!(chain.start_animation(1, run));
            chain.stop_animation(1);
            let runs: Vec<_> = chain.stages.iter().map(PostStage::animation).collect();
            assert_eq!(runs, [Some(run), None]);
        }

        #[test]
        fn a_chain_with_no_loadable_stage_is_no_chain() {
            let (device, _queue, mode) = noop_device();
            let sources = [source("broken", "@fragment fn other() {}")];
            assert!(
                PostChain::new(&device, FORMAT, mode, (16, 8), &sources, &mut |_| {}).is_none()
            );
        }
    }

    #[test]
    fn a_single_stage_reads_the_cell_pass_and_writes_the_surface() {
        let plan = plan(&[false]);
        assert_eq!(plan.io, [(Slot::Cell, Slot::Surface)]);
        assert!(!plan.blit_last);
        assert_eq!(scratch_count(&plan), 0);
    }

    #[test]
    fn each_stage_reads_the_previous_output_through_two_alternating_intermediates() {
        let plan = plan(&[false; 4]);
        assert_eq!(
            plan.io,
            [
                (Slot::Cell, Slot::Scratch(0)),
                (Slot::Scratch(0), Slot::Scratch(1)),
                (Slot::Scratch(1), Slot::Scratch(0)),
                (Slot::Scratch(0), Slot::Surface),
            ]
        );
        assert_eq!(scratch_count(&plan), 2);
        for (input, output) in plan.io {
            assert_ne!(input, output, "a stage never samples its own target");
        }
    }

    #[test]
    fn a_history_stage_writes_its_own_texture_which_the_next_stage_reads() {
        let plan = plan(&[false, true, false]);
        assert_eq!(
            plan.io,
            [
                (Slot::Cell, Slot::Scratch(0)),
                (Slot::Scratch(0), Slot::Own(1)),
                (Slot::Own(1), Slot::Surface),
            ]
        );
        assert!(!plan.blit_last, "a non-final history stage needs no blit");
        assert_eq!(scratch_count(&plan), 1);
    }

    #[test]
    fn a_final_history_stage_is_blitted_to_the_surface() {
        let single = plan(&[true]);
        assert_eq!(single.io, [(Slot::Cell, Slot::Own(0))]);
        assert!(single.blit_last);
        let chained = plan(&[false, false, true]);
        assert_eq!(
            chained.io,
            [
                (Slot::Cell, Slot::Scratch(0)),
                (Slot::Scratch(0), Slot::Scratch(1)),
                (Slot::Scratch(1), Slot::Own(2)),
            ]
        );
        assert!(chained.blit_last);
    }

    #[test]
    fn an_empty_chain_plans_nothing() {
        assert_eq!(
            plan(&[]),
            Plan {
                io: Vec::new(),
                blit_last: false
            }
        );
    }

    #[test]
    fn the_blit_shader_validates() {
        let module = wgpu::naga::front::wgsl::parse_str(BLIT_SOURCE).expect("WGSL parse");
        wgpu::naga::valid::Validator::new(
            wgpu::naga::valid::ValidationFlags::all(),
            wgpu::naga::valid::Capabilities::all(),
        )
        .validate(&module)
        .expect("WGSL validate");
    }
}
