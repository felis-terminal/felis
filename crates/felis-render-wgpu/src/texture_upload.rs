//! Atlas texture writes. In mapped mode they are drawn into the atlas at
//! the head of the next frame instead of copied by the queue (see
//! `buffer_ring` for why a copy is avoided).

use bytemuck::{Pod, Zeroable, cast_slice};
use wgpu::{
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, Buffer, BufferAddress, BufferBindingType, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, CommandEncoder, Device, Extent3d, FragmentState,
    LoadOp, MultisampleState, Operations, Origin3d, PipelineLayoutDescriptor, PrimitiveState,
    PrimitiveTopology, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, StoreOp,
    TexelCopyBufferLayout, TexelCopyTextureInfo, Texture, TextureAspect, TextureFormat,
    TextureUsages, TextureViewDescriptor, VertexAttribute, VertexBufferLayout, VertexFormat,
    VertexState, VertexStepMode,
};

use crate::buffer_ring::UploadMode;

const SHADER_SOURCE: &str = include_str!("texture_upload.wgsl");

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Pod, Zeroable)]
struct UploadInstance {
    dst_origin: [u32; 2],
    extent: [u32; 2],
    target_size: [u32; 2],
    src: [u32; 2],
}

/// Texel layouts the upload pass can write; the view it renders through
/// is the linear twin of an sRGB atlas, so bytes land unconverted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TexelLayout {
    R8,
    Rgba8,
}

impl TexelLayout {
    const fn bytes(self) -> u32 {
        match self {
            Self::R8 => 1,
            Self::Rgba8 => 4,
        }
    }

    const fn view_format(self) -> TextureFormat {
        match self {
            Self::R8 => TextureFormat::R8Unorm,
            Self::Rgba8 => TextureFormat::Rgba8Unorm,
        }
    }
}

/// Usage and view formats an atlas of `format` needs under `mode`.
pub(crate) fn atlas_usage(mode: UploadMode) -> TextureUsages {
    match mode {
        UploadMode::Staged => TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
        UploadMode::Mapped => {
            TextureUsages::TEXTURE_BINDING
                | TextureUsages::COPY_DST
                | TextureUsages::RENDER_ATTACHMENT
        }
    }
}

pub(crate) const fn atlas_view_formats(
    mode: UploadMode,
    format: TextureFormat,
) -> &'static [TextureFormat] {
    match (mode, format) {
        (UploadMode::Mapped, TextureFormat::Rgba8UnormSrgb) => &[TextureFormat::Rgba8Unorm],
        _ => &[],
    }
}

struct Op {
    target: Texture,
    layout: TexelLayout,
    batch: usize,
    instance: UploadInstance,
}

struct Pipelines {
    bind_group_layout: BindGroupLayout,
    r8: RenderPipeline,
    rgba8: RenderPipeline,
}

pub(crate) struct TextureUploader {
    queue: Queue,
    pipelines: Option<Pipelines>,
    staging: Staging,
    ops: Vec<Op>,
}

/// Rows repacked to a 4-byte stride, split into batches that each fit one
/// storage binding.
#[derive(Debug)]
struct Staging {
    bytes: Vec<u8>,
    batch_starts: Vec<usize>,
    limit: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct Chunk {
    batch: usize,
    first_row: usize,
    rows: usize,
    /// From the start of `batch`.
    src_offset: usize,
    stride: usize,
}

impl Staging {
    fn push_rows(
        &mut self,
        src: &[u8],
        src_stride: usize,
        row_bytes: usize,
        height: usize,
    ) -> Vec<Chunk> {
        let stride = align4(row_bytes);
        let rows_per_chunk = (self.limit / stride).max(1);
        let mut chunks = Vec::new();
        let mut first_row = 0;
        while first_row < height {
            let rows = rows_per_chunk.min(height - first_row);
            let fits = self
                .batch_starts
                .last()
                .is_some_and(|&start| self.bytes.len() - start + stride * rows <= self.limit);
            if !fits {
                self.batch_starts.push(self.bytes.len());
            }
            let start = self.batch_starts.last().copied().unwrap_or(0);
            let src_offset = self.bytes.len() - start;
            for r in first_row..first_row + rows {
                let from = r * src_stride;
                self.bytes.extend_from_slice(&src[from..from + row_bytes]);
                self.bytes.resize(self.bytes.len() + stride - row_bytes, 0);
            }
            chunks.push(Chunk {
                batch: self.batch_starts.len() - 1,
                first_row,
                rows,
                src_offset,
                stride,
            });
            first_row += rows;
        }
        chunks
    }

    fn batches(&self) -> impl Iterator<Item = &[u8]> {
        self.batch_starts.iter().enumerate().map(|(i, &start)| {
            let end = self
                .batch_starts
                .get(i + 1)
                .copied()
                .unwrap_or(self.bytes.len());
            &self.bytes[start..end]
        })
    }

