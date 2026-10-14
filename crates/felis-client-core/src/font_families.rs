//! The frontend's font-family listing: `felis fonts list` execs
//! `felis-client` with [`FONT_FAMILIES_PROBE_FLAG`], which writes one
//! [`FamiliesProbe`] as JSON to stdout. Enumerating faces takes the
//! shaping stack, which the GPU-free front door does not link.

use serde::{Deserialize, Serialize};

/// The flag `felis` passes to `felis-client` to request a [`FamiliesProbe`].
pub const FONT_FAMILIES_PROBE_FLAG: &str = "--font-families-probe";

/// Bumped only when a field is removed or retyped; a consumer that
/// reads a `v` it does not know refuses the listing.
pub const FAMILIES_PROBE_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FamiliesProbe {
    /// [`FAMILIES_PROBE_VERSION`] as the producing binary knew it.
    pub v: u32,
    #[serde(flatten)]
    pub outcome: FamiliesOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FamiliesOutcome {
    /// Sorted and deduplicated by the shaping stack's own rule.
    Listed {
        families: Vec<Family>,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Family {
    pub name: String,
    /// Every face of the family declares itself fixed-pitch.
    pub monospace: bool,
    /// Upright first, then by weight.
    pub styles: Vec<Style>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Style {
    /// On the OpenType 1–1000 scale.
    pub weight: u16,
    pub italic: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_round_trips_through_json() {
        let probe = FamiliesProbe {
            v: FAMILIES_PROBE_VERSION,
            outcome: FamiliesOutcome::Listed {
                families: vec![Family {
                    name: "DejaVu Sans Mono".to_owned(),
                    monospace: true,
                    styles: vec![Style {
                        weight: 700,
                        italic: true,
                    }],
                }],
            },
        };
        let text = serde_json::to_string(&probe).expect("serialize");
        assert_eq!(
            text,
            r#"{"v":2,"families":[{"name":"DejaVu Sans Mono","monospace":true,"styles":[{"weight":700,"italic":true}]}]}"#
        );
        let back: FamiliesProbe = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, probe);
    }

    #[test]
    fn a_failed_listing_round_trips_through_json() {
        let probe = FamiliesProbe {
            v: FAMILIES_PROBE_VERSION,
            outcome: FamiliesOutcome::Failed {
                error: "fontconfig: load libfontconfig.so.1".to_owned(),
            },
        };
        let text = serde_json::to_string(&probe).expect("serialize");
        let back: FamiliesProbe = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, probe);
    }
}
