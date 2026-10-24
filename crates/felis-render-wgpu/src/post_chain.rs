//! The chain of post-process stages `shader.post` names: stage 0 samples
//! the cell pass, each later stage samples the one before it, and the last
//! writes the swapchain.

use wgpu::{CommandEncoder, Device, Queue, Texture, TextureFormat, TextureView};

use crate::buffer_ring::UploadMode;
use crate::post::{
    PostShaderError, PostShared, PostStage, PostStageSource, StageInputs, StageIo, create_target,
};

/// Where a stage reads from or writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slot {
    /// The offscreen texture the cell pass renders into.
    Cell,
    /// One of the intermediates the chain ping-pongs between.
    Scratch(usize),
    /// The frame being presented.
    Surface,
}

/// `(input, output)` per stage for a chain of `len` stages.
pub(crate) fn plan(len: usize) -> Vec<(Slot, Slot)> {
    let mut input = Slot::Cell;
    (0..len)
        .map(|position| {
            let output = if position + 1 == len {
                Slot::Surface
            } else {
                Slot::Scratch(position % 2)
            };
            let io = (input, output);
            input = output;
            io
        })
        .collect()
}

fn scratch_count(plan: &[(Slot, Slot)]) -> usize {
    plan.iter()
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

pub(crate) struct PostChain {
    shared: PostShared,
    stages: Vec<PostStage>,
    plan: Vec<(Slot, Slot)>,
    cell: (Texture, TextureView),
    scratch: Vec<(Texture, TextureView)>,
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
                PostStage::new(device, &shared, source)
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
        let plan = plan(stages.len());
        let cell = create_target(device, format, width, height);
        let scratch = (0..scratch_count(&plan))
            .map(|_| create_target(device, format, width, height))
            .collect();
        Some(Self {
            shared,
            stages,
            plan,
            cell,
            scratch,
        })
    }

    pub(crate) const fn cell_target(&self) -> &TextureView {
        &self.cell.1
    }

    pub(crate) fn resize(&mut self, device: &Device, width: u32, height: u32) {
        if self.cell.0.width() == width.max(1) && self.cell.0.height() == height.max(1) {
            return;
        }
        let format = self.shared.format();
        self.cell = create_target(device, format, width, height);
        for texture in &mut self.scratch {
            *texture = create_target(device, format, width, height);
        }
        for stage in &mut self.stages {
            stage.invalidate_bind_group();
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
        } = self;
        let resolution_px = [cell.0.width() as f32, cell.0.height() as f32];
        let view = |slot: Slot| match slot {
            Slot::Cell => &cell.1,
            Slot::Scratch(index) => &scratch[index].1,
            Slot::Surface => surface,
        };
        for (position, (stage, &(input, output))) in stages.iter_mut().zip(plan.iter()).enumerate()
        {
            stage.encode(
                encoder,
                device,
                queue,
                shared,
                StageIo {
                    input: view(input),
                    output: view(output),
                    resolution_px,
                },
                StageInputs {
                    stage_index: u32::try_from(position).unwrap_or(u32::MAX),
                    ..inputs
                },
            );
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
        assert_eq!(plan(1), [(Slot::Cell, Slot::Surface)]);
        assert_eq!(scratch_count(&plan(1)), 0);
    }

    #[test]
    fn each_stage_reads_the_previous_output_through_two_alternating_intermediates() {
        let plan = plan(4);
        assert_eq!(
            plan,
            [
                (Slot::Cell, Slot::Scratch(0)),
                (Slot::Scratch(0), Slot::Scratch(1)),
                (Slot::Scratch(1), Slot::Scratch(0)),
                (Slot::Scratch(0), Slot::Surface),
            ]
        );
        assert_eq!(scratch_count(&plan), 2);
        for (input, output) in plan {
            assert_ne!(input, output, "a stage never samples its own target");
        }
    }

    #[test]
    fn an_empty_chain_plans_nothing() {
        assert_eq!(plan(0), []);
    }
}
