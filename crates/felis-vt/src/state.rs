//! Parser state an in-place daemon upgrade carries across `execve`
//! (`docs/explanation/architecture/overview.md` "In-place upgrade"). The
//! `state-dump` derives admit any value; `check_restored` rejects the ones
//! the parser could never reach and would index out of bounds on.

use std::fmt;

use crate::{
    APC_BUFFER_LIMIT, LimitedBuffer, MAX_INTERMEDIATES, MAX_PARAMS, OSC_BUFFER_LIMIT, Parser,
    State, utf8::Decoder,
};

/// A restored value no run of the parser could have produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError(pub String);

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateError {}

fn check(ok: bool, what: &str) -> Result<(), StateError> {
    if ok {
        Ok(())
    } else {
        Err(StateError(what.to_owned()))
    }
}

/// Byte payloads as standard base64 strings: a JSON number array costs
/// up to four bytes per byte, base64 four per three.
pub mod b64 {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use serde::{Deserialize as _, Deserializer, Serializer, de::Error as _};

    /// # Errors
    /// Only the serializer's own.
    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&encode(bytes))
    }

    /// # Errors
    /// A string that is not base64.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        decode::<D>(&String::deserialize(deserializer)?)
    }

    #[must_use]
    pub fn encode(bytes: &[u8]) -> String {
        STANDARD.encode(bytes)
    }

    pub(super) fn decode<'de, D: Deserializer<'de>>(text: &str) -> Result<Vec<u8>, D::Error> {
        STANDARD
            .decode(text)
            .map_err(|_| D::Error::custom("invalid base64"))
    }
}

/// [`b64`] for an optional payload.
pub mod opt_b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    /// # Errors
    /// Only the serializer's own.
    #[expect(clippy::ref_option, reason = "the signature serde's `with` calls")]
    pub fn serialize<S: Serializer>(
        bytes: &Option<Vec<u8>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => serializer.serialize_some(&super::b64::encode(bytes)),
            None => serializer.serialize_none(),
        }
    }

    /// # Errors
    /// A string that is not base64.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Vec<u8>>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| super::b64::decode::<D>(&text))
            .transpose()
    }
}

impl<const LIMIT: usize> serde::Serialize for LimitedBuffer<LIMIT> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        b64::serialize(&self.bytes, serializer)
    }
}

impl<'de, const LIMIT: usize> serde::Deserialize<'de> for LimitedBuffer<LIMIT> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        b64::deserialize(deserializer).map(|bytes| Self { bytes })
    }
}

impl<const LIMIT: usize> Default for LimitedBuffer<LIMIT> {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    /// # Errors
    /// A parameter, intermediate, or string buffer past its cap.
    pub fn check_restored(&self) -> Result<(), StateError> {
        check(self.params.len <= MAX_PARAMS, "parser parameter count")?;
        check(
            self.intermediates.len <= MAX_INTERMEDIATES,
            "parser intermediate count",
        )?;
        check(
            self.osc.bytes.len() <= OSC_BUFFER_LIMIT,
            "parser OSC buffer",
        )?;
        check(
            self.apc.bytes.len() <= APC_BUFFER_LIMIT,
            "parser APC buffer",
        )?;
        check(
            !self.osc_pending || matches!(self.state, State::Escape),
            "parser pending OSC outside Escape",
        )
    }
}

