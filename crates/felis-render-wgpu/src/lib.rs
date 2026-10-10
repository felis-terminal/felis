#![recursion_limit = "256"]

//! `felis-render-wgpu`: wgpu pipeline, glyph and image atlases, decorations.
//!
//! The surface is built from a `RawWindowHandle`-bearing handle the
//! caller supplies; this crate has no `winit` dependency.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    num::NonZeroU32,
    path::PathBuf,
    sync::Arc,
};

use bytemuck::cast_slice;
use felis_grid::{
    ScreenBuffer,
    images::{ClientPlacement, VirtualPlacement, clip_source},
};
use felis_protocol::{
    ImageId,
    messages::{CursorStyle, ImageFormat, SourceRect, ThemeChannel},
};
pub use felis_shaping::FaceSpec;
use felis_shaping::{CellMetrics, FontStack, ShapingError, StackDescription, StyleFaces};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use thiserror::Error;
use wgpu::{
    Adapter, Backends, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayoutDescriptor,
    BlendState, Buffer, Color, ColorTargetState, ColorWrites, CommandEncoder,
    CommandEncoderDescriptor, Device, DeviceDescriptor, FragmentState, FrontFace, Instance,
    InstanceDescriptor, LoadOp, MemoryHints, MultisampleState, Operations, PipelineLayout,
    PipelineLayoutDescriptor, PolygonMode, PowerPreference, PresentMode, PrimitiveState,
    PrimitiveTopology, Queue, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, RequestAdapterOptions, ShaderModuleDescriptor, ShaderSource, StoreOp,
    Surface, SurfaceConfiguration, TextureFormat, TextureUsages, TextureView,
    TextureViewDescriptor, Trace, VertexBufferLayout, VertexState,
};

pub mod atlas;
pub mod box_drawing;
mod buffer_ring;
mod capture;
pub mod glyphs;
mod gpu_resources;
pub mod image_atlas;
pub mod instances;
pub mod palette;
pub mod pipeline;
pub mod post;
pub mod probe;
mod row_cache;
mod texture_upload;
mod warmup;

pub use crate::capture::{FrameCapture, ReadFrameError};
pub use crate::post::{
    CONTRACT_VERSION, MouseState, PostShaderError, PostUniforms, TRAIL_SHADER_SOURCE, TrailState,
    validate_post_shader,
};
pub use crate::warmup::Warmup;
use crate::{
    buffer_ring::{GrowingInstanceBuffer, UniformRing, UploadMode},
    glyphs::GlyphCache,
    image_atlas::{ImageAtlas, ImageSlot},
    instances::{
        BAR_ELLIPSIS, BgInstance, CURSOR_MARKER_PX, CellPainter, ChromeBar, DecorationInstance,
        FgInstance, ImgInstance, bottom_bar_claim, clip_img_quad_above, extend_confirm_instances,
        extend_link_preview_instances, extend_preedit_instances, extend_search_instances,
        preedit_covered_cols,
    },
    palette::{ResolvedTheme, Theme, effective_theme},
    pipeline::{
        ViewportUniform, atlas_bind_group_layout_entries, bg_instance_layout,
        decoration_instance_layout, fg_instance_layout, glyph_bind_group_layout_entries,
        img_instance_layout, viewport_bind_group_layout_entries,
    },
    post::{PostFrame, PostStage},
    row_cache::{FrameKey, RowCache},
    texture_upload::TextureUploader,
};

/// `start` / `end` are normalized by the client: row-major min/max for
/// a linear selection, axis-aligned bounding box for a rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: (u16, u16),
    pub end: (u16, u16),
    pub rectangle: bool,
}

pub use crate::instances::{
    ConfirmOverlay, LinkPreviewOverlay, PreeditOverlay, SearchHitSpan, SearchOverlay,
};
/// Re-exported so the client can match on `Renderer::render`'s error
/// without a `wgpu` dependency.
#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error("surface lost")]
    Lost,
    #[error("surface outdated")]
    Outdated,
    #[error("surface timeout")]
    Timeout,
    #[error("out of memory")]
    OutOfMemory,
    #[error("surface error: {0}")]
    Other(String),
}

/// `pixels.len()` equals `width * height * bytes_per_pixel(format)`;
/// the daemon dispatcher enforces it.
#[derive(Debug, Clone, Copy)]
pub struct ImageData<'a> {
    pub width: u32,
    pub height: u32,
    pub format: ImageFormat,
    pub pixels: &'a [u8],
}

/// The recovery set after an atlas reset, deduped in first-mention
/// order. Anything a live placement names must be in it: a producer
/// never retransmits an image it already delivered.
fn live_image_ids(
    dirty_images: &[ImageId],
    placements: &[ClientPlacement],
    virtual_placements: &[VirtualPlacement],
) -> Vec<ImageId> {
    let mut seen = HashSet::new();
    dirty_images
        .iter()
        .copied()
        .chain(placements.iter().map(|p| p.image_id))
        .chain(virtual_placements.iter().map(|v| v.image_id))
        .filter(|id| seen.insert(*id))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadOutcome {
    Uploaded,
    SheetFull,
    Missing,
}

/// Supplies image pixels for GPU atlas re-uploads when the atlas recycles itself.
///
/// Images are held in `felis_client_core::ImageShadow`. Returns `None` if pixels have not
/// fully arrived or were deleted, in which case placements paint nothing.
pub trait ImageSource {
    fn image_data(&self, id: ImageId) -> Option<ImageData<'_>>;
}

#[derive(Debug, Error)]
pub enum RendererError {
    #[error("create surface: {0}")]
    CreateSurface(String),
    #[error("no adapter compatible with surface")]
    NoAdapter,
    #[error("request device: {0}")]
    RequestDevice(String),
    #[error("surface has no supported texture format")]
    NoFormat,
    #[error("font: {0}")]
    Font(#[from] ShapingError),
    #[error("an offscreen renderer has no post-process stage")]
    OffscreenPostShader,
}

/// In the logical pixels `font.size_px` is defined in.
pub const DEFAULT_FONT_SIZE_LOGICAL_PX: f32 = 14.0;

/// Equal to the logical default (scale 1 is all a caller with no window
/// can mean), but its own constant: this crate's sizes are physical
/// pixels, and using the `_LOGICAL_` name at a physical site is how a
/// scale-2 display rasterizes at half size.
const DEFAULT_FONT_SIZE_PHYSICAL_PX: f32 = DEFAULT_FONT_SIZE_LOGICAL_PX;

#[derive(Debug, Default, Clone)]
pub struct RendererConfig {
    /// `None` resolves to `Family::Monospace`.
    pub font_family: Option<String>,
    /// Physical pixels: the caller multiplies the logical `font.size_px` by
    /// the window scale factor. `None` rasterizes at the default logical
    /// size at scale 1.
    pub font_size_physical_px: Option<f32>,
    pub theme_fg: Option<String>,
    pub theme_bg: Option<String>,
    /// Sparse `[theme.palette]` overrides by index; absent indices keep
    /// the xterm baseline.
    pub theme_palette: BTreeMap<u8, String>,
    /// `font.fallback`; empty enables auto-discovery.
    pub font_fallbacks: Vec<FaceSpec>,
    pub font_features: Vec<String>,
    pub font_bold: FaceSpec,
    pub font_italic: FaceSpec,
    pub font_bold_italic: FaceSpec,
    /// `None` keeps the reverse-video cursor.
    pub theme_cursor: Option<String>,
    /// `0.0..=1.0`; `None` and `Some(1.0)` keep the opaque surface. The
    /// caller must also create the window with `with_transparent(true)`
    /// or the OS ignores the alpha.
    pub background_opacity: Option<f32>,
    /// WGSL source, already resolved from `shader.post` by the caller
    /// (this crate takes no paths). `None` keeps the single-pass path.
    pub post_shader_wgsl: Option<String>,
    /// Font files that replace the system's fonts for every face lookup
    /// (primary, styles, fallbacks); empty uses the system's. Fixed at
    /// construction: [`Renderer::reload_font`] keeps the renderer's own.
    pub font_files: Vec<PathBuf>,
}

impl RendererConfig {
    #[must_use]
    pub const fn style_faces(&self) -> StyleFaces<'_> {
        StyleFaces {
            bold: &self.font_bold,
            italic: &self.font_italic,
            bold_italic: &self.font_bold_italic,
        }
    }
}

/// Image-atlas side ceiling; the actual side is clamped to the adapter's
/// 2D limit. The 256 MiB `Rgba8` sheet is allocated on first image
/// upload, so an image-free session never pays for it.
pub const MAX_ATLAS_SIDE: u32 = 8192;

/// Glyphs are cell-sized, so the 8192 image ceiling would be gross
/// over-allocation: a 2048² `R8` sheet is 4 MiB against 64 MiB and holds
/// thousands of slots; the packer's evict-and-rebuild path absorbs
/// overflow. Revisit if CJK-heavy sessions show glyph atlas churn.
pub const GLYPH_ATLAS_SIDE: u32 = 2048;

/// Floor for a downlevel adapter: below this no useful shelf fits, and
/// the packer's side must stay non-zero whatever the adapter claims.
const MIN_ATLAS_SIDE: NonZeroU32 = match NonZeroU32::new(256) {
    Some(side) => side,
    None => NonZeroU32::MIN,
};

const fn atlas_side(ceiling: u32, adapter_max_2d: u32) -> NonZeroU32 {
    let clamped = if ceiling < adapter_max_2d {
        ceiling
    } else {
        adapter_max_2d
    };
    if clamped <= MIN_ATLAS_SIDE.get() {
        return MIN_ATLAS_SIDE;
    }
    match NonZeroU32::new(clamped) {
        Some(side) => side,
        None => MIN_ATLAS_SIDE,
    }
}

fn new_instance() -> Instance {
    Instance::new(InstanceDescriptor {
        backends: Backends::PRIMARY,
        ..InstanceDescriptor::new_without_display_handle()
    })
}

async fn request_adapter(
    instance: &Instance,
    compatible_surface: Option<&Surface<'_>>,
) -> Result<Adapter, RendererError> {
    instance
        .request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            compatible_surface,
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await
        .map_err(|_| RendererError::NoAdapter)
}

async fn request_device(adapter: &Adapter) -> Result<(Device, Queue), RendererError> {
    adapter
        .request_device(&DeviceDescriptor {
            label: Some("felis-render-wgpu device"),
            required_features: UploadMode::for_adapter(adapter).required_features(),
            required_limits: adapter.limits(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            // Not `Performance`: on NVIDIA/Vulkan its pre-mapped
            // allocator blocks add ~120 MB to RSS (142 MB vs 22 MB on
            // the /dev/nvidiactl mapping) for allocations that are
            // tiny and rare here.
            memory_hints: MemoryHints::MemoryUsage,
            trace: Trace::Off,
        })
        .await
        .map_err(|e| RendererError::RequestDevice(e.to_string()))
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent paint-time switches (surface alpha mode, focus, blink phase, DECSCNM), not disjoint states of one machine"
)]
pub struct Renderer {
    target: Target,
    device: Device,
    queue: Queue,
    format: TextureFormat,
    width: u32,
    height: u32,
    pub clear_color: Color,
    bg_premultiplied: bool,
    theme: Theme,
    /// `OSC 10` / `11` / `12` overrides, indexed by `ThemeChannel as
    /// usize`; `None` keeps the configured value.
    runtime_theme: [Option<[u8; 3]>; 3],
    runtime_palette: Box<[Option<[u8; 3]>; 256]>,
    resolved_theme: ResolvedTheme,
    reverse_video: bool,
    glyph_repaint_pending: bool,
    glyph_atlas_thrash_logged: bool,
    image_atlas_overflow_logged: bool,
    /// Defaults to `true`: winit creates the window focused on every
    /// target platform.
    window_focused: bool,
    cursor_blink_visible: bool,
    selection: Option<SelectionRange>,
    preedit: Option<PreeditOverlay>,
    search: Option<SearchOverlay>,
    confirm: Option<ConfirmOverlay>,
    link_preview: Option<LinkPreviewOverlay>,
    /// Bottom rows the last `build_cell_instances` handed to the client's
    /// own chrome; read by the image builders, which run after it.
    chrome_rows: u16,
    content_origin_px: [f32; 2],
    bg_pipeline: RenderPipeline,
    fg_pipeline: RenderPipeline,
    deco_pipeline: RenderPipeline,
    image_pipeline: RenderPipeline,
    upload_mode: UploadMode,
    uploader: TextureUploader,
    viewport_bgl: wgpu::BindGroupLayout,
    viewport: UniformRing,
    viewport_written: ViewportUniform,
    bg_buffer: GrowingInstanceBuffer<BgInstance>,
    fg_buffer: GrowingInstanceBuffer<FgInstance>,
    deco_buffer: GrowingInstanceBuffer<DecorationInstance>,
    image_under_bg_buffer: GrowingInstanceBuffer<ImgInstance>,
    image_under_text_buffer: GrowingInstanceBuffer<ImgInstance>,
    image_above_buffer: GrowingInstanceBuffer<ImgInstance>,
    glyphs: GlyphCache,
    images: ImageAtlas,
    shaper: felis_shaping::Shaper,
    font_features: Vec<String>,
    font_files: Vec<PathBuf>,
    shape_frame: glyphs::ShapeFrame,
    cells: RowCache,
    image_scratch_under_bg: Vec<ImgInstance>,
    image_scratch_under_text: Vec<ImgInstance>,
    image_scratch_above: Vec<ImgInstance>,
    vp_map: HashMap<ImageId, VirtualPlacement>,
    post: Option<PostStage>,
    /// Kept even with no stage, so a shader enabled by a live reload
    /// starts from the current state rather than the origin.
    trail_state: TrailState,
    mouse_state: MouseState,
    last_counts: Option<DrawCounts>,
}

enum Target {
    Surface {
        surface: Surface<'static>,
        config: SurfaceConfiguration,
    },
    Offscreen(Offscreen),
}

/// The format [`Renderer::new_offscreen`] draws in: fixed, so a caller
/// reading the pixels back need not negotiate one.
const OFFSCREEN_FORMAT: TextureFormat = TextureFormat::Rgba8UnormSrgb;

struct Offscreen {
    texture: wgpu::Texture,
    view: TextureView,
    /// Built on the first [`Renderer::read_frame`] and dropped on resize.
    staging: Option<Buffer>,
    /// A fresh texture holds no frame, and reading it back would hand
    /// the caller a blank image as if it had been drawn.
    rendered: bool,
}

impl Offscreen {
    fn new(device: &Device, width: u32, height: u32) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("felis offscreen target"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OFFSCREEN_FORMAT,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&TextureViewDescriptor::default());
        Self {
            texture,
            view,
            staging: None,
            rendered: false,
        }
    }
}

