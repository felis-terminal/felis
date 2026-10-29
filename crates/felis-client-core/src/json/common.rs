//! Vocabulary shared by more than one payload.

use felis_grid::row_json::RowJsonError;
use felis_protocol::messages::{GridDims, RequestedDims, ViewportAnchor};

/// Why a payload does not convert to the domain type it names.
#[derive(Debug, thiserror::Error)]
pub enum DtoError {
    #[error("row: {0}")]
    Row(#[from] RowJsonError),
    #[error("field `{field}`: {detail}")]
    Field {
        field: &'static str,
        detail: &'static str,
    },
    /// An arm the protocol deliberately leaves out, rather than one it
    /// failed to convert.
    #[error("`{0}` is outside the JSON protocol")]
    Outside(&'static str),
}

impl DtoError {
    pub(crate) const fn field(field: &'static str, detail: &'static str) -> Self {
        Self::Field { field, detail }
    }
}

/// Raw bytes as a padded standard-alphabet base64 string: an array of
/// integers costs a JSON reader four to five characters per byte.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Base64(pub Vec<u8>);

/// What a [`Base64`] string admits: whole padded groups and nothing else.
pub const BASE64_PATTERN: &str = "^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$";

impl From<Vec<u8>> for Base64 {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<Base64> for Vec<u8> {
    fn from(bytes: Base64) -> Self {
        bytes.0
    }
}

impl serde::Serialize for Base64 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut out = Vec::with_capacity(self.0.len().div_ceil(3) * 4);
        felis_protocol::base64::encode_into(&self.0, &mut out);
        serializer.serialize_str(std::str::from_utf8(&out).map_err(serde::ser::Error::custom)?)
    }
}

impl<'de> serde::Deserialize<'de> for Base64 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        // The shared decoder also takes whitespace and unpadded tails;
        // the schema's pattern does not, and a reader must refuse what
        // the schema refuses.
        if text.len() % 4 != 0 || text.bytes().any(|b| b.is_ascii_whitespace()) {
            return Err(serde::de::Error::custom("not padded base64"));
        }
        felis_protocol::base64::decode(text.as_bytes())
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom("not base64"))
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for Base64 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Base64".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "contentEncoding": "base64",
            "pattern": BASE64_PATTERN,
        })
    }
}

/// A session id, spelled as [`session_id_hex`] spells it and refused in
/// any other spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub u128);

impl serde::Serialize for SessionId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&session_id_hex(self.0))
    }
}

impl<'de> serde::Deserialize<'de> for SessionId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        session_id_from_hex(&text)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "schema")]
impl schemars::JsonSchema for SessionId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "SessionId".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "pattern": SESSION_ID_PATTERN,
        })
    }
}

json_dto! {
    /// A 24-bit color, as an object rather than the domain tuple.
    #[non_exhaustive]
    pub struct RgbJson {
        pub r: u8,
        pub g: u8,
        pub b: u8,
    }

    /// Authoritative grid geometry (`felis_protocol::messages::GridDims`).
    #[non_exhaustive]
    pub struct GridDimsJson {
        pub rows: u16,
        pub cols: u16,
        /// `0` means unknown, the `TIOCGWINSZ` convention.
        pub pixel_w: u16,
        /// `0` means unknown.
        pub pixel_h: u16,
    }

    /// Geometry a client asks for, before the daemon admits it.
    #[non_exhaustive]
    pub struct RequestedDimsJson {
        pub rows: u32,
        pub cols: u32,
        /// `0` means unknown.
        pub pixel_w: u32,
        /// `0` means unknown.
        pub pixel_h: u32,
    }

    /// Opaque history point of a browsing viewport offset; a client
    /// echoes it back unchanged.
    #[non_exhaustive]
    pub struct ViewportAnchorJson {
        pub pushed: u64,
        pub generation: u64,
    }
}

impl From<ViewportAnchor> for ViewportAnchorJson {
    fn from(a: ViewportAnchor) -> Self {
        Self {
            pushed: a.pushed,
            generation: a.generation,
        }
    }
}

impl From<ViewportAnchorJson> for ViewportAnchor {
    fn from(a: ViewportAnchorJson) -> Self {
        Self {
            pushed: a.pushed,
            generation: a.generation,
        }
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

/// The only spelling a session id has.
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
pub fn session_id_from_hex(text: &str) -> Result<u128, DtoError> {
    if text.len() != 32
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(DtoError::field("session", "not 32 lowercase hex digits"));
    }
    u128::from_str_radix(text, 16).map_err(|_| DtoError::field("session", "not hexadecimal"))
}

#[cfg(test)]
mod tests {
    use super::{Base64, DtoError, session_id_from_hex, session_id_hex};

    /// One spelling, the lowercase one: accepting the uppercase form
    /// would let two strings name one session on a surface whose
    /// pattern admits only one of them.
    #[test]
    fn an_uppercase_session_id_is_refused() {
        assert!(matches!(
            session_id_from_hex("0123456789ABCDEF0123456789ABCDEF"),
            Err(DtoError::Field { .. })
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

    #[test]
    fn bytes_round_trip_through_a_padded_base64_string() {
        for bytes in [&b""[..], b"h", b"hi", b"hi!", b"\x00\xff\x10\x80"] {
            let text = serde_json::to_string(&Base64(bytes.to_vec())).expect("bytes serialize");
            let back: Base64 = serde_json::from_str(&text).expect("the string reads back");
            assert_eq!(back.0, bytes, "{text}");
        }
        assert_eq!(
            serde_json::to_string(&Base64(b"hi".to_vec())).expect("bytes serialize"),
            r#""aGk=""#
        );
    }

    #[test]
    fn an_unpadded_or_wrapped_base64_string_is_refused() {
        for text in [r#""aGk""#, r#""aG k=""#, r#""aGk=\n""#, r#""a===""#] {
            assert!(
                serde_json::from_str::<Base64>(text).is_err(),
                "{text} must not read"
            );
        }
    }
}