impl Decoder {
    /// # Errors
    /// A held sequence longer than the one its lead byte announced.
    pub fn check_restored(&self) -> Result<(), StateError> {
        let held = usize::from(self.needed);
        check(
            if self.len == 0 {
                self.needed == 0
            } else {
                (1..=3).contains(&held) && self.len <= held
            },
            "UTF-8 decoder state",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Sink;

    /// Every Williams action, in order, so two parsers that dispatch
    /// alike record alike.
    #[derive(Default, PartialEq, Eq, Debug)]
    struct Recorder(Vec<String>);

    impl Sink for Recorder {
        fn print(&mut self, byte: u8) {
            self.0.push(format!("print {byte}"));
        }
        fn execute(&mut self, byte: u8) {
            self.0.push(format!("exec {byte}"));
        }
        fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8) {
            self.0.push(format!("esc {intermediates:?} {final_byte}"));
        }
        fn csi_dispatch(
            &mut self,
            params: &[u16],
            subparams: u32,
            intermediates: &[u8],
            ignore: bool,
            final_byte: u8,
        ) {
            self.0.push(format!(
                "csi {params:?} {subparams} {intermediates:?} {ignore} {final_byte}"
            ));
        }
        fn dcs_hook(&mut self, params: &[u16], intermediates: &[u8], ignore: bool, final_byte: u8) {
            self.0.push(format!(
                "hook {params:?} {intermediates:?} {ignore} {final_byte}"
            ));
        }
        fn dcs_put(&mut self, byte: u8) {
            self.0.push(format!("put {byte}"));
        }
        fn dcs_unhook(&mut self) {
            self.0.push("unhook".to_owned());
        }
        fn osc_dispatch(&mut self, body: &[u8], end: crate::OscEnd) {
            self.0.push(format!("osc {body:?} {end:?}"));
        }
        fn apc_dispatch(&mut self, body: &[u8]) {
            self.0.push(format!("apc {body:?}"));
        }
    }

    fn round_trip<T: serde::Serialize + serde::de::DeserializeOwned>(value: &T) -> T {
        serde_json::from_slice(&serde_json::to_vec(value).unwrap()).unwrap()
    }

    /// Splits each sequence at every byte, carries the parser across the
    /// split, and checks the rest dispatches exactly as it would have
    /// without the hop.
    #[test]
    fn a_parser_stopped_mid_sequence_resumes_where_it_stopped() {
        let streams: [&[u8]; 7] = [
            b"\x1b[38:2::10:20:30;1;4:3m",
            b"\x1b]8;id=x;https://example.com/a\x1b\\",
            b"\x1b]2;title\x1b[1m",
            b"\x1b]2;title\x07",
            b"\x1b_Ga=T,f=24,s=1,v=1;AAAA\x1b\\",
            b"\x1bP$qm\x1b\\",
            b"\x1b(B\x1b[?1049h",
        ];
        for stream in streams {
            for split in 0..=stream.len() {
                let mut original = Parser::new();
                let mut head = Recorder::default();
                original.advance(&mut head, &stream[..split]);
                let mut restored = round_trip(&original);
                restored.check_restored().unwrap();
                assert_eq!(restored, original);
                let (mut want, mut got) = (Recorder::default(), Recorder::default());
                original.advance(&mut want, &stream[split..]);
                restored.advance(&mut got, &stream[split..]);
                assert_eq!(got, want, "split at {split} of {stream:?}");
            }
        }
    }

    #[test]
    fn a_utf8_sequence_split_across_the_hop_decodes_whole() {
        let mut decoder = Decoder::new();
        let mut seen = Vec::new();
        for &b in &"\u{1F600}".as_bytes()[..2] {
            decoder.push(b, |c| seen.push(c));
        }
        let mut restored = round_trip(&decoder);
        restored.check_restored().unwrap();
        for &b in &"\u{1F600}".as_bytes()[2..] {
            restored.push(b, |c| seen.push(c));
        }
        assert_eq!(seen, ['\u{1F600}']);
    }

    #[test]
    fn states_no_run_could_reach_are_refused() {
        let mut parser = Parser::new();
        parser.params.len = MAX_PARAMS + 1;
        assert!(parser.check_restored().is_err());
        let mut parser = Parser::new();
        parser.osc_pending = true;
        assert!(parser.check_restored().is_err());
        let decoder: Decoder =
            serde_json::from_str(r#"{"buf":[240,159,0,0],"len":3,"needed":1}"#).unwrap();
        assert!(decoder.check_restored().is_err());
    }

    #[test]
    fn an_absent_field_takes_the_fresh_parser_value() {
        let restored: Parser = serde_json::from_str("{}").unwrap();
        assert_eq!(restored, Parser::new());
    }
}