#[derive(Clone, Copy)]
struct DrawCounts {
    bg: u32,
    fg: u32,
    deco: u32,
    img_under_bg: u32,
    img_under_text: u32,
    img_above: u32,
}

struct Parts {
    target: Target,
    device: Device,
    queue: Queue,
    upload_mode: UploadMode,
    format: TextureFormat,
    width: u32,
    height: u32,
    bg_premultiplied: bool,
}

fn offscreen_config(mut cfg: RendererConfig) -> Result<RendererConfig, RendererError> {
    if cfg.post_shader_wgsl.is_some() {
        return Err(RendererError::OffscreenPostShader);
    }
    cfg.background_opacity = None;
    Ok(cfg)
}

fn resolve_font_stack(
    warmup: Option<&mut Warmup>,
    cfg: &RendererConfig,
) -> Result<FontStack, RendererError> {
    match warmup.and_then(|w| w.take_fonts(cfg)) {
        Some(stack) => Ok(stack?),
        None => Ok(discover_fonts(
            &cfg.font_files,
            cfg.font_family.as_deref(),
            &cfg.font_features,
            &cfg.font_fallbacks,
            &cfg.style_faces(),
        )?),
    }
}

/// The stack [`Renderer::new_with_config`] would resolve from `cfg`,
/// described rather than kept.
pub fn describe_fonts(cfg: &RendererConfig) -> Result<StackDescription, ShapingError> {
    discover_fonts(
        &cfg.font_files,
        cfg.font_family.as_deref(),
        &cfg.font_features,
        &cfg.font_fallbacks,
        &cfg.style_faces(),
    )
    .map(|stack| stack.describe())
}

fn discover_fonts(
    files: &[PathBuf],
    family: Option<&str>,
    features: &[String],
    fallbacks: &[FaceSpec],
    styles: &StyleFaces<'_>,
) -> Result<FontStack, ShapingError> {
    if files.is_empty() {
        FontStack::auto_discover(family, features, fallbacks, styles)
    } else {
        FontStack::discover_in_files(files, family, features, fallbacks, styles)
    }
}

