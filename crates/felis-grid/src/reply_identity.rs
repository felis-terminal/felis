//! The host's name in the XTVERSION and DA3 replies.

/// Printable ASCII only: the grid copies both strings into a DCS reply,
/// where a control byte would end or corrupt the frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplyIdentity {
    xtversion: Box<str>,
    unit_id: Box<str>,
}

impl ReplyIdentity {
    /// `xtversion` is the XTVERSION payload (`felis 0.3.0`); `unit_id` is
    /// the DA3 unit ID before hex encoding (`felis-1`). `None` when
    /// either holds a byte outside `0x20..=0x7e`.
    #[must_use]
    pub fn new(xtversion: &str, unit_id: &str) -> Option<Self> {
        let printable = |s: &str| s.bytes().all(|b| (0x20..=0x7e).contains(&b));
        (printable(xtversion) && printable(unit_id)).then(|| Self {
            xtversion: xtversion.into(),
            unit_id: unit_id.into(),
        })
    }

    #[must_use]
    pub fn xtversion(&self) -> &str {
        &self.xtversion
    }

    #[must_use]
    pub fn unit_id(&self) -> &str {
        &self.unit_id
    }

    pub(crate) fn xtversion_reply(&self) -> Vec<u8> {
        let mut reply = Vec::with_capacity(self.xtversion.len() + 6);
        reply.extend_from_slice(b"\x1bP>|");
        reply.extend_from_slice(self.xtversion.as_bytes());
        reply.extend_from_slice(b"\x1b\\");
        reply
    }

    pub(crate) fn da3_reply(identity: Option<&Self>) -> Vec<u8> {
        let mut reply = b"\x1bP!|".to_vec();
        match identity {
            Some(identity) => {
                for b in identity.unit_id.bytes() {
                    reply.extend_from_slice(format!("{b:02x}").as_bytes());
                }
            }
            None => reply.extend_from_slice(b"00000000"),
        }
        reply.extend_from_slice(b"\x1b\\");
        reply
    }
}
