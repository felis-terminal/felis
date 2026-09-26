//! Pipeline scaffolding for the bg + fg passes: the WGSL source and the
//! vertex layouts mirroring the instance POD structs. Device-side
//! construction lives in the renderer.

use std::mem::offset_of;
use std::num::NonZeroU64;

use bytemuck::{Pod, Zeroable};
use wgpu::{
    BindGroupLayoutEntry, BindingType, BufferBindingType, SamplerBindingType, ShaderStages,
    TextureSampleType, TextureViewDimension, VertexAttribute, VertexBufferLayout, VertexFormat,
    VertexStepMode,
};

use crate::instances::{BgInstance, DecorationInstance, FgInstance, ImgInstance};

/// CPU-side mirror of the WGSL `Viewport` uniform. `origin_px` doubles
/// as the 16-byte rounding the uniform address space requires, so there
/// is no explicit trailing pad.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Pod, Zeroable)]
pub struct ViewportUniform {
    /// Framebuffer size in physical pixels (`width`, `height`).
    pub size_px: [f32; 2],
    /// Content-block origin in physical pixels, added to every instance
    /// position in the shader (the mirroring letterbox,
    /// docs/explanation/architecture/session-lifecycle.md "Same-user mirroring").
    pub origin_px: [f32; 2],
}

impl ViewportUniform {
    #[must_use]
    pub const fn new(width: u32, height: u32) -> Self {
        Self::with_origin(width, height, [0.0, 0.0])
    }

    #[must_use]
    pub const fn with_origin(width: u32, height: u32, origin_px: [f32; 2]) -> Self {
        Self {
            size_px: [width as f32, height as f32],
            origin_px,
        }
    }
}

pub const SHADER_SOURCE: &str = include_str!("shader.wgsl");

/// Locations match the `@location(N)` slots of `bg_vs`'s `BgInstance`.
#[must_use]
pub const fn bg_instance_layout() -> VertexBufferLayout<'static> {
    const ATTRS: &[VertexAttribute] = &[
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(BgInstance, origin_px) as u64,
            shader_location: 0,
        },
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(BgInstance, size_px) as u64,
            shader_location: 1,
        },
        VertexAttribute {
            format: VertexFormat::Float32x4,
            offset: offset_of!(BgInstance, color) as u64,
            shader_location: 2,
        },
    ];
    VertexBufferLayout {
        array_stride: size_of::<BgInstance>() as u64,
        step_mode: VertexStepMode::Instance,
        attributes: ATTRS,
    }
}

#[must_use]
pub const fn img_instance_layout() -> VertexBufferLayout<'static> {
    const ATTRS: &[VertexAttribute] = &[
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(ImgInstance, origin_px) as u64,
            shader_location: 0,
        },
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(ImgInstance, size_px) as u64,
            shader_location: 1,
        },
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(ImgInstance, uv_min) as u64,
            shader_location: 2,
        },
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(ImgInstance, uv_max) as u64,
            shader_location: 3,
        },
        VertexAttribute {
            format: VertexFormat::Float32,
            offset: offset_of!(ImgInstance, opacity) as u64,
            shader_location: 4,
        },
    ];
    VertexBufferLayout {
        array_stride: size_of::<ImgInstance>() as u64,
        step_mode: VertexStepMode::Instance,
        attributes: ATTRS,
    }
}

#[must_use]
pub const fn fg_instance_layout() -> VertexBufferLayout<'static> {
    const ATTRS: &[VertexAttribute] = &[
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(FgInstance, origin_px) as u64,
            shader_location: 0,
        },
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(FgInstance, size_px) as u64,
            shader_location: 1,
        },
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(FgInstance, uv_min) as u64,
            shader_location: 2,
        },
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(FgInstance, uv_max) as u64,
            shader_location: 3,
        },
        VertexAttribute {
            format: VertexFormat::Float32x4,
            offset: offset_of!(FgInstance, color) as u64,
            shader_location: 4,
        },
        VertexAttribute {
            format: VertexFormat::Float32,
            offset: offset_of!(FgInstance, is_color) as u64,
            shader_location: 5,
        },
    ];
    VertexBufferLayout {
        array_stride: size_of::<FgInstance>() as u64,
        step_mode: VertexStepMode::Instance,
        attributes: ATTRS,
    }
}