impl Renderer {
    /// `(width, height)` is the initial framebuffer size in physical
    /// pixels.
    pub async fn new<T>(target: Arc<T>, width: u32, height: u32) -> Result<Self, RendererError>
    where
        T: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        Self::new_with_config(target, width, height, RendererConfig::default(), None).await
    }

    /// The result and its errors are those of a call with no `warmup`.
    pub async fn new_with_config<T>(
        target: Arc<T>,
        width: u32,
        height: u32,
        cfg: RendererConfig,
        mut warmup: Option<Warmup>,
    ) -> Result<Self, RendererError>
    where
        T: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let (instance, warm_device) = match warmup.as_mut().and_then(Warmup::take_gpu) {
            Some(gpu) => (gpu.instance, gpu.device),
            None => (new_instance(), None),
        };

        let surface = instance
            .create_surface(target)
            .map_err(|e| RendererError::CreateSurface(e.to_string()))?;

        // The warm adapter was picked without the surface. Picking with it
        // only drops the adapters that cannot present to it and keeps the
        // order, so a warm adapter that can present is the one that
        // picking with the surface would have returned.
        let (adapter, device, queue) = match warm_device {
            Some((adapter, device, queue)) if adapter.is_surface_supported(&surface) => {
                (adapter, device, queue)
            }
            warm => {
                if warm.is_some() {
                    tracing::debug!(
                        "warm GPU adapter cannot present to this window; picking again"
                    );
                }
                let adapter = request_adapter(&instance, Some(&surface)).await?;
                let (device, queue) = request_device(&adapter).await?;
                (adapter, device, queue)
            }
        };

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(TextureFormat::is_srgb)
            .or_else(|| caps.formats.first().copied())
            .ok_or(RendererError::NoFormat)?;

        let opacity = cfg.background_opacity.map_or(1.0, |a| a.clamp(0.0, 1.0));
        let (alpha_mode, bg_premultiplied) = select_alpha_mode(&caps.alpha_modes, opacity);

        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: width.max(1),
            height: height.max(1),
            present_mode: PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let upload_mode = UploadMode::for_adapter(&adapter);
        let stack = resolve_font_stack(warmup.as_mut(), &cfg)?;
        Ok(Self::build(
            Parts {
                target: Target::Surface { surface, config },
                device,
                queue,
                upload_mode,
                format,
                width: width.max(1),
                height: height.max(1),
                bg_premultiplied,
            },
            &cfg,
            stack,
        ))
    }

    /// Draws into a texture of its own instead of a window, for
    /// [`Self::read_frame`]. `background_opacity` is ignored, so the
    /// target holds straight RGB. A post-process shader is refused: its
    /// uniforms read the host clock, and the trail shader paints the
    /// whole frame unless its caller drives [`TrailState`].
    pub async fn new_offscreen(
        width: u32,
        height: u32,
        cfg: RendererConfig,
        mut warmup: Option<Warmup>,
    ) -> Result<Self, RendererError> {
        let cfg = offscreen_config(cfg)?;
        let (instance, warm_device) = match warmup.as_mut().and_then(Warmup::take_gpu) {
            Some(gpu) => (gpu.instance, gpu.device),
            None => (new_instance(), None),
        };
        let (adapter, device, queue) = if let Some(warm) = warm_device {
            warm
        } else {
            let adapter = request_adapter(&instance, None).await?;
            let (device, queue) = request_device(&adapter).await?;
            (adapter, device, queue)
        };
        let upload_mode = UploadMode::for_adapter(&adapter);
        let stack = resolve_font_stack(warmup.as_mut(), &cfg)?;
        Ok(Self::build_offscreen(
            device,
            queue,
            upload_mode,
            width,
            height,
            &cfg,
            stack,
        ))
    }

    fn build_offscreen(
        device: Device,
        queue: Queue,
        upload_mode: UploadMode,
        width: u32,
        height: u32,
        cfg: &RendererConfig,
        stack: FontStack,
    ) -> Self {
        let (width, height) = (width.max(1), height.max(1));
        Self::build(
            Parts {
                target: Target::Offscreen(Offscreen::new(&device, width, height)),
                device,
                queue,
                upload_mode,
                format: OFFSCREEN_FORMAT,
                width,
                height,
                bg_premultiplied: false,
            },
            cfg,
            stack,
        )
    }

    fn build(parts: Parts, cfg: &RendererConfig, stack: FontStack) -> Self {
        let Parts {
            target,
            device,
            queue,
            upload_mode,
            format,
            width,
            height,
            bg_premultiplied,
        } = parts;
        let adapter_max_2d = device.limits().max_texture_dimension_2d;

        // Records only; recovery runs through the `Lost` / `Outdated`
        // surface-error path in `render`.
        device.set_device_lost_callback(|reason, message| {
            tracing::error!(?reason, message, "wgpu device lost");
        });

        let viewport_bgl = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("felis viewport bgl"),
            entries: &viewport_bind_group_layout_entries(),
        });
        let atlas_bgl = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("felis atlas bgl"),
            entries: &atlas_bind_group_layout_entries(),
        });
        let glyph_bgl = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("felis glyph bgl"),
            entries: &glyph_bind_group_layout_entries(),
        });

        let initial_viewport = ViewportUniform::new(width, height);
        let mut viewport = UniformRing::new("felis viewport uniform", upload_mode);
        viewport.write(&device, &queue, bytemuck::bytes_of(&initial_viewport));
        viewport.prepare_bind_group(|buf| viewport_bind_group(&device, &viewport_bgl, buf));

        let module = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("felis shader"),
            source: ShaderSource::Wgsl(pipeline::SHADER_SOURCE.into()),
        });
        let bg_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("felis bg pipeline layout"),
            bind_group_layouts: &[Some(&viewport_bgl)],
            immediate_size: 0,
        });
        let fg_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("felis fg pipeline layout"),
            bind_group_layouts: &[Some(&viewport_bgl), Some(&glyph_bgl)],
            immediate_size: 0,
        });
        let bg_pipeline = build_pipeline(
            &device,
            &module,
            format,
            PipelineSpec {
                layout: &bg_layout,
                vs_entry: "bg_vs",
                fs_entry: "bg_fs",
                buffer_layout: bg_instance_layout(),
                blend: None,
            },
        );
        let fg_pipeline = build_pipeline(
            &device,
            &module,
            format,
            PipelineSpec {
                layout: &fg_layout,
                vs_entry: "fg_vs",
                fs_entry: "fg_fs",
                buffer_layout: fg_instance_layout(),
                blend: Some(BlendState::ALPHA_BLENDING),
            },
        );
        let deco_pipeline = build_pipeline(
            &device,
            &module,
            format,
            PipelineSpec {
                layout: &bg_layout,
                vs_entry: "deco_vs",
                fs_entry: "deco_fs",
                buffer_layout: decoration_instance_layout(),
                blend: Some(BlendState::ALPHA_BLENDING),
            },
        );
        let image_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("felis image pipeline layout"),
            bind_group_layouts: &[Some(&viewport_bgl), Some(&atlas_bgl)],
            immediate_size: 0,
        });
        let image_pipeline = build_pipeline(
            &device,
            &module,
            format,
            PipelineSpec {
                layout: &image_layout,
                vs_entry: "image_vs",
                fs_entry: "image_fs",
                buffer_layout: img_instance_layout(),
                blend: Some(BlendState::ALPHA_BLENDING),
            },
        );

        let glyph_side = atlas_side(GLYPH_ATLAS_SIDE, adapter_max_2d);
        let image_side = atlas_side(MAX_ATLAS_SIDE, adapter_max_2d);
        let font_size_physical_px = cfg
            .font_size_physical_px
            .unwrap_or(DEFAULT_FONT_SIZE_PHYSICAL_PX);
        let glyphs = GlyphCache::new(
            &device,
            upload_mode,
            &glyph_bgl,
            stack,
            font_size_physical_px,
            glyph_side,
        );
        let images = ImageAtlas::new(&device, upload_mode, &atlas_bgl, image_side);
        let uploader = TextureUploader::new(&device, &queue, upload_mode);

        // Logged on `felis::mem`, the target the client logs its RSS
        // milestones on.
        #[expect(
            clippy::items_after_statements,
            reason = "kept beside its only use in this footprint-trace block rather than hoisted away from context"
        )]
        const MIB: f64 = 1024.0 * 1024.0;
        let glyph_texels = u64::from(glyph_side.get()) * u64::from(glyph_side.get());
        let image_texels = u64::from(image_side.get()) * u64::from(image_side.get());
        tracing::info!(
            target: "felis::mem",
            glyph_side = glyph_side.get(),
            image_side = image_side.get(),
            glyph_atlas_mib = glyph_texels as f64 / MIB,
            image_atlas_mib_on_demand = (image_texels * 4) as f64 / MIB,
            ?upload_mode,
            "GPU atlas allocation budget",
        );

        // A refused shader is not a refused window: keep the single-pass
        // path, as a live reload does.
        let post = cfg.post_shader_wgsl.as_deref().and_then(|src| {
            match PostStage::new(&device, upload_mode, format, width, height, src) {
                Ok(stage) => Some(stage),
                Err(err) => {
                    tracing::error!(%err, "post-process shader refused; rendering without it");
                    None
                }
            }
        });

        let theme = Self::theme_from_config(cfg);
        let resolved_theme = ResolvedTheme::new(&premultiplied_bg(theme, bg_premultiplied));
        let clear_color = clear_color_for_theme(&resolved_theme, false);

        let cells = RowCache::new(device.limits().max_buffer_size);
        Self {
            target,
            device,
            queue,
            format,
            width,
            height,
            clear_color,
            bg_premultiplied,
            theme,
            runtime_theme: [None; 3],
            runtime_palette: Box::new([None; 256]),
            resolved_theme,
            reverse_video: false,
            glyph_repaint_pending: false,
            glyph_atlas_thrash_logged: false,
            image_atlas_overflow_logged: false,
            window_focused: true,
            cursor_blink_visible: true,
            selection: None,
            preedit: None,
            search: None,
            confirm: None,
            link_preview: None,
            chrome_rows: 0,
            content_origin_px: [0.0, 0.0],
            bg_pipeline,
            fg_pipeline,
            deco_pipeline,
            image_pipeline,
            upload_mode,
            uploader,
            viewport_bgl,
            viewport,
            viewport_written: initial_viewport,
            bg_buffer: GrowingInstanceBuffer::new("felis bg instances", upload_mode),
            fg_buffer: GrowingInstanceBuffer::new("felis fg instances", upload_mode),
            deco_buffer: GrowingInstanceBuffer::new("felis decoration instances", upload_mode),
            image_under_bg_buffer: GrowingInstanceBuffer::new(
                "felis image-under-bg instances",
                upload_mode,
            ),
            image_under_text_buffer: GrowingInstanceBuffer::new(
                "felis image-under-text instances",
                upload_mode,
            ),
            image_above_buffer: GrowingInstanceBuffer::new(
                "felis image-above instances",
                upload_mode,
            ),
            glyphs,
            images,
            shaper: felis_shaping::Shaper::new(),
            font_features: cfg.font_features.clone(),
            font_files: cfg.font_files.clone(),
            shape_frame: glyphs::ShapeFrame::empty(),
            cells,
            image_scratch_under_bg: Vec::new(),
            image_scratch_under_text: Vec::new(),
            image_scratch_above: Vec::new(),
            vp_map: HashMap::new(),
            post,
            trail_state: TrailState::default(),
            mouse_state: MouseState::default(),
            last_counts: None,
        }
    }

    #[must_use]
    pub const fn cell_metrics(&self) -> CellMetrics {
        self.glyphs.cell_metrics()
    }

    /// Zero dimensions clamp to 1; wgpu refuses zero-extent surfaces.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width.max(1);
        self.height = height.max(1);
        match &mut self.target {
            Target::Surface { surface, config } => {
                config.width = self.width;
                config.height = self.height;
                surface.configure(&self.device, config);
            }
            Target::Offscreen(offscreen) => {
                *offscreen = Offscreen::new(&self.device, self.width, self.height);
            }
        }
        if let Some(post) = self.post.as_mut() {
            post.resize(&self.device, self.format, self.width, self.height);
        }
    }

    pub fn set_post_shader(&mut self, wgsl: Option<&str>) -> Result<(), PostShaderError> {
        match wgsl {
            None => {
                self.post = None;
                Ok(())
            }
            Some(_) if matches!(self.target, Target::Offscreen(_)) => {
                Err(PostShaderError::Offscreen)
            }
            Some(src) => {
                let stage = PostStage::new(
                    &self.device,
                    self.upload_mode,
                    self.format,
                    self.width,
                    self.height,
                    src,
                )?;
                self.post = Some(stage);
                Ok(())
            }
        }
    }

    #[must_use]
    pub const fn has_post_shader(&self) -> bool {
        self.post.is_some()
    }

    /// The client owns the easing clock: only it can arm and drop the
    /// frame timer the animation's end must stop.
    pub const fn set_trail_state(&mut self, state: TrailState) {
        self.trail_state = state;
    }

    pub const fn set_mouse_state(&mut self, state: MouseState) {
        self.mouse_state = state;
    }

    /// Cursor rectangle in UV space (origin top-left), the same rectangle
    /// the cell pass paints for `style`, so a bar or underline cursor
    /// trails its own thin shape.
    #[must_use]
    pub fn cursor_rect_uv(&self, screen: &ScreenBuffer, row: u16, col: u16) -> [f32; 4] {
        let [x, y, w, h] = cursor_rect_px(
            self.glyphs.cell_metrics(),
            self.content_origin_px,
            row,
            screen.char_span(row, col),
            screen.cursor_style(),
        );
        let (vw, vh) = (self.width as f32, self.height as f32);
        [x / vw, y / vh, w / vw, h / vh]
    }

    /// After `SurfaceError::Lost` / `Outdated`, so the next redraw
    /// acquires a fresh swapchain texture. A no-op offscreen.
    pub fn reconfigure_surface(&self) {
        if let Target::Surface { surface, config } = &self.target {
            surface.configure(&self.device, config);
        }
    }

    pub fn apply_theme_override(&mut self, channel: ThemeChannel, rgb: Option<(u8, u8, u8)>) {
        self.runtime_theme[channel as usize] = rgb.map(<[u8; 3]>::from);
        self.refresh_theme();
    }

    /// Resolved at paint time, so the recolor reaches cells already on
    /// screen: a cell stores the palette index, never a color.
    pub fn apply_palette_override(&mut self, index: u8, rgb: Option<(u8, u8, u8)>) {
        self.runtime_palette[usize::from(index)] = rgb.map(<[u8; 3]>::from);
        self.refresh_theme();
    }

    /// Bare `OSC 104`. The `OSC 10` / `11` / `12` channels stay: xterm's
    /// `OSC 104` addresses the indexed palette alone, and their reset is
    /// `OSC 110` / `111` / `112`.
    pub fn reset_palette_overrides(&mut self) {
        *self.runtime_palette = [None; 256];
        self.refresh_theme();
    }

    /// Called at the start of each rehydrate: the burst replays only the
    /// overrides in force, so it cannot unset the previous session's.
    pub fn reset_session_colors(&mut self) {
        self.runtime_theme = [None; 3];
        *self.runtime_palette = [None; 256];
        self.refresh_theme();
    }

    /// Resolved at paint time, so the flip reaches scrollback rows with
    /// no re-transmission; image pixels and client chrome stay untouched.
    pub fn set_reverse_video(&mut self, on: bool) {
        if self.reverse_video == on {
            return;
        }
        self.reverse_video = on;
        self.clear_color = clear_color_for_theme(&self.resolved_theme, on);
    }

    fn refresh_theme(&mut self) {
        self.resolved_theme = ResolvedTheme::new(&premultiplied_bg(
            effective_theme(&self.theme, &self.runtime_theme, &self.runtime_palette),
            self.bg_premultiplied,
        ));
        self.clear_color = clear_color_for_theme(&self.resolved_theme, self.reverse_video);
        self.cells.invalidate();
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        self.theme = *theme;
        self.refresh_theme();
    }

    pub fn reload_font(
        &mut self,
        family: Option<&str>,
        size_physical_px: Option<f32>,
        features: &[String],
        fallbacks: &[FaceSpec],
        styles: &StyleFaces<'_>,
    ) -> Result<CellMetrics, ShapingError> {
        let stack = discover_fonts(&self.font_files, family, features, fallbacks, styles)?;
        let font_size_physical_px = size_physical_px.unwrap_or(DEFAULT_FONT_SIZE_PHYSICAL_PX);
        self.glyphs.reload_font(stack, font_size_physical_px);
        self.font_features = features.to_vec();
        self.cells.invalidate();
        Ok(self.glyphs.cell_metrics())
    }

    /// Size only, skipping the fontdb rescan [`Self::reload_font`] pays:
    /// on a system with hundreds of faces the scan dominates the zoom
    /// frame budget and stutters while the chord is held.
    pub fn reload_font_size(&mut self, size_physical_px: f32) -> CellMetrics {
        self.glyphs.reload_font_size(size_physical_px);
        self.cells.invalidate();
        self.glyphs.cell_metrics()
    }

    pub fn reload_font_features(&mut self, features: &[String]) {
        self.font_features = features.to_vec();
        // Same-size reload to reset every atlas slot: glyph-id slots from
        // a prior features pass go stale (a `=>` ligature populated under
        // `calt` must not resolve after `calt` is turned off).
        self.glyphs
            .reload_font_size(self.glyphs.font_size_physical_px());
        self.cells.invalidate();
    }

    #[must_use]
    pub fn theme_from_config(cfg: &RendererConfig) -> Theme {
        let mut theme = Theme::with_overrides(cfg.theme_fg.as_deref(), cfg.theme_bg.as_deref())
            .with_palette(&cfg.theme_palette)
            .with_cursor(cfg.theme_cursor.as_deref());
        // Only the default background (and the letterbox clear) carries
        // the opacity; a cell with an explicit bg stays at alpha 1.0,
        // kitty's `background_opacity` model.
        if let Some(a) = cfg.background_opacity {
            theme.bg[3] = a.clamp(0.0, 1.0);
        }
        theme
    }

    pub const fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    pub const fn set_cursor_blink_visible(&mut self, visible: bool) {
        self.cursor_blink_visible = visible;
    }

    /// Letterbox origin of the last frame, in physical pixels
    /// (docs/explanation/architecture/session-lifecycle.md "Same-user
    /// mirroring"). The client subtracts it when translating mouse
    /// pixels to cells, or clicks land a margin off on a letterboxed
    /// mirror.
    #[must_use]
    // The lint's `.into()` is not const; keep this getter const with the
    // other simple renderer accessors.
    #[allow(clippy::tuple_array_conversions)]
    pub const fn content_origin_px(&self) -> (f32, f32) {
        (self.content_origin_px[0], self.content_origin_px[1])
    }

    pub const fn set_selection(&mut self, range: Option<SelectionRange>) {
        self.selection = range;
    }

    pub fn set_preedit(&mut self, overlay: Option<PreeditOverlay>) {
        self.preedit = overlay.filter(|o| !o.text.is_empty());
    }

    /// Read by the client to resolve the bottom-row precedence at click
    /// time (`instances::bottom_bar_claim`): the composition lives here,
    /// while the search and confirmation states it competes with live on
    /// the client.
    #[must_use]
    pub const fn preedit_active(&self) -> bool {
        self.preedit.is_some()
    }

    pub fn set_search_overlay(&mut self, overlay: Option<SearchOverlay>) {
        self.search = overlay;
    }

    pub fn set_confirm_overlay(&mut self, overlay: Option<ConfirmOverlay>) {
        self.confirm = overlay;
    }

    pub fn set_link_preview_overlay(&mut self, overlay: Option<LinkPreviewOverlay>) {
        self.link_preview = overlay;
    }

    /// Submits one frame following `docs/reference/protocols/kitty-graphics.md` "Z-ordering".
    ///
    /// The cursor is composited into cell layers so opaque `z >= 0` images cover it.
    /// `images` supplies pixels when a full atlas recycles and live images must re-upload.
    pub fn render(
        &mut self,
        screen: &ScreenBuffer,
        dirty_images: &[ImageId],
        images: &impl ImageSource,
        placements: &[ClientPlacement],
        virtual_placements: &[VirtualPlacement],
        viewport: u32,
        viewport_max: u32,
    ) -> Result<(), SurfaceError> {
        self.populate_glyphs(screen);
        // After the grid walk, not at the `set_*_overlay` call: a walk
        // that filled the atlas recycles the whole sheet, dropping slots
        // handed out earlier, and its retry re-populates grid glyphs
        // only, which would leave a bar's label or truncation mark absent.
        self.populate_overlay_glyphs();
        if self.glyphs.atlas_was_reset() {
            // Overlay priming filled the sheet, invalidating earlier grid slots.
            // Walking again re-allocates without re-rasterizing before overlay re-priming.
            self.populate_glyphs(screen);
            self.populate_overlay_glyphs();
            if self.glyphs.atlas_was_reset() {
                self.glyph_repaint_pending = true;
            }
        }
        self.upload_dirty_images(dirty_images, images, placements, virtual_placements);

        let metrics = self.glyphs.cell_metrics();
        self.build_cell_instances(screen, metrics, viewport, viewport_max);
        if self.glyph_repaint_pending {
            // Rows painted around a missing glyph must not outlive the
            // repaint the client is about to request.
            self.cells.invalidate();
        }
        self.build_placement_instances(screen, placements, metrics, viewport);
        self.build_placeholder_instances(screen, virtual_placements, metrics);
        self.clip_images_out_of_chrome(screen.rows(), metrics);
        let counts = self.upload_frame_buffers(screen, metrics);
        self.last_counts = Some(counts);

        let (frame, view) = match &self.target {
            Target::Surface { surface, .. } => {
                let frame = match surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                    wgpu::CurrentSurfaceTexture::Timeout
                    | wgpu::CurrentSurfaceTexture::Occluded => {
                        return Err(SurfaceError::Timeout);
                    }
                    wgpu::CurrentSurfaceTexture::Outdated => return Err(SurfaceError::Outdated),
                    wgpu::CurrentSurfaceTexture::Lost => return Err(SurfaceError::Lost),
                    wgpu::CurrentSurfaceTexture::Validation => {
                        return Err(SurfaceError::Other("validation".to_owned()));
                    }
                };
                let view = frame.texture.create_view(&TextureViewDescriptor::default());
                (Some(frame), view)
            }
            Target::Offscreen(offscreen) => (None, offscreen.view.clone()),
        };
        let cell_target = self.post.as_ref().map_or(&view, PostStage::target_view);
        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("felis frame encoder"),
            });
        self.uploader.encode(&self.device, &mut encoder);
        self.encode_cell_pass(&mut encoder, cell_target, &counts);
        self.encode_post_pass(&mut encoder, &view, screen, metrics, viewport);
        let submission = self.queue.submit(std::iter::once(encoder.finish()));
        self.mark_instances_submitted(&submission);
        match (&mut self.target, frame) {
            (_, Some(frame)) => self.queue.present(frame),
            (Target::Offscreen(offscreen), None) => offscreen.rendered = true,
            (Target::Surface { .. }, None) => {}
        }
        Ok(())
    }

    /// Retries once if a mid-walk reset dropped slots handed out earlier.
    ///
    /// The second pass re-allocates without re-rasterizing because the CPU bitmap cache
    /// survives. A second reset means distinct screen glyphs exceed the whole sheet.
    fn populate_glyphs(&mut self, screen: &ScreenBuffer) {
        self.glyphs.populate_grid_with_features(
            screen,
            &mut self.shaper,
            &self.font_features,
            &mut self.uploader,
            &mut self.shape_frame,
        );
        if !self.glyphs.atlas_was_reset() {
            return;
        }
        self.glyphs.populate_grid_with_features(
            screen,
            &mut self.shaper,
            &self.font_features,
            &mut self.uploader,
            &mut self.shape_frame,
        );
        if self.glyphs.atlas_was_reset() {
            self.glyph_repaint_pending = true;
            if !self.glyph_atlas_thrash_logged {
                // Once per renderer; a per-frame warning at vsync would
                // bury the log.
                self.glyph_atlas_thrash_logged = true;
                tracing::warn!(
                    "glyph atlas filled twice in one frame; some cells paint blank until the screen holds fewer distinct glyphs"
                );
            }
        }
    }

    /// The glyphs the chrome bars and the preedit panel need this frame,
    /// including [`BAR_ELLIPSIS`], which the preview bar synthesizes at
    /// paint time and no overlay's own text carries.
    fn populate_overlay_glyphs(&mut self) {
        let texts = [
            self.preedit.as_ref().map(|o| o.text.as_str()),
            self.search.as_ref().map(|o| o.label.as_str()),
            self.confirm.as_ref().map(|o| o.label.as_str()),
            self.link_preview.as_ref().map(|o| o.text.as_str()),
        ];
        self.glyphs.trim_overlay_clusters();
        for text in texts.into_iter().flatten().filter(|t| !t.is_empty()) {
            self.glyphs
                .populate_overlay_text(text, &mut self.shaper, &mut self.uploader);
        }
        if self
            .link_preview
            .as_ref()
            .is_some_and(|o| !o.text.is_empty())
        {
            self.glyphs
                .populate_chars([BAR_ELLIPSIS], &mut self.uploader);
        }
    }

    /// Clears on read. Rendering is damage-driven, so without a repaint
    /// request the blanks would sit until something else dirtied those
    /// rows.
    pub const fn take_glyph_repaint_request(&mut self) -> bool {
        std::mem::replace(&mut self.glyph_repaint_pending, false)
    }

    /// A full sheet is recycled wholesale (the shelf packer does not
    /// defragment), which drops every slot; the live set is re-uploaded
    /// here rather than left to producers, which never re-send an image
    /// they already delivered.
    fn upload_dirty_images(
        &mut self,
        dirty_images: &[ImageId],
        images: &impl ImageSource,
        placements: &[ClientPlacement],
        virtual_placements: &[VirtualPlacement],
    ) {
        for id in dirty_images {
            if self.try_upload_image(*id, images) == UploadOutcome::SheetFull {
                self.images.reset();
                self.reupload_live_images(dirty_images, images, placements, virtual_placements);
                return;
            }
        }
    }

    /// At most once per frame: a second full sheet means the live set
    /// does not fit even an empty atlas.
    fn reupload_live_images(
        &mut self,
        dirty_images: &[ImageId],
        images: &impl ImageSource,
        placements: &[ClientPlacement],
        virtual_placements: &[VirtualPlacement],
    ) {
        let mut skipped = 0usize;
        for id in live_image_ids(dirty_images, placements, virtual_placements) {
            if self.try_upload_image(id, images) == UploadOutcome::SheetFull {
                skipped += 1;
            }
        }
        if skipped > 0 && !self.image_atlas_overflow_logged {
            // Once per renderer; a per-frame warning at vsync would bury
            // the log.
            self.image_atlas_overflow_logged = true;
            tracing::warn!(
                skipped,
                side = self.images.atlas_side(),
                "images do not fit the atlas even when empty; their placements paint nothing"
            );
        }
    }

    /// `Missing` is not an atlas condition and must not trigger the
    /// recovery path.
    fn try_upload_image(&mut self, id: ImageId, images: &impl ImageSource) -> UploadOutcome {
        let Some(data) = images.image_data(id) else {
            return UploadOutcome::Missing;
        };
        if self
            .images
            .upload(
                &self.device,
                &mut self.uploader,
                id,
                data.width,
                data.height,
                data.format,
                data.pixels,
            )
            .is_some()
        {
            UploadOutcome::Uploaded
        } else {
            UploadOutcome::SheetFull
        }
    }

    fn build_cell_instances(
        &mut self,
        screen: &ScreenBuffer,
        metrics: CellMetrics,
        viewport: u32,
        viewport_max: u32,
    ) {
        // Keep in lockstep with `extend_search_instances` /
        // `extend_confirm_instances` / `extend_link_preview_instances`,
        // which draw under the same `!label.is_empty()` condition; the
        // reserved row must not double through the overlay.
        let search_active = self.search.as_ref().is_some_and(|o| !o.label.is_empty());
        let confirm_active = self.confirm.as_ref().is_some_and(|o| !o.label.is_empty());
        let link_preview_active = self
            .link_preview
            .as_ref()
            .is_some_and(|o| !o.text.is_empty());
        let preedit_active = self.preedit.as_ref().is_some_and(|o| !o.text.is_empty());
        let chrome = ChromeBar::from_flags(search_active, confirm_active);
        let bottom_bar = bottom_bar_claim(chrome, preedit_active, link_preview_active);
        let reserved_bottom_rows = bottom_bar.reserved_rows();
        self.chrome_rows = reserved_bottom_rows;
        // Reserved so the overlay's opaque bg hides the underlying glyph;
        // the fg pass cannot occlude across the two passes.
        let preedit_cols = self
            .preedit
            .as_ref()
            .and_then(|o| preedit_covered_cols(o, screen.cols()));
        let painter = CellPainter::new(
            screen,
            metrics,
            &self.resolved_theme,
            &self.glyphs,
            &self.shape_frame,
        )
        .with_cursor_visible(self.window_focused && self.cursor_blink_visible)
        .with_selection(self.selection)
        .with_viewport(viewport, viewport_max)
        .with_reserved_bottom_rows(reserved_bottom_rows)
        .with_preedit(preedit_cols)
        .with_reverse_video(self.reverse_video)
        .resolved();
        let key = FrameKey {
            rows: screen.rows(),
            cols: screen.cols(),
            metrics: [metrics.width, metrics.height, metrics.ascent],
            viewport,
            reserved_bottom_rows,
            preedit: preedit_cols,
            selection: self.selection,
            reverse_video: self.reverse_video,
            shaped: !self.font_features.is_empty() || screen.cluster_count() > 0,
            atlas_generation: self.glyphs.atlas_resets(),
        };
        // Before any overlay is appended, so only producer-controlled
        // quads are cut: a glyph anchored above the reserved row can
        // still reach into it (a tall bitmap, an OSC 66 run scaled
        // across rows), and it rides the same pass as the bar's text.
        let clip_max_y = (reserved_bottom_rows > 0).then(|| {
            f32::from(screen.rows().saturating_sub(reserved_bottom_rows)) * metrics.height as f32
        });
        let dirty = screen.damage().dirty_rows();
        let out = self.cells.paint(&painter, key, dirty, clip_max_y);
        if let Some(overlay) = &self.preedit {
            extend_preedit_instances(
                overlay,
                metrics,
                screen.rows(),
                screen.cols(),
                &self.glyphs,
                out,
            );
        }
        if let Some(overlay) = &self.search {
            // Hit highlights regardless; the bar only when the search
            // holds the row. A chord can arm a confirmation while a
            // search is composing, and two bars in one row interleave
            // their glyphs rather than one hiding the other.
            extend_search_instances(
                overlay,
                metrics,
                screen.rows(),
                screen.cols(),
                matches!(chrome, ChromeBar::Search),
                &self.glyphs,
                out,
            );
        }
        // After the search bar: an open question wins the bottom row.
        if let Some(overlay) = &self.confirm {
            extend_confirm_instances(
                overlay,
                metrics,
                screen.rows(),
                screen.cols(),
                &self.glyphs,
                out,
            );
        }
        // Last, and only when neither of the above claimed the row
        // (`bottom_bar_claim` carries the precedence and its test).
        if bottom_bar.draw_link_preview()
            && let Some(overlay) = &self.link_preview
        {
            extend_link_preview_instances(
                overlay,
                metrics,
                screen.rows(),
                screen.cols(),
                &self.glyphs,
                out,
            );
        }
    }

    /// Trims images out of rows occupied by client chrome.
    ///
    /// Because `z >= 0` images draw after cells (`docs/reference/protocols/kitty-graphics.md`),
    /// bottom-row placements would otherwise cover chrome bars. Clipping quads rather than
    /// reordering passes preserves relative z-order against terminal content.
    fn clip_images_out_of_chrome(&mut self, screen_rows: u16, metrics: CellMetrics) {
        if self.chrome_rows == 0 {
            return;
        }
        let max_y = f32::from(screen_rows.saturating_sub(self.chrome_rows)) * metrics.height as f32;
        for quads in [
            &mut self.image_scratch_under_bg,
            &mut self.image_scratch_under_text,
            &mut self.image_scratch_above,
        ] {
            quads.retain_mut(|quad| {
                clip_img_quad_above(*quad, max_y).is_some_and(|clipped| {
                    *quad = clipped;
                    true
                })
            });
        }
    }

    fn build_placement_instances(
        &mut self,
        screen: &ScreenBuffer,
        placements: &[ClientPlacement],
        metrics: CellMetrics,
        viewport: u32,
    ) {
        self.image_scratch_under_bg.clear();
        self.image_scratch_under_text.clear();
        self.image_scratch_above.clear();
        let mut sorted = placements.to_vec();
        sorted.sort_by_key(|p| p.z_index);
        let cw = metrics.width as f32;
        let ch = metrics.height as f32;
        let rows = u32::from(screen.rows());
        for p in &sorted {
            let Some(slot) = self.images.slot(p.image_id) else {
                continue;
            };
            if slot.width == 0 || slot.height == 0 {
                continue;
            }
            // Mirrors `ScreenBuffer::cell_at_viewport`: live row L paints
            // at visible row L + viewport.
            let top_row = i64::from(p.anchor_row) - 1 + i64::from(viewport);
            if top_row >= i64::from(rows) {
                continue;
            }
            // Same ceil-div `quad_for_placement` resolves, so the cull
            // bound matches what draws.
            let quad_rows = if p.rows == 0 {
                let ch_px = (ch.max(1.0)) as u32;
                i64::from(slot.height.div_ceil(ch_px))
            } else {
                i64::from(p.rows)
            };
            if top_row + quad_rows <= 0 {
                continue;
            }
            let Some(instance) = quad_for_placement(p, slot, cw, ch, viewport) else {
                continue;
            };
            match z_layer(p.z_index) {
                ZLayer::UnderBg => self.image_scratch_under_bg.push(instance),
                ZLayer::UnderText => self.image_scratch_under_text.push(instance),
                ZLayer::AboveText => self.image_scratch_above.push(instance),
            }
        }
    }

    /// Kitty Unicode-placeholder tiles (`U=1`). Placeholder cells are
    /// screen content already composed into the viewport, so unlike
    /// anchored placements a tile takes no viewport offset.
    fn build_placeholder_instances(
        &mut self,
        screen: &ScreenBuffer,
        virtual_placements: &[VirtualPlacement],
        metrics: CellMetrics,
    ) {
        if virtual_placements.is_empty() {
            return;
        }
        let cw = metrics.width as f32;
        let ch = metrics.height as f32;
        self.vp_map.clear();
        self.vp_map
            .extend(virtual_placements.iter().map(|v| (v.image_id, *v)));
        let mut ph_cells = 0u32;
        let mut ph_tiles = 0u32;
        let mut ph_no_slot = 0u32;
        let mut ph_no_vp = 0u32;
        for pc in screen.placeholder_cells() {
            ph_cells += 1;
            let Some(&vp) = self.vp_map.get(&pc.image_id) else {
                ph_no_vp += 1;
                continue;
            };
            let Some(slot) = self.images.slot(pc.image_id) else {
                ph_no_slot += 1;
                continue;
            };
            let (eff_cols, eff_rows) =
                placeholder_grid_extent(vp.cols, vp.rows, slot.width, slot.height, cw, ch);
            let Some(instance) = placeholder_tile_quad(
                slot,
                cw,
                ch,
                PlaceholderTile {
                    screen_row: pc.row,
                    screen_col: pc.col,
                    tile_row: pc.tile_row,
                    tile_col: pc.tile_col,
                    total_cols: eff_cols,
                    total_rows: eff_rows,
                },
            ) else {
                continue;
            };
            ph_tiles += 1;
            match z_layer(vp.z_index) {
                ZLayer::UnderBg => self.image_scratch_under_bg.push(instance),
                ZLayer::UnderText => self.image_scratch_under_text.push(instance),
                ZLayer::AboveText => self.image_scratch_above.push(instance),
            }
        }
        tracing::debug!(
            target: "felis::placeholder",
            vps = virtual_placements.len(),
            ph_cells,
            ph_tiles,
            ph_no_slot,
            ph_no_vp,
            "placeholder pass",
        );
    }

    /// The letterbox origin rides the viewport uniform so every instance
    /// shifts as one block in the shader.
    fn upload_frame_buffers(&mut self, screen: &ScreenBuffer, metrics: CellMetrics) -> DrawCounts {
        self.content_origin_px = letterbox_origin(
            screen.rows(),
            screen.cols(),
            metrics.width as f32,
            metrics.height as f32,
            self.width as f32,
            self.height as f32,
        );
        let viewport_uniform =
            ViewportUniform::with_origin(self.width, self.height, self.content_origin_px);
        if viewport_uniform != self.viewport_written {
            self.viewport.write(
                &self.device,
                &self.queue,
                bytemuck::bytes_of(&viewport_uniform),
            );
            self.viewport_written = viewport_uniform;
        }
        let (device, bgl) = (&self.device, &self.viewport_bgl);
        self.viewport
            .prepare_bind_group(|buf| viewport_bind_group(device, bgl, buf));
        let cells = &self.cells.out;
        let counts = DrawCounts {
            bg: cells.bg.len() as u32,
            fg: cells.fg.len() as u32,
            deco: cells.deco.len() as u32,
            img_under_bg: self.image_scratch_under_bg.len() as u32,
            img_under_text: self.image_scratch_under_text.len() as u32,
            img_above: self.image_scratch_above.len() as u32,
        };
        self.bg_buffer.upload(
            &self.device,
            &self.queue,
            cast_slice(&cells.bg),
            Some(self.cells.updates(size_of::<BgInstance>(), 0)),
        );
        self.fg_buffer.upload(
            &self.device,
            &self.queue,
            cast_slice(&cells.fg),
            Some(self.cells.updates(size_of::<FgInstance>(), 1)),
        );
        self.deco_buffer.upload(
            &self.device,
            &self.queue,
            cast_slice(&cells.deco),
            Some(self.cells.updates(size_of::<DecorationInstance>(), 2)),
        );
        if !self.image_scratch_under_bg.is_empty() {
            self.image_under_bg_buffer.upload(
                &self.device,
                &self.queue,
                cast_slice(&self.image_scratch_under_bg),
                None,
            );
        }
        if !self.image_scratch_under_text.is_empty() {
            self.image_under_text_buffer.upload(
                &self.device,
                &self.queue,
                cast_slice(&self.image_scratch_under_text),
                None,
            );
        }
        if !self.image_scratch_above.is_empty() {
            self.image_above_buffer.upload(
                &self.device,
                &self.queue,
                cast_slice(&self.image_scratch_above),
                None,
            );
        }
        counts
    }

    fn mark_instances_submitted(&mut self, submission: &wgpu::SubmissionIndex) {
        self.bg_buffer.submitted(submission);
        self.fg_buffer.submitted(submission);
        self.deco_buffer.submitted(submission);
        self.image_under_bg_buffer.submitted(submission);
        self.image_under_text_buffer.submitted(submission);
        self.image_above_buffer.submitted(submission);
        self.viewport.submitted(submission);
        if let Some(post) = self.post.as_mut() {
            post.submitted(submission);
        }
    }

    fn encode_cell_pass(
        &self,
        encoder: &mut CommandEncoder,
        cell_target: &TextureView,
        counts: &DrawCounts,
    ) {
        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("felis cell pass"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: cell_target,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(self.clear_color),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let Some(viewport_bg) = self.viewport.bind_group() else {
            return;
        };
        pass.set_bind_group(0, viewport_bg, &[]);
        // Images under the cell backgrounds. The image bind group is
        // `None` until the first upload, so the lazy atlas folds into
        // the draw guard.
        if let (Some(buf), Some(bg), true) = (
            self.image_under_bg_buffer.buffer(),
            self.images.bind_group(),
            counts.img_under_bg > 0,
        ) {
            pass.set_pipeline(&self.image_pipeline);
            pass.set_bind_group(1, bg, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..4, 0..counts.img_under_bg);
        }
        if let (Some(buf), true) = (self.bg_buffer.buffer(), counts.bg > 0) {
            pass.set_pipeline(&self.bg_pipeline);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..4, 0..counts.bg);
        }
        // Above the backgrounds, below text: the common `z=-1` tier
        // (yazi previews).
        if let (Some(buf), Some(bg), true) = (
            self.image_under_text_buffer.buffer(),
            self.images.bind_group(),
            counts.img_under_text > 0,
        ) {
            pass.set_pipeline(&self.image_pipeline);
            pass.set_bind_group(1, bg, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..4, 0..counts.img_under_text);
        }
        if let (Some(buf), true) = (self.fg_buffer.buffer(), counts.fg > 0) {
            pass.set_pipeline(&self.fg_pipeline);
            pass.set_bind_group(1, self.glyphs.bind_group(), &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..4, 0..counts.fg);
        }
        // After the glyphs, so a strikethrough sits over the ink and an
        // underline crosses descenders.
        if let (Some(buf), true) = (self.deco_buffer.buffer(), counts.deco > 0) {
            pass.set_pipeline(&self.deco_pipeline);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..4, 0..counts.deco);
        }
        if let (Some(buf), Some(bg), true) = (
            self.image_above_buffer.buffer(),
            self.images.bind_group(),
            counts.img_above > 0,
        ) {
            pass.set_pipeline(&self.image_pipeline);
            pass.set_bind_group(1, bg, &[]);
            pass.set_vertex_buffer(0, buf.slice(..));
            pass.draw(0..4, 0..counts.img_above);
        }
    }

    /// The cursor gate mirrors `CellPainter`'s, so a shader is told the
    /// cursor is visible exactly when one was painted.
    fn encode_post_pass(
        &mut self,
        encoder: &mut CommandEncoder,
        view: &TextureView,
        screen: &ScreenBuffer,
        metrics: CellMetrics,
        viewport: u32,
    ) {
        let theme = &self.resolved_theme;
        if let Some(post) = self.post.as_mut() {
            let cursor = screen.cursor();
            let cursor_visible = cursor.visible
                && self.window_focused
                && self.cursor_blink_visible
                && viewport == 0
                && cursor.row < screen.rows()
                && cursor.col < screen.cols();
            post.encode(
                encoder,
                &self.device,
                &self.queue,
                view,
                PostFrame {
                    cell_size_px: [metrics.width as f32, metrics.height as f32],
                    theme,
                    // The cell pass resolves the cursor against the
                    // cell's fg under reverse video; the contract
                    // reports the theme-level color.
                    cursor_color: theme.cursor.unwrap_or(theme.fg),
                    cursor_style: screen.cursor_style(),
                    cursor_visible,
                    focused: self.window_focused,
                },
                self.trail_state,
                self.mouse_state,
            );
        }
    }

    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

