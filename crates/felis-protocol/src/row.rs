//! Opaque per-row cell payload for `GridMsg::RowDelta`.
//!
//! Row codec format is specified in `docs/reference/row-codec.md`.

use serde::{Deserialize, Serialize};

/// Encoded cells for one row; the bytes to hand `felis_grid::decode_row`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RowPayload(pub Vec<u8>);

impl From<Vec<u8>> for RowPayload {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `From<Vec<u8>>` wraps the exact bytes it was handed.
    #[test]
    fn from_vec_preserves_the_exact_bytes() {
        let bytes = vec![0xDE_u8, 0xAD, 0xBE, 0xEF];
        let via_from = RowPayload::from(bytes.clone());
        assert_eq!(via_from.0, bytes);
        let via_into: RowPayload = bytes.clone().into();
        assert_eq!(via_into.0, bytes);
        assert_ne!(via_from, RowPayload::default());
    }

    /// The serde form is byte-identical to a plain `Vec<u8>`.
    #[test]
    fn the_serde_form_matches_a_plain_byte_vec() {
        let bytes = vec![0xDE_u8, 0xAD, 0xBE, 0xEF, 0x00, 0x7F];
        let via_payload = postcard::to_allocvec(&RowPayload(bytes.clone())).unwrap();
        let via_vec = postcard::to_allocvec(&bytes).unwrap();
        assert_eq!(via_payload, via_vec);
        let back: RowPayload = postcard::from_bytes(&via_payload).unwrap();
        assert_eq!(back, RowPayload(bytes));
    }
}