/// Locations match the `@location(N)` slots of `deco_vs`'s `DecoInstance`.
#[must_use]
pub const fn decoration_instance_layout() -> VertexBufferLayout<'static> {
    const ATTRS: &[VertexAttribute] = &[
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(DecorationInstance, origin_px) as u64,
            shader_location: 0,
        },
        VertexAttribute {
            format: VertexFormat::Float32x2,
            offset: offset_of!(DecorationInstance, size_px) as u64,
            shader_location: 1,
        },
        VertexAttribute {
            format: VertexFormat::Float32x4,
            offset: offset_of!(DecorationInstance, color) as u64,
            shader_location: 2,
        },
        VertexAttribute {
            format: VertexFormat::Float32,
            offset: offset_of!(DecorationInstance, kind) as u64,
            shader_location: 3,
        },
        VertexAttribute {
            format: VertexFormat::Float32,
            offset: offset_of!(DecorationInstance, thickness_px) as u64,
            shader_location: 4,
        },
        VertexAttribute {
            format: VertexFormat::Float32,
            offset: offset_of!(DecorationInstance, period_px) as u64,
            shader_location: 5,
        },
    ];
    VertexBufferLayout {
        array_stride: size_of::<DecorationInstance>() as u64,
        step_mode: VertexStepMode::Instance,
        attributes: ATTRS,
    }
}

/// `@group(0)`: the viewport uniform. `min_binding_size` is pinned so
/// wgpu rejects an undersized buffer at bind time rather than inside
/// the shader.
#[must_use]
pub const fn viewport_bind_group_layout_entries() -> [BindGroupLayoutEntry; 1] {
    [BindGroupLayoutEntry {
        binding: 0,
        visibility: ShaderStages::VERTEX,
        ty: BindingType::Buffer {
            ty: BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: NonZeroU64::new(size_of::<ViewportUniform>() as u64),
        },
        count: None,
    }]
}