fn viewport_bind_group(
    device: &Device,
    layout: &wgpu::BindGroupLayout,
    buffer: &Buffer,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("felis viewport bg"),
        layout,
        entries: &[BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    })
}

/// Kitty draws images with `z < INT32_MIN / 2` under the cell
/// backgrounds and `[INT32_MIN / 2, 0)` above them but below text
/// (`docs/reference/protocols/kitty-graphics.md` "Z-ordering").
/// Collapsing all negative z under the backgrounds lets opaque
/// backgrounds paint over `z=-1` placements (yazi's previews).
const Z_UNDER_BG_MAX: i32 = i32::MIN / 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ZLayer {
    UnderBg,
    UnderText,
    AboveText,
}

const fn z_layer(z: i32) -> ZLayer {
    if z < Z_UNDER_BG_MAX {
        ZLayer::UnderBg
    } else if z < 0 {
        ZLayer::UnderText
    } else {
        ZLayer::AboveText
    }
}

/// The anchor is 1-based (`anchor_row - 1 + viewport`, `anchor_col - 1`),
/// shifted by `viewport` so a placement tracks its text while browsing
/// scrollback. `None` for a degenerate placement.
fn quad_for_placement(
    p: &ClientPlacement,
    slot: ImageSlot,
    cw: f32,
    ch: f32,
    viewport: u32,
) -> Option<ImgInstance> {
    let SourceRect {
        x: sx,
        y: sy,
        width: sw,
        height: sh,
    } = clip_source(slot.width, slot.height, p.source)?;
    let cols = if p.cols == 0 {
        ((sw + (cw.max(1.0) as u32).saturating_sub(1)) / cw.max(1.0) as u32).max(1) as u16
    } else {
        p.cols
    };
    let rows = if p.rows == 0 {
        ((sh + (ch.max(1.0) as u32).saturating_sub(1)) / ch.max(1.0) as u32).max(1) as u16
    } else {
        p.rows
    };
    if cols == 0 || rows == 0 {
        return None;
    }
    // Signed: a scrollback-anchored placement (`anchor_row <= 0`) has a
    // negative origin until the viewport lifts it into view, and the GPU
    // clips the rest.
    let origin_row = i64::from(p.anchor_row) - 1 + i64::from(viewport);
    let origin_px = [
        f32::from(p.anchor_col.saturating_sub(1)) * cw,
        origin_row as f32 * ch,
    ];
    // Natural-sized boxes matching `ceil(px / cell)` draw at native pixels to avoid
    // non-integer resampling seams on high-contrast edges. Explicit `c=` or `r=`
    // dimensions still scale to the target box.
    let cw_px = cw.max(1.0) as u32;
    let ch_px = ch.max(1.0) as u32;
    let width_px = if u32::from(cols) == sw.div_ceil(cw_px) {
        sw as f32
    } else {
        f32::from(cols) * cw
    };
    let height_px = if u32::from(rows) == sh.div_ceil(ch_px) {
        sh as f32
    } else {
        f32::from(rows) * ch
    };
    let size_px = [width_px, height_px];
    let u0 = slot.uv_min[0];
    let v0 = slot.uv_min[1];
    let du = slot.uv_max[0] - u0;
    let dv = slot.uv_max[1] - v0;
    let inv_w = 1.0 / slot.width as f32;
    let inv_h = 1.0 / slot.height as f32;
    let uv_min = [
        du.mul_add(sx as f32 * inv_w, u0),
        dv.mul_add(sy as f32 * inv_h, v0),
    ];
    let uv_max = [
        du.mul_add((sx + sw) as f32 * inv_w, u0),
        dv.mul_add((sy + sh) as f32 * inv_h, v0),
    ];
    Some(ImgInstance {
        origin_px,
        size_px,
        uv_min,
        uv_max,
        opacity: 1.0,
        _pad: [0.0; 3],
    })
}

