//! The frontend probe report: `felis doctor` execs
//! `felis-client --doctor-probe`, which writes a versioned [`ProbeReport`] as
//! JSON to stdout. Linking wgpu and arboard directly into `felis` would defeat
//! the GPU-free front door.

use serde::{Deserialize, Serialize};

/// Bumped only when a field is removed or retyped; a consumer that
/// reads a `v` it does not know reports the probe as unusable.
pub const PROBE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeReport {
    /// [`PROBE_VERSION`] as the producing binary knew it.
    pub v: u32,
    pub client_version: String,
    pub gpu: GpuProbe,
    pub clipboard: ClipboardProbe,
    /// Written only for [`DOCTOR_REPORT_PROBE_FLAG`]: resolving the stack
    /// scans the system's fonts, a cost plain `felis doctor` must not pay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fonts: Option<FontsProbe>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuProbe {
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The wgpu backend token (`vulkan`, `metal`, `dx12`, `gl`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// `discrete_gpu`, `integrated_gpu`, `virtual_gpu`, `cpu`, `other`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// The driver's version string (Mesa, NVIDIA), where the backend
    /// reports one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver_info: Option<String>,
}

/// The faces a window would draw with, resolved from the same config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FontsProbe {
    Resolved {
        regular: String,
        bold: String,
        italic: String,
        bold_italic: String,
        fallbacks: Vec<String>,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardProbe {
    /// `false` is not a felis failure: the client falls back to its
    /// in-process bag for felis-internal copy/paste.
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl GpuProbe {
    #[must_use]
    pub const fn unavailable() -> Self {
        Self {
            available: false,
            name: None,
            backend: None,
            device_type: None,
            driver: None,
            driver_info: None,
        }
    }
}

/// The flag `felis` passes to `felis-client` to request a [`ProbeReport`].
pub const DOCTOR_PROBE_FLAG: &str = "--doctor-probe";

/// [`DOCTOR_PROBE_FLAG`] plus [`ProbeReport::fonts`], for
/// `felis doctor report`. Its optional `=<path>` value is the config
/// file to resolve fonts against, absolute; absent means the default
/// discovery a window launch makes.
pub const DOCTOR_REPORT_PROBE_FLAG: &str = "--doctor-report-probe";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_report_round_trips_through_json() {
        let report = ProbeReport {
            v: PROBE_VERSION,
            client_version: "0.1.0".to_owned(),
            gpu: GpuProbe {
                available: true,
                name: Some("Test Adapter".to_owned()),
                backend: Some("vulkan".to_owned()),
                device_type: Some("discrete_gpu".to_owned()),
                driver: Some("radv".to_owned()),
                driver_info: Some("Mesa 26.1.2".to_owned()),
            },
            clipboard: ClipboardProbe {
                available: false,
                detail: Some("no display".to_owned()),
            },
            fonts: Some(FontsProbe::Resolved {
                regular: "Mono Regular".to_owned(),
                bold: "Mono Bold".to_owned(),
                italic: "Mono Italic".to_owned(),
                bold_italic: "Mono Bold Italic".to_owned(),
                fallbacks: vec!["Emoji".to_owned()],
            }),
        };
        let text = serde_json::to_string(&report).expect("serialize");
        let back: ProbeReport = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, report);
    }

    #[test]
    fn a_failed_font_probe_round_trips_through_json() {
        let fonts = FontsProbe::Failed {
            error: "no font".to_owned(),
        };
        let text = serde_json::to_string(&fonts).expect("serialize");
        let back: FontsProbe = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, fonts);
    }

    /// The fields this epoch added stay optional: a client that predates
    /// them still parses under `PROBE_VERSION` 1.
    #[test]
    fn a_report_without_the_added_fields_parses() {
        let text = r#"{"v":1,"client_version":"0.1.0","gpu":{"available":true,"name":"A","driver":"d"},"clipboard":{"available":true}}"#;
        let report: ProbeReport = serde_json::from_str(text).expect("deserialize");
        assert_eq!(report.fonts, None);
        assert_eq!(report.gpu.driver_info, None);
    }

    /// Absent fields are omitted, not written as null.
    #[test]
    fn an_unavailable_gpu_writes_no_empty_fields() {
        let text = serde_json::to_string(&GpuProbe::unavailable()).expect("serialize");
        assert_eq!(text, r#"{"available":false}"#);
    }
}