    /// A one-off large image leaves no resident staging behind, while a
    /// producer re-uploading every frame keeps its capacity.
    fn clear(&mut self) {
        let used = self.bytes.len();
        self.bytes.clear();
        self.batch_starts.clear();
        if used * 4 < self.bytes.capacity() {
            self.bytes.shrink_to(used);
        }
    }
}

const fn align4(n: usize) -> usize {
    n.next_multiple_of(4)
}

impl TextureUploader {
    pub(crate) fn new(device: &Device, queue: &Queue, mode: UploadMode) -> Self {
        let limits = device.limits();
        let batch_limit = limits
            .max_storage_buffer_binding_size
            .min(limits.max_buffer_size);
        let batch_limit = usize::try_from(batch_limit).unwrap_or(usize::MAX) & !3;
        Self {
            queue: queue.clone(),
            pipelines: (mode == UploadMode::Mapped).then(|| build_pipelines(device)),
            staging: Staging {
                bytes: Vec::new(),
                batch_starts: Vec::new(),
                limit: batch_limit,
            },
            ops: Vec::new(),
        }
    }

    /// `bytes` holds `height` rows of `width` texels, `bytes_per_row` apart.
    pub(crate) fn write(
        &mut self,
        target: &Texture,
        layout: TexelLayout,
        origin: [u32; 2],
        extent: [u32; 2],
        bytes: &[u8],
        bytes_per_row: u32,
    ) {
        let [width, height] = extent;
        if width == 0 || height == 0 {
            return;
        }
        let row_bytes = (width * layout.bytes()) as usize;
        if bytes.len() < (height as usize - 1) * bytes_per_row as usize + row_bytes {
            tracing::warn!(
                width,
                height,
                len = bytes.len(),
                "atlas upload shorter than its extent"
            );
            return;
        }
        if self.pipelines.is_none() {
            self.queue.write_texture(
                TexelCopyTextureInfo {
                    texture: target,
                    mip_level: 0,
                    origin: Origin3d {
                        x: origin[0],
                        y: origin[1],
                        z: 0,
                    },
                    aspect: TextureAspect::All,
                },
                bytes,
                TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
                Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            return;
        }
        for chunk in
            self.staging
                .push_rows(bytes, bytes_per_row as usize, row_bytes, height as usize)
        {
            self.ops.push(Op {
                target: target.clone(),
                layout,
                batch: chunk.batch,
                instance: UploadInstance {
                    dst_origin: [origin[0], origin[1] + chunk.first_row as u32],
                    extent: [width, chunk.rows as u32],
                    target_size: [target.width(), target.height()],
                    src: [chunk.src_offset as u32, chunk.stride as u32],
                },
            });
        }
    }

    /// Draws every write recorded since the last call into `encoder`,
    /// ahead of whatever the caller encodes next.
    pub(crate) fn encode(&mut self, device: &Device, encoder: &mut CommandEncoder) {
        let Some(pipelines) = self.pipelines.as_ref() else {
            return;
        };
        if self.ops.is_empty() {
            return;
        }
        let batch_groups: Vec<BindGroup> = self
            .staging
            .batches()
            .map(|bytes| {
                let buffer = mapped_buffer(
                    device,
                    "felis atlas upload texels",
                    BufferUsages::STORAGE,
                    bytes,
                );
                device.create_bind_group(&BindGroupDescriptor {
                    label: Some("felis atlas upload bg"),
                    layout: &pipelines.bind_group_layout,
                    entries: &[BindGroupEntry {
                        binding: 0,
                        resource: buffer.as_entire_binding(),
                    }],
                })
            })
            .collect();
        let instances: Vec<UploadInstance> = self.ops.iter().map(|op| op.instance).collect();
        let instance_buffer = mapped_buffer(
            device,
            "felis atlas upload instances",
            BufferUsages::VERTEX,
            cast_slice(&instances),
        );

        let mut first = 0;
        while first < self.ops.len() {
            let head = &self.ops[first];
            let run = self.ops[first..]
                .iter()
                .take_while(|op| {
                    op.target == head.target && op.layout == head.layout && op.batch == head.batch
                })
                .count();
            let view = head.target.create_view(&TextureViewDescriptor {
                format: Some(head.layout.view_format()),
                ..TextureViewDescriptor::default()
            });
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("felis atlas upload pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
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
            pass.set_pipeline(match head.layout {
                TexelLayout::R8 => &pipelines.r8,
                TexelLayout::Rgba8 => &pipelines.rgba8,
            });
            pass.set_bind_group(0, &batch_groups[head.batch], &[]);
            pass.set_vertex_buffer(0, instance_buffer.slice(..));
            pass.draw(0..4, first as u32..(first + run) as u32);
            drop(pass);
            first += run;
        }

        self.ops.clear();
        self.staging.clear();
    }
}

/// A buffer the GPU reads once, filled through its creation mapping.
fn mapped_buffer(device: &Device, label: &str, usage: BufferUsages, bytes: &[u8]) -> Buffer {
    let size = align4(bytes.len()).max(4) as BufferAddress;
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some(label),
        size,
        usage: usage | BufferUsages::MAP_WRITE,
        mapped_at_creation: true,
    });
    if !bytes.is_empty() {
        match buffer.get_mapped_range_mut(..bytes.len() as BufferAddress) {
            Ok(mut view) => view.copy_from_slice(bytes),
            Err(err) => tracing::warn!(?err, "atlas upload: mapping refused"),
        }
    }
    buffer.unmap();
    buffer
}

const UPLOAD_INSTANCE_ATTRS: &[VertexAttribute] = &[
    VertexAttribute {
        format: VertexFormat::Uint32x2,
        offset: 0,
        shader_location: 0,
    },
    VertexAttribute {
        format: VertexFormat::Uint32x2,
        offset: 8,
        shader_location: 1,
    },
    VertexAttribute {
        format: VertexFormat::Uint32x2,
        offset: 16,
        shader_location: 2,
    },
    VertexAttribute {
        format: VertexFormat::Uint32x2,
        offset: 24,
        shader_location: 3,
    },
];

fn build_pipelines(device: &Device) -> Pipelines {
    let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
        label: Some("felis atlas upload bgl"),
        entries: &[BindGroupLayoutEntry {
            binding: 0,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Buffer {
                ty: BufferBindingType::Storage { read_only: true },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("felis atlas upload layout"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let module = device.create_shader_module(ShaderModuleDescriptor {
        label: Some("felis atlas upload shader"),
        source: ShaderSource::Wgsl(SHADER_SOURCE.into()),
    });
    let pipeline = |entry: &str, format: TextureFormat| {
        device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("felis atlas upload pipeline"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &module,
                entry_point: Some("upload_vs"),
                buffers: &[Some(VertexBufferLayout {
                    array_stride: size_of::<UploadInstance>() as BufferAddress,
                    step_mode: VertexStepMode::Instance,
                    attributes: UPLOAD_INSTANCE_ATTRS,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(FragmentState {
                module: &module,
                entry_point: Some(entry),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: None,
                    write_mask: ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: PrimitiveState {
                topology: PrimitiveTopology::TriangleStrip,
                ..PrimitiveState::default()
            },
            depth_stencil: None,
            multisample: MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        })
    };
    Pipelines {
        r8: pipeline("upload_r8_fs", TexelLayout::R8.view_format()),
        rgba8: pipeline("upload_rgba8_fs", TexelLayout::Rgba8.view_format()),
        bind_group_layout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staging(limit: usize) -> Staging {
        Staging {
            bytes: Vec::new(),
            batch_starts: Vec::new(),
            limit,
        }
    }

    #[test]
    fn rows_are_padded_to_a_four_byte_stride_and_drop_source_padding() {
        let mut st = staging(1 << 20);
        // Two 3-byte rows, 5 bytes apart in the source.
        let chunks = st.push_rows(&[1, 2, 3, 9, 9, 4, 5, 6], 5, 3, 2);
        assert_eq!(
            chunks,
            vec![Chunk {
                batch: 0,
                first_row: 0,
                rows: 2,
                src_offset: 0,
                stride: 4,
            }]
        );
        assert_eq!(st.bytes, vec![1, 2, 3, 0, 4, 5, 6, 0]);
    }

    #[test]
    fn a_write_larger_than_the_limit_splits_by_rows_across_batches() {
        let mut st = staging(8);
        let src: Vec<u8> = (0..12).collect();
        let chunks = st.push_rows(&src, 4, 4, 3);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            (chunks[0].batch, chunks[0].first_row, chunks[0].rows),
            (0, 0, 2)
        );
        assert_eq!(
            (chunks[1].batch, chunks[1].first_row, chunks[1].rows),
            (1, 2, 1)
        );
        assert_eq!(
            chunks[1].src_offset, 0,
            "offsets are relative to their batch"
        );
        let batches: Vec<&[u8]> = st.batches().collect();
        assert_eq!(batches, vec![&src[..8], &src[8..]]);
    }

    #[test]
    fn a_later_write_opens_a_batch_only_when_the_current_one_is_full() {
        let mut st = staging(8);
        st.push_rows(&[1; 4], 4, 4, 1);
        let second = st.push_rows(&[2; 4], 4, 4, 1);
        assert_eq!((second[0].batch, second[0].src_offset), (0, 4));
        let third = st.push_rows(&[3; 4], 4, 4, 1);
        assert_eq!((third[0].batch, third[0].src_offset), (1, 0));
    }

    #[test]
    fn a_row_wider_than_the_limit_still_travels_alone() {
        let mut st = staging(4);
        let chunks = st.push_rows(&[7; 16], 8, 8, 2);
        assert_eq!(
            chunks.iter().map(|c| c.batch).collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn clear_releases_capacity_a_much_smaller_frame_does_not_need() {
        let mut st = staging(1 << 20);
        st.push_rows(&[0; 4096], 4096, 4096, 1);
        st.clear();
        let big = st.bytes.capacity();
        st.push_rows(&[0; 4], 4, 4, 1);
        st.clear();
        assert!(st.bytes.capacity() < big);
        assert_eq!(st.batch_starts, [0_usize; 0]);
    }
}