/// The producer's `c=`/`r=` when sent (presenterm), else the natural
/// `ceil(image_px / cell_px)`: yazi omits them and paints exactly that
/// screen, and a zero extent would skip every tile.
fn placeholder_grid_extent(
    vp_cols: u16,
    vp_rows: u16,
    img_w: u32,
    img_h: u32,
    cw: f32,
    ch: f32,
) -> (u16, u16) {
    let natural = |px: u32, cell: f32| {
        u16::try_from(px.div_ceil((cell.max(1.0)) as u32))
            .unwrap_or(u16::MAX)
            .max(1)
    };
    let cols = if vp_cols > 0 {
        vp_cols
    } else {
        natural(img_w, cw)
    };
    let rows = if vp_rows > 0 {
        vp_rows
    } else {
        natural(img_h, ch)
    };
    (cols, rows)
}

#[derive(Clone, Copy)]
struct PlaceholderTile {
    /// 0-based, unlike `ClientPlacement`'s anchor.
    screen_row: u16,
    screen_col: u16,
    /// 0-based.
    tile_row: u32,
    tile_col: u32,
    total_cols: u16,
    total_rows: u16,
}

fn placeholder_tile_quad(
    slot: ImageSlot,
    cw: f32,
    ch: f32,
    at: PlaceholderTile,
) -> Option<ImgInstance> {
    let cols = u32::from(at.total_cols);
    let rows = u32::from(at.total_rows);
    if cols == 0 || rows == 0 || at.tile_col >= cols || at.tile_row >= rows {
        return None;
    }
    if slot.width == 0 || slot.height == 0 {
        return None;
    }
    // Consecutive cut points, so rounding never leaves a one-pixel seam
    // between adjacent tiles.
    let sx = at.tile_col * slot.width / cols;
    let sw = (at.tile_col + 1) * slot.width / cols - sx;
    let sy = at.tile_row * slot.height / rows;
    let sh = (at.tile_row + 1) * slot.height / rows - sy;
    if sw == 0 || sh == 0 {
        return None;
    }
    let p = ClientPlacement {
        image_id: ImageId(0),
        placement_id: None,
        anchor_row: i32::from(at.screen_row) + 1,
        anchor_col: at.screen_col.saturating_add(1),
        cols: 1,
        rows: 1,
        source: Some(SourceRect {
            x: sx,
            y: sy,
            width: sw,
            height: sh,
        }),
        z_index: 0,
    };
    quad_for_placement(&p, slot, cw, ch, 0)
}

