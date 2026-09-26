//! Vocabulary shared by more than one family's DTO.

use felis_protocol::messages::{GridDims, RequestedDims};

use super::JsonError;

json_dto! {
    /// A 24-bit color, as an object rather than the domain tuple.
    pub struct RgbJson {
        pub r: u8,
        pub g: u8,
        pub b: u8,
    }

    /// Authoritative grid geometry (`felis_protocol::messages::GridDims`).
    pub struct GridDimsJson {
        pub rows: u16,
        pub cols: u16,
        /// `0` means unknown, the `TIOCGWINSZ` convention.
        pub pixel_w: u16,
        /// `0` means unknown.
        pub pixel_h: u16,
    }

    /// Geometry a client asks for, before the daemon admits it.
    pub struct RequestedDimsJson {
        pub rows: u32,
        pub cols: u32,
        /// `0` means unknown.
        pub pixel_w: u32,
        /// `0` means unknown.
        pub pixel_h: u32,
    }
}

impl From<(u8, u8, u8)> for RgbJson {
    fn from((r, g, b): (u8, u8, u8)) -> Self {
        Self { r, g, b }
    }
}

impl From<RgbJson> for (u8, u8, u8) {
    fn from(rgb: RgbJson) -> Self {
        (rgb.r, rgb.g, rgb.b)
    }
}

impl From<GridDims> for GridDimsJson {
    fn from(dims: GridDims) -> Self {
        Self {
            rows: dims.rows,
            cols: dims.cols,
            pixel_w: dims.pixel_w,
            pixel_h: dims.pixel_h,
        }
    }
}

impl From<GridDimsJson> for GridDims {
    fn from(dims: GridDimsJson) -> Self {
        Self {
            rows: dims.rows,
            cols: dims.cols,
            pixel_w: dims.pixel_w,
            pixel_h: dims.pixel_h,
        }
    }
}

impl From<RequestedDims> for RequestedDimsJson {
    fn from(dims: RequestedDims) -> Self {
        Self {
            rows: dims.rows,
            cols: dims.cols,
            pixel_w: dims.pixel_w,
            pixel_h: dims.pixel_h,
        }
    }
}

impl From<RequestedDimsJson> for RequestedDims {
    fn from(dims: RequestedDimsJson) -> Self {
        Self {
            rows: dims.rows,
            cols: dims.cols,
            pixel_w: dims.pixel_w,
            pixel_h: dims.pixel_h,
        }
    }
}

/// The only spelling a v1 session id has.
pub const SESSION_ID_PATTERN: &str = "^[0-9a-f]{32}$";

/// A session id as every felis machine surface publishes it: the full
/// 32-digit lowercase hex rendering. A JSON number would lose the low
/// bits of a `u128` in any consumer that reads numbers as `f64`.
#[must_use]
pub fn session_id_hex(id: u128) -> String {
    format!("{id:032x}")
}

/// Inverse of [`session_id_hex`].
///
/// # Errors
///
/// When `text` is not 32 lowercase hex digits.
pub fn session_id_from_hex(text: &str) -> Result<u128, JsonError> {
    if text.len() != 32
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(JsonError::field(
            "session id",
            "not 32 lowercase hex digits",
        ));
    }
    u128::from_str_radix(text, 16).map_err(|_| JsonError::field("session id", "not hexadecimal"))
}

#[cfg(test)]
mod tests {
    use super::{JsonError, session_id_from_hex, session_id_hex};

    /// One spelling, the lowercase one: accepting the uppercase form
    /// would let two strings name one session on a surface whose
    /// pattern admits only one of them.
    #[test]
    fn an_uppercase_session_id_is_refused() {
        assert!(matches!(
            session_id_from_hex("0123456789ABCDEF0123456789ABCDEF"),
            Err(JsonError::Field { .. })
        ));
    }

    #[test]
    fn a_session_id_round_trips_through_its_lowercase_spelling() {
        let id = 0x0123_4567_89ab_cdef_0123_4567_89ab_cdefu128;
        let text = session_id_hex(id);
        assert_eq!(text, "0123456789abcdef0123456789abcdef");
        assert_eq!(session_id_from_hex(&text).expect("it parses back"), id);
    }

    #[test]
    fn a_short_session_id_is_refused() {
        assert!(session_id_from_hex("0123abcd").is_err());
    }
}
