//! Shared wgpu resource factories for the glyph and image atlases.

use crate::{buffer_ring::UploadMode, texture_upload};
use wgpu::{
    AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindingResource,
    Device, Extent3d, FilterMode, MipmapFilterMode, Sampler, SamplerDescriptor, Texture,
    TextureDescriptor, TextureDimension, TextureFormat, TextureView,
};

pub(crate) fn create_atlas_texture(
    device: &Device,
    mode: UploadMode,
    side: u32,
    format: TextureFormat,
    label: &str,
) -> Texture {
    device.create_texture(&TextureDescriptor {
        label: Some(label),
        size: Extent3d {
            width: side,
            height: side,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: texture_upload::atlas_usage(mode),
        view_formats: texture_upload::atlas_view_formats(mode, format),
    })
}

/// Linear, not nearest: sub-pixel cell offsets and scaled image
/// placements show seams under nearest sampling.
pub(crate) fn create_atlas_sampler(device: &Device, label: &str) -> Sampler {
    device.create_sampler(&SamplerDescriptor {
        label: Some(label),
        address_mode_u: AddressMode::ClampToEdge,
        address_mode_v: AddressMode::ClampToEdge,
        address_mode_w: AddressMode::ClampToEdge,
        mag_filter: FilterMode::Linear,
        min_filter: FilterMode::Linear,
        mipmap_filter: MipmapFilterMode::Nearest,
        ..Default::default()
    })
}

/// Binding slots 0 (view), 1 (sampler) and 2 (optional color-emoji
/// view) are the WGSL contract shared by both passes.
pub(crate) fn create_atlas_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    view: &TextureView,
    sampler: &Sampler,
    extra_view: Option<&TextureView>,
    label: &str,
) -> BindGroup {
    let mut entries = vec![
        BindGroupEntry {
            binding: 0,
            resource: BindingResource::TextureView(view),
        },
        BindGroupEntry {
            binding: 1,
            resource: BindingResource::Sampler(sampler),
        },
    ];
    if let Some(extra) = extra_view {
        entries.push(BindGroupEntry {
            binding: 2,
            resource: BindingResource::TextureView(extra),
        });
    }
    device.create_bind_group(&BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &entries,
    })
}