/// Under DECSCNM the letterbox takes the theme foreground too, or a
/// padding strip frames the reversed screen in a visible border. The
/// alpha stays the background's: `window.opacity` is a window property,
/// not part of the color `?5` swaps.
fn clear_color_for_theme(theme: &ResolvedTheme, reverse_video: bool) -> Color {
    let rgb = if reverse_video { theme.fg } else { theme.bg };
    Color {
        r: f64::from(rgb[0]),
        g: f64::from(rgb[1]),
        b: f64::from(rgb[2]),
        a: f64::from(theme.bg[3]),
    }
}

/// felis paints the background straight-alpha (the bg pass runs with
/// blending off), so a `PostMultiplied` surface takes the theme as is.
/// Only `bg` is touched: fg, cursor, and explicit cell backgrounds are
/// alpha 1.0, for which premultiply is identity.
#[must_use]
fn premultiplied_bg(mut theme: Theme, premultiply: bool) -> Theme {
    if premultiply {
        let a = theme.bg[3];
        theme.bg[0] *= a;
        theme.bg[1] *= a;
        theme.bg[2] *= a;
    }
    theme
}

/// Computes letterbox origin for screen mirroring (`docs/explanation/architecture/session-lifecycle.md`).
///
/// Surpluses of at least one cell center the content on whole pixels; sub-cell remainders
/// anchor top-left. Deficits anchor bottom-left so the GPU clips while keeping the prompt visible.
fn letterbox_origin(rows: u16, cols: u16, cw: f32, ch: f32, win_w: f32, win_h: f32) -> [f32; 2] {
    let surplus_x = f32::from(cols).mul_add(-cw, win_w);
    let surplus_y = f32::from(rows).mul_add(-ch, win_h);
    let x = if surplus_x >= cw {
        (surplus_x / 2.0).floor()
    } else {
        0.0
    };
    let y = if surplus_y >= ch {
        (surplus_y / 2.0).floor()
    } else if surplus_y < 0.0 {
        surplus_y
    } else {
        0.0
    };
    [x, y]
}

/// Selects `(mode, premultiply)` for surface composition.
///
/// Translucent requests prefer `PostMultiplied`, then `PreMultiplied` or `Inherit`.
/// On macOS, `CoreAnimation` composites premultiplied unconditionally even when
/// `PostMultiplied` is selected, so macOS premultiplies in both modes.
#[must_use]
fn select_alpha_mode(
    available: &[wgpu::CompositeAlphaMode],
    opacity: f32,
) -> (wgpu::CompositeAlphaMode, bool) {
    use wgpu::CompositeAlphaMode::{Inherit, PostMultiplied, PreMultiplied};

    let opaque_default = available
        .first()
        .copied()
        .unwrap_or(wgpu::CompositeAlphaMode::Opaque);
    if opacity >= 1.0 {
        return (opaque_default, false);
    }

    let has = |m: wgpu::CompositeAlphaMode| available.contains(&m);
    if has(PostMultiplied) {
        (PostMultiplied, cfg!(target_os = "macos"))
    } else if has(PreMultiplied) {
        (PreMultiplied, true)
    } else if has(Inherit) {
        (Inherit, true)
    } else {
        tracing::warn!(
            ?available,
            "no transparent composite-alpha mode; rendering opaque despite opacity < 1.0"
        );
        (opaque_default, false)
    }
}

struct PipelineSpec<'a> {
    layout: &'a PipelineLayout,
    vs_entry: &'a str,
    fs_entry: &'a str,
    buffer_layout: VertexBufferLayout<'a>,
    blend: Option<BlendState>,
}

