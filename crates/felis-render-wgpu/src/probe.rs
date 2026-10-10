//! Headless adapter probe.
//!
//! Unlike `Renderer::new_with_config`, this runs without a window handle to diagnose headless
//! or broken driver setups. A clean probe does not guarantee window surface creation will succeed.

use wgpu::{Backends, DeviceType, Instance, InstanceDescriptor, PowerPreference};

/// Fields are the `felis doctor --format json` surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterProbe {
    /// The adapter's own name, as the driver reports it.
    pub name: String,
    /// The wgpu backend token (`vulkan`, `metal`, `dx12`, `gl`, …).
    pub backend: String,
    /// `discrete_gpu`, `integrated_gpu`, `virtual_gpu`, `cpu`, `other`.
    pub device_type: String,
    /// Driver name, empty where the backend reports none.
    pub driver: String,
    /// Driver version (Mesa, NVIDIA), empty where the backend reports
    /// none.
    pub driver_info: String,
}

/// The same [`PowerPreference`] as the renderer, so the answer names
/// the adapter felis would actually pick.
pub async fn probe_adapter() -> Option<AdapterProbe> {
    let instance = Instance::new(InstanceDescriptor {
        backends: Backends::PRIMARY,
        ..InstanceDescriptor::new_without_display_handle()
    });
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            compatible_surface: None,
            // Not `true`: a software fallback reports success on a
            // machine where felis is unusably slow.
            force_fallback_adapter: false,
            apply_limit_buckets: false,
        })
        .await
        .ok()?;
    let info = adapter.get_info();
    Some(AdapterProbe {
        name: info.name,
        backend: info.backend.to_str().to_owned(),
        device_type: device_type_token(info.device_type).to_owned(),
        driver: info.driver,
        driver_info: info.driver_info,
    })
}

/// Not derived from `Debug`: these tokens are part of
/// `felis doctor --format json`, and a wgpu rename must not retype them.
const fn device_type_token(device_type: DeviceType) -> &'static str {
    match device_type {
        DeviceType::Other => "other",
        DeviceType::IntegratedGpu => "integrated_gpu",
        DeviceType::DiscreteGpu => "discrete_gpu",
        DeviceType::VirtualGpu => "virtual_gpu",
        DeviceType::Cpu => "cpu",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tokens are the machine contract, so pin every arm.
    #[test]
    fn device_type_tokens_are_the_documented_set() {
        for (device_type, token) in [
            (DeviceType::Other, "other"),
            (DeviceType::IntegratedGpu, "integrated_gpu"),
            (DeviceType::DiscreteGpu, "discrete_gpu"),
            (DeviceType::VirtualGpu, "virtual_gpu"),
            (DeviceType::Cpu, "cpu"),
        ] {
            assert_eq!(device_type_token(device_type), token);
        }
    }
}
