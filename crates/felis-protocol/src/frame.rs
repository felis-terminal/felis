//! Frame layer: `len:u32 | kind:u16 | body`.
//!
//! Defined in `docs/reference/ipc.md` "Frame layer". Multi-byte integers
//! are little-endian. [`FrameError::BodyTooLarge`] is raised before
//! body allocation.

use thiserror::Error;

/// Wire-format frame header size: `len(4) + kind(2) = 6` bytes.
pub const HEADER_LEN: usize = 6;

/// Bytes covered by the `len` field that are not the body (`kind`).
pub const LEN_OVERHEAD: u32 = 2;

/// Default body-size ceiling: the 50 MiB rehydration burst from
/// `docs/explanation/architecture/ipc.md` "Goals" with headroom.
pub const DEFAULT_MAX_BODY: u32 = 64 * 1024 * 1024;

#[derive(Debug, Error, PartialEq, Eq, Clone)]
pub enum FrameError {
    /// Buffer is shorter than the frame; keep reading.
    #[error("frame incomplete: need {need} more bytes")]
    Incomplete {
        /// Additional bytes required beyond what was passed in.
        need: usize,
    },
    /// Fatal: a body larger than the ceiling (advertised by a peer header
    /// or passed to a local encode).
    #[error("frame body length {body_len} exceeds ceiling {ceiling}")]
    BodyTooLarge {
        /// Body length. Wider than the `len` field so an outbound body
        /// past `u32` reports its real size instead of a wrapped one.
        body_len: u64,
        ceiling: u32,
    },
    /// `len` does not even cover the inner header field.
    #[error("frame len {len} does not cover the inner header ({LEN_OVERHEAD} bytes)")]
    LenUnderflow {
        /// `len` field as read.
        len: u32,
    },
}

/// Decoded frame view borrowing from the input buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    /// Family; see `MessageKind` in `lib.rs`.
    pub kind: u16,
    /// Encoded body bytes: always protobuf binary, the wire negotiates
    /// no other encoding.
    pub body: &'a [u8],
}

impl Frame<'_> {
    /// Total size on the wire including the 6-byte header.
    #[must_use]
    pub const fn encoded_len(&self) -> usize {
        HEADER_LEN + self.body.len()
    }

    /// The body length as a `len` field can carry it, or the error a
    /// refusal reports (REQ-105).
    ///
    /// # Errors
    /// [`FrameError::BodyTooLarge`] for a body past [`DEFAULT_MAX_BODY`].
    pub fn checked_body_len(&self) -> Result<u32, FrameError> {
        u32::try_from(self.body.len())
            .ok()
            .filter(|len| *len <= DEFAULT_MAX_BODY)
            .ok_or(FrameError::BodyTooLarge {
                body_len: self.body.len() as u64,
                ceiling: DEFAULT_MAX_BODY,
            })
    }

    /// Appends the encoded form to `out`.
    ///
    /// # Errors
    /// Returns [`FrameError::BodyTooLarge`] when the body exceeds
    /// [`DEFAULT_MAX_BODY`], checked before `u32` narrowing to prevent wrap.
    pub fn encode_to(&self, out: &mut Vec<u8>) -> Result<(), FrameError> {
        let len = LEN_OVERHEAD + self.checked_body_len()?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(self.body);
        Ok(())
    }

    /// # Errors
    /// As [`Self::encode_to`].
    pub fn encode(&self) -> Result<Vec<u8>, FrameError> {
        // Checked before the reservation, not only inside `encode_to`:
        // reserving for an over-limit body first would abort the
        // process on an allocation failure the caller was promised as
        // a typed refusal.
        self.checked_body_len()?;
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode_to(&mut out)?;
        Ok(out)
    }
}

/// Decode a frame from the head of `input`, returning the frame and
/// the number of bytes consumed, with [`DEFAULT_MAX_BODY`] as ceiling.
pub fn decode(input: &[u8]) -> Result<(Frame<'_>, usize), FrameError> {
    decode_with_ceiling(input, DEFAULT_MAX_BODY)
}

/// Like [`decode`] but with an explicit body-size ceiling.
pub fn decode_with_ceiling(input: &[u8], ceiling: u32) -> Result<(Frame<'_>, usize), FrameError> {
    if input.len() < 4 {
        return Err(FrameError::Incomplete {
            need: 4 - input.len(),
        });
    }
    let len = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
    if len < LEN_OVERHEAD {
        return Err(FrameError::LenUnderflow { len });
    }
    let body_len = len - LEN_OVERHEAD;
    if body_len > ceiling {
        return Err(FrameError::BodyTooLarge {
            body_len: u64::from(body_len),
            ceiling,
        });
    }
    let total = HEADER_LEN + body_len as usize;
    if input.len() < total {
        return Err(FrameError::Incomplete {
            need: total - input.len(),
        });
    }
    let kind = u16::from_le_bytes([input[4], input[5]]);
    let body = &input[HEADER_LEN..total];
    Ok((Frame { kind, body }, total))
}

#[cfg(kani)]
mod kani_proofs {
    #![allow(clippy::expect_used)]

    use super::{Frame, decode};

    /// Encode then decode reproduces the frame and reports the full
    /// encoded length as consumed, for all header values.
    #[kani::proof]
    #[kani::unwind(21)]
    fn frame_round_trips() {
        let kind: u16 = kani::any();
        let body: [u8; 4] = kani::any();
        let frame = Frame { kind, body: &body };
        let bytes = frame.encode().expect("a 4-byte body is inside the ceiling");
        let (decoded, consumed) = decode(&bytes).expect("a freshly encoded frame decodes");
        assert!(decoded.kind == kind);
        assert!(decoded.body == &body[..]);
        assert!(consumed == frame.encoded_len());
    }