fn build_pipeline(
    device: &Device,
    module: &wgpu::ShaderModule,
    format: TextureFormat,
    spec: PipelineSpec<'_>,
) -> RenderPipeline {
    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("felis cell pipeline"),
        layout: Some(spec.layout),
        vertex: VertexState {
            module,
            entry_point: Some(spec.vs_entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[Some(spec.buffer_layout)],
        },
        fragment: Some(FragmentState {
            module,
            entry_point: Some(spec.fs_entry),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(ColorTargetState {
                format,
                blend: spec.blend,
                write_mask: ColorWrites::ALL,
            })],
        }),
        primitive: PrimitiveState {
            // `cull_mode: None` sidesteps the strip winding after the
            // flip-Y projection; 2D has no overdraw to save.
            topology: PrimitiveTopology::TriangleStrip,
            strip_index_format: None,
            front_face: FrontFace::Ccw,
            cull_mode: None,
            unclipped_depth: false,
            polygon_mode: PolygonMode::Fill,
            conservative: false,
        },
        depth_stencil: None,
        multisample: MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// `(x, y, width, height)` in pixels of the cursor the cell pass paints
/// over the columns `span` covers.
fn cursor_rect_px(
    metrics: CellMetrics,
    origin_px: [f32; 2],
    row: u16,
    (first, last): (u16, u16),
    style: CursorStyle,
) -> [f32; 4] {
    let (cw, ch) = (metrics.width as f32, metrics.height as f32);
    let x = f32::from(first).mul_add(cw, origin_px[0]);
    let y = f32::from(row).mul_add(ch, origin_px[1]);
    let w = f32::from(last - first + 1) * cw;
    match style {
        CursorStyle::Block => [x, y, w, ch],
        CursorStyle::Underline => [x, y + ch - CURSOR_MARKER_PX, w, CURSOR_MARKER_PX],
        CursorStyle::Bar => [x, y, CURSOR_MARKER_PX, ch],
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use crate::palette::{XTERM_PALETTE, srgb_to_linear_rgba};

    use super::*;

    #[test]
    fn cursor_rect_spans_both_columns_of_a_wide_character() {
        let metrics = CellMetrics {
            width: 10,
            height: 20,
            ascent: 16,
        };
        let span = (3, 4);
        assert_eq!(
            cursor_rect_px(metrics, [5.0, 0.0], 1, span, CursorStyle::Block),
            [35.0, 20.0, 20.0, 20.0]
        );
        assert_eq!(
            cursor_rect_px(metrics, [5.0, 0.0], 1, span, CursorStyle::Underline),
            [35.0, 40.0 - CURSOR_MARKER_PX, 20.0, CURSOR_MARKER_PX]
        );
        assert_eq!(
            cursor_rect_px(metrics, [5.0, 0.0], 1, span, CursorStyle::Bar),
            [35.0, 20.0, CURSOR_MARKER_PX, 20.0]
        );
    }

    /// REQ-705 / REQ-1005: sides are `min(ceiling, adapter max 2D)`; a
    /// downlevel (2048) adapter clamps the image sheet and leaves the
    /// glyph sheet alone.
    #[test]
    fn atlas_sides_clamp_to_the_adapter_limit() {
        assert_eq!(atlas_side(MAX_ATLAS_SIDE, 8192).get(), 8192);
        assert_eq!(atlas_side(GLYPH_ATLAS_SIDE, 8192).get(), 2048);

        assert_eq!(atlas_side(MAX_ATLAS_SIDE, 2048).get(), 2048);
        assert_eq!(atlas_side(GLYPH_ATLAS_SIDE, 2048).get(), 2048);

        assert_eq!(atlas_side(MAX_ATLAS_SIDE, 16384).get(), 8192);
    }

    /// REQ-1005 / REQ-1006: the device asks for the adapter's own limits,
    /// so a downlevel (2048) adapter gets one and the atlas clamp runs
    /// instead of `request_device` refusing the 8192 default.
    #[test]
    fn a_downlevel_adapter_gets_a_device_and_clamps_the_atlas() {
        let mut desc = InstanceDescriptor {
            backends: Backends::NOOP,
            ..InstanceDescriptor::new_without_display_handle()
        };
        desc.backend_options.noop = wgpu::NoopBackendOptions {
            enable: true,
            limits: Some(wgpu::Limits::downlevel_defaults()),
            ..wgpu::NoopBackendOptions::default()
        };
        let instance = Instance::new(desc);
        warmup::block_on(async {
            let adapter = request_adapter(&instance, None)
                .await
                .expect("noop adapter");
            assert_eq!(adapter.limits().max_texture_dimension_2d, 2048);
            let (device, _queue) = request_device(&adapter)
                .await
                .expect("a device at the adapter's own limits");
            let max_2d = device.limits().max_texture_dimension_2d;
            assert_eq!(atlas_side(MAX_ATLAS_SIDE, max_2d).get(), 2048);
        });
    }

    /// A downlevel adapter claiming nothing useful still gets a sheet
    /// the packer can shelve into.
    #[test]
    fn atlas_sides_floor_below_the_minimum() {
        assert_eq!(atlas_side(MAX_ATLAS_SIDE, 0), MIN_ATLAS_SIDE);
        assert_eq!(atlas_side(GLYPH_ATLAS_SIDE, 64), MIN_ATLAS_SIDE);
        assert_eq!(
            atlas_side(GLYPH_ATLAS_SIDE, MIN_ATLAS_SIDE.get()),
            MIN_ATLAS_SIDE
        );
    }

    fn unit_slot() -> ImageSlot {
        ImageSlot {
            uv_min: [0.0, 0.0],
            uv_max: [32.0 / 256.0, 32.0 / 256.0],
            width: 32,
            height: 32,
        }
    }

    #[test]
    fn quad_for_explicit_cols_rows_uses_them_directly() {
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: 3,
            anchor_col: 5,
            cols: 4,
            rows: 2,
            source: None,
            z_index: 0,
        };
        let inst = quad_for_placement(&p, unit_slot(), 8.0, 16.0, 0).expect("quad emits");
        assert_eq!(inst.origin_px, [4.0 * 8.0, 2.0 * 16.0]);
        assert_eq!(inst.size_px, [4.0 * 8.0, 2.0 * 16.0]);
        assert_eq!(inst.uv_min, [0.0, 0.0]);
        assert_eq!(inst.uv_max, [32.0 / 256.0, 32.0 / 256.0]);
        assert_eq!(inst.opacity, 1.0);
    }

    #[test]
    fn placeholder_tile_quad_carves_the_right_image_slice_into_one_cell() {
        let inst = placeholder_tile_quad(
            unit_slot(),
            8.0,
            16.0,
            PlaceholderTile {
                screen_row: 3,
                screen_col: 5,
                tile_row: 1,
                tile_col: 2,
                total_cols: 4,
                total_rows: 2,
            },
        )
        .expect("tile quad emits");
        assert_eq!(inst.origin_px, [5.0 * 8.0, 3.0 * 16.0]);
        assert_eq!(inst.size_px, [8.0, 16.0]);
        let span = 32.0 / 256.0;
        assert!(f32::mul_add(span, -0.5, inst.uv_min[0]).abs() < 1e-6);
        assert!(f32::mul_add(span, -0.75, inst.uv_max[0]).abs() < 1e-6);
        assert!(f32::mul_add(span, -0.5, inst.uv_min[1]).abs() < 1e-6);
        assert!((inst.uv_max[1] - span).abs() < 1e-6);
    }

    #[test]
    fn placeholder_grid_extent_uses_c_r_or_falls_back_to_natural() {
        assert_eq!(
            placeholder_grid_extent(40, 11, 353, 221, 9.0, 22.0),
            (40, 11)
        );
        assert_eq!(placeholder_grid_extent(0, 0, 353, 221, 9.0, 22.0), (40, 11));
        assert_eq!(placeholder_grid_extent(5, 0, 90, 44, 9.0, 22.0), (5, 2));
        assert_eq!(placeholder_grid_extent(0, 0, 1, 1, 9.0, 22.0), (1, 1));
    }

    #[test]
    fn placeholder_tile_quad_rejects_out_of_grid_tiles() {
        let tile = |tile_row, tile_col, total_cols| PlaceholderTile {
            screen_row: 0,
            screen_col: 0,
            tile_row,
            tile_col,
            total_cols,
            total_rows: 2,
        };
        assert!(placeholder_tile_quad(unit_slot(), 8.0, 16.0, tile(0, 4, 4)).is_none());
        assert!(placeholder_tile_quad(unit_slot(), 8.0, 16.0, tile(2, 0, 4)).is_none());
        assert!(placeholder_tile_quad(unit_slot(), 8.0, 16.0, tile(0, 0, 0)).is_none());
    }

    /// The recovery set covers every placed image, not just the dirty
    /// ids; a producer never retransmits an image it already delivered.
    #[test]
    fn recovery_set_covers_dirty_plus_every_placed_image_once() {
        let anchored = |id: u32| ClientPlacement {
            image_id: ImageId(id),
            placement_id: None,
            anchor_row: 1,
            anchor_col: 1,
            cols: 1,
            rows: 1,
            source: None,
            z_index: 0,
        };
        let virt = |id: u32| VirtualPlacement {
            image_id: ImageId(id),
            cols: 1,
            rows: 1,
            z_index: 0,
        };

        let ids = live_image_ids(
            &[ImageId(1)],
            &[anchored(2), anchored(1), anchored(2)],
            &[virt(3), virt(2)],
        );
        assert_eq!(ids, vec![ImageId(1), ImageId(2), ImageId(3)]);
    }

    #[test]
    fn quad_shifts_down_by_viewport_so_images_track_scrollback() {
        // A live row L paints at visible row L + viewport; the quad
        // follows and x is untouched.
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: 3,
            anchor_col: 5,
            cols: 4,
            rows: 2,
            source: None,
            z_index: 0,
        };
        let inst = quad_for_placement(&p, unit_slot(), 8.0, 16.0, 5).expect("quad emits");
        assert_eq!(inst.origin_px, [4.0 * 8.0, 7.0 * 16.0]);
        assert_eq!(inst.size_px, [4.0 * 8.0, 2.0 * 16.0]);
    }

    /// Scrollback-anchored placement (docs/reference/protocols/kitty-graphics.md
    /// "Scrollback-anchored placements"): browsing back exactly to its
    /// line lands the quad at the window top.
    #[test]
    fn scrollback_anchored_quad_surfaces_at_matching_viewport() {
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: -4,
            anchor_col: 1,
            cols: 4,
            rows: 2,
            source: None,
            z_index: 0,
        };
        let inst = quad_for_placement(&p, unit_slot(), 8.0, 16.0, 5).expect("quad emits");
        assert_eq!(inst.origin_px, [0.0, 0.0]);
        let inst = quad_for_placement(&p, unit_slot(), 8.0, 16.0, 4).expect("straddling quad");
        assert_eq!(inst.origin_px, [0.0, -16.0]);
    }

    #[test]
    fn z_minus_one_lands_above_the_cell_backgrounds() {
        // yazi's direct path places previews at z=-1; that tier paints
        // above the opaque cell backgrounds.
        assert_eq!(z_layer(-1), ZLayer::UnderText);
    }

    #[test]
    fn only_z_below_int_min_half_paints_under_the_backgrounds() {
        assert_eq!(z_layer(Z_UNDER_BG_MAX), ZLayer::UnderText);
        assert_eq!(z_layer(Z_UNDER_BG_MAX - 1), ZLayer::UnderBg);
        assert_eq!(z_layer(i32::MIN), ZLayer::UnderBg);
    }

    #[test]
    fn non_negative_z_paints_above_text() {
        assert_eq!(z_layer(0), ZLayer::AboveText);
        assert_eq!(z_layer(1), ZLayer::AboveText);
        assert_eq!(z_layer(i32::MAX), ZLayer::AboveText);
    }

    #[test]
    fn quad_for_zero_cols_falls_back_to_natural_cell_count() {
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: 1,
            anchor_col: 1,
            cols: 0,
            rows: 0,
            source: None,
            z_index: 0,
        };
        let inst = quad_for_placement(&p, unit_slot(), 8.0, 16.0, 0).expect("natural sizing");
        assert_eq!(inst.size_px, [4.0 * 8.0, 2.0 * 16.0]);
    }

    #[test]
    fn quad_resolved_natural_cells_render_at_native_pixels_not_stretched() {
        // Natural sizing arrives resolved to `ceil(px/cell)` cells (cols=4
        // for 30 px at 8-px cells); the quad stays the native 30 px rather
        // than the 32-px box, or the resample leaves a seam.
        let slot = ImageSlot {
            uv_min: [0.0, 0.0],
            uv_max: [30.0 / 256.0, 30.0 / 256.0],
            width: 30,
            height: 30,
        };
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: 1,
            anchor_col: 1,
            cols: 4,
            rows: 2,
            source: None,
            z_index: 0,
        };
        let inst = quad_for_placement(&p, slot, 8.0, 16.0, 0).expect("native sizing");
        assert_eq!(inst.size_px, [30.0, 30.0]);
    }

    #[test]
    fn quad_explicit_cell_box_unequal_to_native_still_scales() {
        // An explicit `c=`/`r=` naming a box other than `ceil(native)`
        // still scales.
        let slot = ImageSlot {
            uv_min: [0.0, 0.0],
            uv_max: [30.0 / 256.0, 30.0 / 256.0],
            width: 30,
            height: 30,
        };
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: 1,
            anchor_col: 1,
            cols: 10,
            rows: 5,
            source: None,
            z_index: 0,
        };
        let inst = quad_for_placement(&p, slot, 8.0, 16.0, 0).expect("explicit scale");
        assert_eq!(inst.size_px, [10.0 * 8.0, 5.0 * 16.0]);
    }

    #[test]
    fn quad_for_source_subrect_clips_uvs_within_slot() {
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: 1,
            anchor_col: 1,
            cols: 2,
            rows: 1,
            source: Some(SourceRect {
                x: 8,
                y: 0,
                width: 16,
                height: 16,
            }),
            z_index: 0,
        };
        let inst = quad_for_placement(&p, unit_slot(), 8.0, 16.0, 0).expect("sub-rect");
        let slot_u_extent = 32.0 / 256.0;
        let slot_v_extent = 32.0 / 256.0;
        assert!(f32::mul_add(slot_u_extent, -0.25, inst.uv_min[0]).abs() < 1e-6);
        assert!((inst.uv_min[1] - 0.0).abs() < 1e-6);
        assert!(f32::mul_add(slot_u_extent, -0.75, inst.uv_max[0]).abs() < 1e-6);
        assert!(f32::mul_add(slot_v_extent, -0.5, inst.uv_max[1]).abs() < 1e-6);
    }

    #[test]
    fn quad_for_source_clamps_to_image_bounds_without_crash() {
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: 1,
            anchor_col: 1,
            cols: 1,
            rows: 1,
            source: Some(SourceRect {
                x: 24,
                y: 24,
                width: 999,
                height: 999,
            }),
            z_index: 0,
        };
        let inst = quad_for_placement(&p, unit_slot(), 8.0, 16.0, 0).expect("clipped sub-rect");
        let slot_u_extent = 32.0 / 256.0;
        assert!((inst.uv_max[0] - slot_u_extent).abs() < 1e-6);
        assert!((inst.uv_max[1] - slot_u_extent).abs() < 1e-6);
    }

    #[test]
    fn quad_for_degenerate_source_returns_none() {
        let degenerate_slot = ImageSlot {
            uv_min: [0.0, 0.0],
            uv_max: [0.0, 0.0],
            width: 0,
            height: 0,
        };
        let p = ClientPlacement {
            image_id: ImageId(1),
            placement_id: None,
            anchor_row: 1,
            anchor_col: 1,
            cols: 1,
            rows: 1,
            source: None,
            z_index: 0,
        };
        assert!(quad_for_placement(&p, degenerate_slot, 8.0, 16.0, 0).is_none());
    }

    #[test]
    fn theme_from_config_default_renderer_config_is_default_theme() {
        let theme = Renderer::theme_from_config(&RendererConfig::default());
        let default = Theme::default();
        assert_eq!(theme.fg, default.fg);
        assert_eq!(theme.bg, default.bg);
        assert_eq!(theme.cursor, default.cursor);
        for slot in theme.palette {
            assert!(slot.is_none(), "default config touches no palette slot");
        }
    }

    #[test]
    fn theme_from_config_threads_fg_bg_cursor_and_palette_through() {
        let palette = BTreeMap::from([(1u8, "#ff5555".to_owned()), (12u8, "#5555ff".to_owned())]);
        let cfg = RendererConfig {
            theme_fg: Some("#cdcdcd".into()),
            theme_bg: Some("#101010".into()),
            theme_cursor: Some("#ffaa00".into()),
            theme_palette: palette,
            ..Default::default()
        };
        let theme = Renderer::theme_from_config(&cfg);
        assert_eq!(theme.fg, srgb_to_linear_rgba([0xCD, 0xCD, 0xCD], 1.0));
        assert_eq!(theme.bg, srgb_to_linear_rgba([0x10, 0x10, 0x10], 1.0));
        assert_eq!(
            theme.cursor,
            Some(srgb_to_linear_rgba([0xFF, 0xAA, 0x00], 1.0))
        );
        assert_eq!(theme.palette[1], Some([0xFF, 0x55, 0x55]));
        assert_eq!(theme.palette[12], Some([0x55, 0x55, 0xFF]));
        assert!(theme.palette[0].is_none());
        assert!(theme.palette[15].is_none());
        let _ = XTERM_PALETTE;
    }

    #[test]
    fn theme_from_config_default_is_fully_opaque() {
        let theme = Renderer::theme_from_config(&RendererConfig::default());
        assert_eq!(theme.bg[3], 1.0);
    }

    #[test]
    fn theme_from_config_carries_background_opacity_into_bg_alpha() {
        let cfg = RendererConfig {
            theme_bg: Some("#101010".into()),
            background_opacity: Some(0.8),
            ..Default::default()
        };
        let theme = Renderer::theme_from_config(&cfg);
        let opaque = srgb_to_linear_rgba([0x10, 0x10, 0x10], 1.0);
        assert_eq!(
            [theme.bg[0], theme.bg[1], theme.bg[2]],
            [opaque[0], opaque[1], opaque[2]]
        );
        assert!((theme.bg[3] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn theme_from_config_clamps_out_of_range_opacity() {
        // The renderer-side clamp is the last line behind the client's
        // `clamped_opacity`.
        let low = Renderer::theme_from_config(&RendererConfig {
            background_opacity: Some(-1.0),
            ..Default::default()
        });
        assert_eq!(low.bg[3], 0.0);
        let high = Renderer::theme_from_config(&RendererConfig {
            background_opacity: Some(2.0),
            ..Default::default()
        });
        assert_eq!(high.bg[3], 1.0);
    }

    #[test]
    fn letterbox_origin_keeps_top_left_for_the_everyday_remainder() {
        assert_eq!(
            letterbox_origin(24, 80, 10.0, 20.0, 805.0, 485.0),
            [0.0, 0.0]
        );
    }

    #[test]
    fn letterbox_origin_centers_a_smaller_authoritative_grid() {
        assert_eq!(
            letterbox_origin(10, 40, 10.0, 20.0, 800.0, 600.0),
            [200.0, 200.0]
        );
    }

    #[test]
    fn letterbox_origin_bottom_anchors_a_larger_authoritative_grid() {
        assert_eq!(
            letterbox_origin(30, 100, 10.0, 20.0, 800.0, 500.0),
            [0.0, -100.0]
        );
    }

    #[test]
    fn clear_color_mirrors_the_resolved_theme_background_alpha_included() {
        // Dropping the alpha to 1.0 would punch an opaque hole through a
        // translucent window's margins.
        let theme = Theme {
            bg: [0.25, 0.5, 0.75, 0.5],
            ..Theme::default()
        };
        let clear = clear_color_for_theme(&ResolvedTheme::new(&theme), false);
        assert_eq!(
            [clear.r, clear.g, clear.b, clear.a],
            [0.25, 0.5, 0.75, 0.5].map(f64::from),
        );
        let premultiplied =
            clear_color_for_theme(&ResolvedTheme::new(&premultiplied_bg(theme, true)), false);
        assert_eq!(
            [
                premultiplied.r,
                premultiplied.g,
                premultiplied.b,
                premultiplied.a
            ],
            [0.125, 0.25, 0.375, 0.5].map(f64::from),
        );
    }

    #[test]
    fn clear_color_under_decscnm_takes_the_foreground_and_keeps_bg_alpha() {
        let theme = Theme {
            fg: [0.75, 0.5, 0.25, 1.0],
            bg: [0.125, 0.25, 0.375, 0.5],
            ..Theme::default()
        };
        let clear = clear_color_for_theme(&ResolvedTheme::new(&theme), true);
        assert_eq!(
            [clear.r, clear.g, clear.b, clear.a],
            [0.75, 0.5, 0.25, 0.5].map(f64::from),
        );
    }

    #[test]
    fn premultiplied_bg_scales_rgb_by_alpha_only_when_asked() {
        let theme = Theme {
            bg: [0.4, 0.6, 0.8, 0.5],
            ..Theme::default()
        };
        assert_eq!(premultiplied_bg(theme, false).bg, [0.4, 0.6, 0.8, 0.5]);
        assert_eq!(premultiplied_bg(theme, true).bg, [0.2, 0.3, 0.4, 0.5]);
    }

    #[test]
    fn select_alpha_mode_keeps_opaque_path_byte_for_byte() {
        use wgpu::CompositeAlphaMode::{Opaque, PostMultiplied};
        let modes = [Opaque, PostMultiplied];
        assert_eq!(select_alpha_mode(&modes, 1.0), (Opaque, false));
    }

    #[test]
    fn select_alpha_mode_prefers_straight_then_premultiplied() {
        use wgpu::CompositeAlphaMode::{Inherit, Opaque, PostMultiplied, PreMultiplied};
        // On macOS the `PostMultiplied` advertisement is nominal (see
        // `select_alpha_mode`).
        assert_eq!(
            select_alpha_mode(&[Opaque, PostMultiplied], 0.85),
            (PostMultiplied, cfg!(target_os = "macos"))
        );
        assert_eq!(
            select_alpha_mode(&[Opaque, PreMultiplied, Inherit], 0.85),
            (PreMultiplied, true)
        );
        assert_eq!(select_alpha_mode(&[Opaque, Inherit], 0.85), (Inherit, true));
    }

    #[test]
    fn select_alpha_mode_falls_back_to_opaque_when_no_transparent_mode() {
        use wgpu::CompositeAlphaMode::Opaque;
        assert_eq!(select_alpha_mode(&[Opaque], 0.5), (Opaque, false));
    }

    struct NoImages;

    impl ImageSource for NoImages {
        fn image_data(&self, _: ImageId) -> Option<ImageData<'_>> {
            None
        }
    }

    fn noop_instance() -> Instance {
        noop_instance_with(None)
    }

    fn noop_instance_with(limits: Option<wgpu::Limits>) -> Instance {
        let mut desc = InstanceDescriptor {
            backends: Backends::NOOP,
            ..InstanceDescriptor::new_without_display_handle()
        };
        desc.backend_options.noop = wgpu::NoopBackendOptions {
            enable: true,
            limits,
            ..wgpu::NoopBackendOptions::default()
        };
        Instance::new(desc)
    }

    /// `None` when `FELIS_TEST_FONT_DIR` is unset, the skip every
    /// font-dependent test in the crate takes.
    fn offscreen_on(
        instance: &Instance,
        width: u32,
        height: u32,
        cfg: RendererConfig,
    ) -> Option<Renderer> {
        let font = felis_shaping::Font::try_load_test_font()?;
        let cfg = offscreen_config(cfg).expect("no post shader configured");
        warmup::block_on(async {
            let adapter = request_adapter(instance, None).await.expect("adapter");
            let (device, queue) = request_device(&adapter).await.expect("device");
            Some(Renderer::build_offscreen(
                device,
                queue,
                UploadMode::for_adapter(&adapter),
                width,
                height,
                &cfg,
                FontStack::new(Arc::new(font)),
            ))
        })
    }

    /// Cursor hidden, so every pixel is the background.
    fn render_blank(renderer: &mut Renderer, rows: u16, cols: u16) {
        let mut grid = felis_grid::Grid::new(rows, cols);
        felis_vt::Parser::new().advance(&mut grid, b"\x1b[?25l");
        renderer
            .render(grid.screen(), &[], &NoImages, &[], &[], 0, 0)
            .expect("an offscreen render has no surface to lose");
    }

    #[test]
    fn new_offscreen_refuses_a_configured_post_shader() {
        let cfg = RendererConfig {
            post_shader_wgsl: Some(TRAIL_SHADER_SOURCE.to_owned()),
            ..RendererConfig::default()
        };
        let result = warmup::block_on(Renderer::new_offscreen(8, 8, cfg, None));
        assert!(matches!(result, Err(RendererError::OffscreenPostShader)));
    }

    #[test]
    fn read_frame_refuses_a_target_nothing_was_rendered_into() {
        let Some(mut renderer) = offscreen_on(&noop_instance(), 8, 8, RendererConfig::default())
        else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut out = vec![0xAA; 3];
        assert!(matches!(
            renderer.read_frame(&mut out),
            Err(ReadFrameError::NoFrame)
        ));
        assert_eq!(out, [0xAA; 3]);
    }

    #[test]
    fn set_post_shader_refuses_an_offscreen_target() {
        let Some(mut renderer) = offscreen_on(&noop_instance(), 8, 8, RendererConfig::default())
        else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        assert!(matches!(
            renderer.set_post_shader(Some(TRAIL_SHADER_SOURCE)),
            Err(PostShaderError::Offscreen)
        ));
        assert!(!renderer.has_post_shader());
        assert!(renderer.set_post_shader(None).is_ok());
    }

    /// 37 px rows are 148 bytes, padded to 256 for the copy: the
    /// padding must not reach `out`, before or after a resize.
    #[test]
    fn read_frame_returns_unpadded_rows_at_the_current_size() {
        let Some(mut renderer) = offscreen_on(&noop_instance(), 37, 5, RendererConfig::default())
        else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut out = vec![0xAA; 3];
        render_blank(&mut renderer, 1, 2);
        renderer.read_frame(&mut out).expect("readback");
        assert_eq!(out.len(), 37 * 5 * 4);

        renderer.resize(70, 3);
        assert!(matches!(
            renderer.read_frame(&mut out),
            Err(ReadFrameError::NoFrame)
        ));
        render_blank(&mut renderer, 1, 2);
        renderer
            .read_frame(&mut out)
            .expect("readback after resize");
        assert_eq!(out.len(), 70 * 3 * 4);
        assert_eq!(renderer.size(), (70, 3));
    }

    /// 300 rows of 256 padded bytes overflow a 64 KiB buffer limit: the
    /// readback must split into strips rather than trip wgpu validation.
    #[test]
    fn read_frame_splits_a_frame_past_the_buffer_limit_into_strips() {
        let limits = wgpu::Limits {
            max_buffer_size: 64 << 10,
            ..wgpu::Limits::default()
        };
        let instance = noop_instance_with(Some(limits));
        let Some(mut renderer) = offscreen_on(&instance, 37, 300, RendererConfig::default()) else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        render_blank(&mut renderer, 1, 2);
        let mut out = Vec::new();
        renderer.read_frame(&mut out).expect("readback in strips");
        assert_eq!(out.len(), 37 * 300 * 4);
    }

    fn assert_every_pixel(frame: &[u8], want: [u8; 4]) {
        for px in frame.as_chunks::<4>().0 {
            let close = px.iter().zip(want).all(|(got, w)| got.abs_diff(w) <= 1);
            assert!(close, "pixel {px:?}, want {want:?}");
        }
    }

    /// Pixels need a real adapter; the noop backend never executes a
    /// pass. `background_opacity` must not reach the offscreen clear,
    /// and a reused staging buffer must carry the new frame, not the old.
    #[test]
    #[ignore = "needs a GPU adapter; `just test-gpu` runs it on lavapipe"]
    fn an_offscreen_frame_reads_back_the_opaque_theme_background() {
        let cfg = RendererConfig {
            theme_bg: Some("#336699".to_owned()),
            background_opacity: Some(0.25),
            ..RendererConfig::default()
        };
        let Some(mut renderer) = offscreen_on(&new_instance(), 40, 20, cfg) else {
            eprintln!("FELIS_TEST_FONT_DIR unset; skipping");
            return;
        };
        let mut out = Vec::new();
        render_blank(&mut renderer, 1, 1);
        renderer.read_frame(&mut out).expect("readback");
        assert_eq!(out.len(), 40 * 20 * 4);
        assert_every_pixel(&out, [0x33, 0x66, 0x99, 0xFF]);

        renderer.apply_theme_override(ThemeChannel::Background, Some((0xCC, 0x11, 0x44)));
        render_blank(&mut renderer, 1, 1);
        renderer
            .read_frame(&mut out)
            .expect("readback of a second frame");
        assert_every_pixel(&out, [0xCC, 0x11, 0x44, 0xFF]);

        renderer.resize(24, 30);
        render_blank(&mut renderer, 1, 1);
        renderer
            .read_frame(&mut out)
            .expect("readback after resize");
        assert_eq!(out.len(), 24 * 30 * 4);
        assert_every_pixel(&out, [0xCC, 0x11, 0x44, 0xFF]);
    }
}