/// `@group(1)` of the image pass; the glyph pass uses the wider
/// [`glyph_bind_group_layout_entries`].
#[must_use]
pub const fn atlas_bind_group_layout_entries() -> [BindGroupLayoutEntry; 2] {
    [
        BindGroupLayoutEntry {
            binding: 0,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Texture {
                sample_type: TextureSampleType::Float { filterable: true },
                view_dimension: TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        BindGroupLayoutEntry {
            binding: 1,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Sampler(SamplerBindingType::Filtering),
            count: None,
        },
    ]
}

/// `@group(1)` of the glyph pass: coverage atlas (0), sampler (1),
/// color-emoji atlas (2). Separate from
/// [`atlas_bind_group_layout_entries`] so the image pipeline binds no
/// redundant color view.
#[must_use]
pub const fn glyph_bind_group_layout_entries() -> [BindGroupLayoutEntry; 3] {
    [
        BindGroupLayoutEntry {
            binding: 0,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Texture {
                sample_type: TextureSampleType::Float { filterable: true },
                view_dimension: TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
        BindGroupLayoutEntry {
            binding: 1,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Sampler(SamplerBindingType::Filtering),
            count: None,
        },
        BindGroupLayoutEntry {
            binding: 2,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Texture {
                sample_type: TextureSampleType::Float { filterable: true },
                view_dimension: TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    #![allow(clippy::float_cmp)]

    use wgpu::naga::{
        Binding, Handle, Module, ScalarKind, StructMember, Type, TypeInner, VectorSize,
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    use super::*;

    #[test]
    fn shader_source_parses_and_validates() {
        let module = wgsl::parse_str(SHADER_SOURCE).expect("WGSL parse");
        let mut validator = Validator::new(ValidationFlags::all(), Capabilities::all());
        validator.validate(&module).expect("WGSL validate");
    }

    fn instance_members(module: &Module, entry: &str) -> Vec<StructMember> {
        let function = &module
            .entry_points
            .iter()
            .find(|ep| ep.name == entry)
            .unwrap_or_else(|| panic!("no entry point `{entry}`"))
            .function;
        let arg = function
            .arguments
            .iter()
            .find(|arg| arg.binding.is_none())
            .unwrap_or_else(|| panic!("`{entry}` takes no struct argument"));
        match &module.types[arg.ty].inner {
            TypeInner::Struct { members, .. } => members.clone(),
            other => panic!("`{entry}`'s instance argument is not a struct: {other:?}"),
        }
    }

    fn wgsl_type_accepts(module: &Module, ty: Handle<Type>, format: VertexFormat) -> bool {
        match (&module.types[ty].inner, format) {
            (&TypeInner::Scalar(scalar), VertexFormat::Float32)
            | (
                &TypeInner::Vector {
                    size: VectorSize::Bi,
                    scalar,
                },
                VertexFormat::Float32x2,
            )
            | (
                &TypeInner::Vector {
                    size: VectorSize::Quad,
                    scalar,
                },
                VertexFormat::Float32x4,
            ) => scalar.kind == ScalarKind::Float && scalar.width == 4,
            _ => false,
        }
    }

    #[test]
    fn wgsl_instance_structs_match_the_rust_vertex_layouts() {
        // Nothing at build time ties the layouts to the WGSL structs; a
        // `@location` renumber must fail here, not corrupt the fetch.
        let module = wgsl::parse_str(SHADER_SOURCE).expect("WGSL parse");
        for (entry, layout) in [
            ("bg_vs", bg_instance_layout()),
            ("fg_vs", fg_instance_layout()),
            ("deco_vs", decoration_instance_layout()),
            ("image_vs", img_instance_layout()),
        ] {
            let members = instance_members(&module, entry);
            assert_eq!(
                members.len(),
                layout.attributes.len(),
                "{entry}: field count"
            );
            for attr in layout.attributes {
                let member = members
                    .iter()
                    .find(|m| {
                        matches!(
                            &m.binding,
                            Some(Binding::Location { location, .. })
                                if *location == attr.shader_location
                        )
                    })
                    .unwrap_or_else(|| {
                        panic!("{entry}: no @location({}) member", attr.shader_location)
                    });
                assert_eq!(
                    u64::from(member.offset),
                    attr.offset,
                    "{entry}: @location({}) offset",
                    attr.shader_location
                );
                assert!(
                    wgsl_type_accepts(&module, member.ty, attr.format),
                    "{entry}: @location({}) is not fed by {:?}",
                    attr.shader_location,
                    attr.format
                );
            }
        }
    }

    #[test]
    fn bg_layout_offsets_are_field_offsets() {
        let layout = bg_instance_layout();
        assert_eq!(layout.array_stride, size_of::<BgInstance>() as u64);
        assert_eq!(layout.attributes.len(), 3);
        assert_eq!(layout.attributes[0].offset, 0);
        assert_eq!(layout.attributes[1].offset, 8);
        assert_eq!(layout.attributes[2].offset, 16);
    }

    #[test]
    fn img_layout_offsets_are_field_offsets() {
        let layout = img_instance_layout();
        assert_eq!(layout.array_stride, size_of::<ImgInstance>() as u64);
        assert_eq!(layout.attributes.len(), 5);
        assert_eq!(layout.attributes[0].offset, 0);
        assert_eq!(layout.attributes[1].offset, 8);
        assert_eq!(layout.attributes[2].offset, 16);
        assert_eq!(layout.attributes[3].offset, 24);
        assert_eq!(layout.attributes[4].offset, 32);
    }

    #[test]
    fn fg_layout_offsets_are_field_offsets() {
        let layout = fg_instance_layout();
        assert_eq!(layout.array_stride, size_of::<FgInstance>() as u64);
        assert_eq!(layout.attributes.len(), 6);
        assert_eq!(layout.attributes[0].offset, 0);
        assert_eq!(layout.attributes[1].offset, 8);
        assert_eq!(layout.attributes[2].offset, 16);
        assert_eq!(layout.attributes[3].offset, 24);
        assert_eq!(layout.attributes[4].offset, 32);
        assert_eq!(layout.attributes[5].offset, 48);
    }

    #[test]
    fn decoration_layout_offsets_are_field_offsets() {
        let layout = decoration_instance_layout();
        assert_eq!(
            layout.array_stride,
            size_of::<DecorationInstance>() as u64,
            "stride tracks the struct so a new field can't silently desync",
        );
        assert_eq!(layout.attributes.len(), 6);
        assert_eq!(layout.attributes[0].offset, 0);
        assert_eq!(layout.attributes[1].offset, 8);
        assert_eq!(layout.attributes[2].offset, 16);
        assert_eq!(layout.attributes[3].offset, 32);
        assert_eq!(layout.attributes[4].offset, 36);
        assert_eq!(layout.attributes[5].offset, 40);
    }

    #[test]
    fn viewport_uniform_is_16_bytes_with_size_at_origin() {
        assert_eq!(size_of::<ViewportUniform>(), 16);
        let u = ViewportUniform::new(800, 600);
        assert_eq!(u.size_px, [800.0, 600.0]);
        let bytes: &[u8] = bytemuck::bytes_of(&u);
        let first_f32 = f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert!((first_f32 - 800.0).abs() < f32::EPSILON);
    }

    #[test]
    fn viewport_bgl_is_one_vertex_uniform() {
        let entries = viewport_bind_group_layout_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].binding, 0);
        assert!(entries[0].visibility.contains(ShaderStages::VERTEX));
        match entries[0].ty {
            BindingType::Buffer {
                ty,
                has_dynamic_offset,
                min_binding_size,
            } => {
                assert_eq!(ty, BufferBindingType::Uniform);
                assert!(!has_dynamic_offset);
                assert_eq!(
                    min_binding_size.map(NonZeroU64::get),
                    Some(size_of::<ViewportUniform>() as u64)
                );
            }
            _ => panic!("expected Buffer binding"),
        }
    }

    #[test]
    fn atlas_bgl_is_filterable_texture_plus_sampler() {
        let entries = atlas_bind_group_layout_entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].binding, 0);
        assert!(entries[0].visibility.contains(ShaderStages::FRAGMENT));
        match entries[0].ty {
            BindingType::Texture {
                sample_type,
                view_dimension,
                multisampled,
            } => {
                assert_eq!(sample_type, TextureSampleType::Float { filterable: true });
                assert_eq!(view_dimension, TextureViewDimension::D2);
                assert!(!multisampled);
            }
            _ => panic!("expected Texture binding at slot 0"),
        }
        assert_eq!(entries[1].binding, 1);
        assert!(entries[1].visibility.contains(ShaderStages::FRAGMENT));
        match entries[1].ty {
            BindingType::Sampler(kind) => assert_eq!(kind, SamplerBindingType::Filtering),
            _ => panic!("expected Sampler binding at slot 1"),
        }
    }

    #[test]
    fn shader_locations_cover_every_field() {
        let bg_locs: Vec<u32> = bg_instance_layout()
            .attributes
            .iter()
            .map(|a| a.shader_location)
            .collect();
        assert_eq!(bg_locs, vec![0, 1, 2]);
        let fg_locs: Vec<u32> = fg_instance_layout()
            .attributes
            .iter()
            .map(|a| a.shader_location)
            .collect();
        assert_eq!(fg_locs, vec![0, 1, 2, 3, 4, 5]);
        let img_locs: Vec<u32> = img_instance_layout()
            .attributes
            .iter()
            .map(|a| a.shader_location)
            .collect();
        assert_eq!(img_locs, vec![0, 1, 2, 3, 4]);
        let deco_locs: Vec<u32> = decoration_instance_layout()
            .attributes
            .iter()
            .map(|a| a.shader_location)
            .collect();
        assert_eq!(deco_locs, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn glyph_bgl_is_coverage_texture_sampler_and_color_texture() {
        let entries = glyph_bind_group_layout_entries();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].binding, 0);
        assert!(entries[0].visibility.contains(ShaderStages::FRAGMENT));
        match entries[0].ty {
            BindingType::Texture {
                sample_type,
                view_dimension,
                multisampled,
            } => {
                assert_eq!(sample_type, TextureSampleType::Float { filterable: true });
                assert_eq!(view_dimension, TextureViewDimension::D2);
                assert!(!multisampled);
            }
            _ => panic!("expected Texture binding at slot 0"),
        }
        assert_eq!(entries[1].binding, 1);
        assert!(entries[1].visibility.contains(ShaderStages::FRAGMENT));
        match entries[1].ty {
            BindingType::Sampler(kind) => assert_eq!(kind, SamplerBindingType::Filtering),
            _ => panic!("expected Sampler binding at slot 1"),
        }
        assert_eq!(entries[2].binding, 2);
        assert!(entries[2].visibility.contains(ShaderStages::FRAGMENT));
        match entries[2].ty {
            BindingType::Texture {
                sample_type,
                view_dimension,
                multisampled,
            } => {
                assert_eq!(sample_type, TextureSampleType::Float { filterable: true });
                assert_eq!(view_dimension, TextureViewDimension::D2);
                assert!(!multisampled);
            }
            _ => panic!("expected Texture binding at slot 2"),
        }
    }
}