    #[kani::proof]
    fn decode_never_panics_on_arbitrary_bytes() {
        let input: [u8; 16] = kani::any();
        let _ = decode(&input);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_body_round_trips() {
        let f = Frame { kind: 0, body: &[] };
        let bytes = f.encode().unwrap();
        assert_eq!(bytes.len(), HEADER_LEN);
        let (decoded, consumed) = decode(&bytes).unwrap();
        assert_eq!(decoded, f);
        assert_eq!(consumed, HEADER_LEN);
    }

    #[test]
    fn body_round_trips() {
        let body = b"hello-felis";
        let f = Frame { kind: 2, body };
        let bytes = f.encode().unwrap();
        let (decoded, consumed) = decode(&bytes).unwrap();
        assert_eq!(decoded.kind, 2);
        assert_eq!(decoded.body, body);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn encoded_len_counts_header_plus_body() {
        let f = Frame {
            kind: 0,
            body: b"abcde",
        };
        assert_eq!(f.encoded_len(), HEADER_LEN + 5);
        let empty = Frame { kind: 0, body: &[] };
        assert_eq!(empty.encoded_len(), HEADER_LEN);
    }

    #[test]
    fn one_byte_buffer_needs_three_more_for_length_field() {
        match decode(&[0u8]) {
            Err(FrameError::Incomplete { need }) => assert_eq!(need, 3),
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn body_len_exactly_at_ceiling_is_accepted() {
        let body = b"abcd";
        let f = Frame { kind: 0, body };
        let bytes = f.encode().unwrap();
        let (decoded, _) = decode_with_ceiling(&bytes, body.len() as u32).unwrap();
        assert_eq!(decoded.body, body);
    }

    #[test]
    fn partial_body_reports_remaining_byte_count() {
        let body = b"abcdefgh";
        let f = Frame { kind: 0, body };
        let bytes = f.encode().unwrap();
        match decode(&bytes[..9]) {
            Err(FrameError::Incomplete { need }) => assert_eq!(need, 5),
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn encode_to_appends_rather_than_replaces() {
        let f = Frame {
            kind: 2,
            body: b"abc",
        };
        let mut out = b"prior".to_vec();
        f.encode_to(&mut out).unwrap();
        assert_eq!(&out[..5], b"prior");
        let (decoded, _) = decode(&out[5..]).unwrap();
        assert_eq!(decoded.body, b"abc");
    }

    #[test]
    fn body_too_large_is_rejected_before_any_body_allocation() {
        let mut bytes = vec![0u8; 4];
        let len: u32 = LEN_OVERHEAD + 100;
        bytes[..4].copy_from_slice(&len.to_le_bytes());
        let outcome = decode_with_ceiling(&bytes, 50);
        assert_eq!(
            outcome,
            Err(FrameError::BodyTooLarge {
                body_len: 100,
                ceiling: 50,
            })
        );
    }

    /// The ceiling is inclusive on encode, exactly as it is on decode.
    #[test]
    fn a_body_at_the_ceiling_encodes_and_one_past_it_does_not() {
        let body = vec![0u8; DEFAULT_MAX_BODY as usize];
        let at = Frame {
            kind: 0,
            body: &body,
        };
        assert_eq!(
            at.encode().unwrap().len(),
            HEADER_LEN + DEFAULT_MAX_BODY as usize
        );

        let body = vec![0u8; DEFAULT_MAX_BODY as usize + 1];
        let past = Frame {
            kind: 0,
            body: &body,
        };
        assert_eq!(
            past.encode(),
            Err(FrameError::BodyTooLarge {
                body_len: u64::from(DEFAULT_MAX_BODY) + 1,
                ceiling: DEFAULT_MAX_BODY,
            })
        );
    }

    /// A refused encode appends nothing, so a shared scratch buffer
    /// cannot carry half a frame into the next write.
    #[test]
    fn a_refused_encode_leaves_the_output_buffer_untouched() {
        let body = vec![0u8; DEFAULT_MAX_BODY as usize + 1];
        let mut out = b"prior".to_vec();
        assert!(
            Frame {
                kind: 0,
                body: &body
            }
            .encode_to(&mut out)
            .is_err()
        );
        assert_eq!(out, b"prior");
    }

    #[test]
    fn len_field_smaller_than_inner_header_is_rejected() {
        let mut bytes = vec![0u8; 4];
        bytes[..4].copy_from_slice(&1u32.to_le_bytes());
        match decode(&bytes) {
            Err(FrameError::LenUnderflow { len }) => assert_eq!(len, 1),
            other => panic!("expected LenUnderflow, got {other:?}"),
        }
    }

    #[test]
    fn two_frames_decode_independently_from_a_concatenated_stream() {
        let a = Frame {
            kind: 0,
            body: b"first",
        };
        let b = Frame {
            kind: 1,
            body: b"second-frame-here",
        };
        let mut wire = a.encode().unwrap();
        wire.extend_from_slice(&b.encode().unwrap());

        let (decoded_a, consumed_a) = decode(&wire).unwrap();
        assert_eq!(decoded_a, a);
        let (decoded_b, consumed_b) = decode(&wire[consumed_a..]).unwrap();
        assert_eq!(decoded_b, b);
        assert_eq!(consumed_a + consumed_b, wire.len());
    }
}
