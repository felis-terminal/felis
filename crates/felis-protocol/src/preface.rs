//! Frozen version preface and handshake bootstrap.
//!
//! Exchanged before schema-encoded frames; specified in `docs/reference/ipc.md`.

use thiserror::Error;

/// Leading bytes of both prefaces: `FLIS`, ASCII. A peer whose first
/// bytes are anything else is not felis, and the connection closes with
/// no reply at all: writing one would answer an unknown protocol in ours.
pub const MAGIC: [u8; 4] = *b"FLIS";

/// Protocol major this build speaks. Bumped only for a semantic break
/// (`docs/explanation/architecture/ipc.md`
/// "Schema evolution: major, minor, feature flag"). Until
/// the compatibility freeze, pre-freeze breaks change the bytes under
/// this same major.
pub const PROTOCOL_MAJOR: u16 = 1;

/// Protocol minor this build speaks: additive schema growth only, each
/// addition with defined old-peer behavior (`docs/reference/ipc.md`
/// "The minor ledger").
pub const PROTOCOL_MINOR: u16 = 0;

/// Ordered list of minors and their additions (`docs/reference/ipc.md` "The minor ledger").
///
/// Tested to ensure every [`PROTOCOL_MINOR`] bump records its old-peer behavior.
pub const MINOR_LEDGER: &[(u16, &str)] = &[(0, "the base schema of protocol major 1")];

const _: () = {
    let last = MINOR_LEDGER[MINOR_LEDGER.len() - 1].0;
    assert!(
        last == PROTOCOL_MINOR,
        "PROTOCOL_MINOR moved without a MINOR_LEDGER entry (and a ledger row in docs/reference/ipc.md)"
    );
};

/// Lowest protocol major this build can serve.
pub const SUPPORTED_MAJOR_MIN: u16 = PROTOCOL_MAJOR;

/// Highest protocol major this build can serve.
pub const SUPPORTED_MAJOR_MAX: u16 = PROTOCOL_MAJOR;

/// Size of the client preface. Frozen.
pub const CLIENT_PREFACE_LEN: usize = 8;

/// Size of the daemon reply. Frozen, and always written.
pub const DAEMON_PREFACE_LEN: usize = 10;

/// Reply status: the daemon serves the client's major.
const STATUS_ACCEPT: u16 = 0;

/// Reply status: the daemon does not serve the client's major; the two
/// words are its supported range.
const STATUS_REFUSE: u16 = 1;

/// The preface did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum PrefaceError {
    /// The first four bytes were not [`MAGIC`].
    #[error("not a felis peer (magic {magic:02x?})")]
    NotFelis {
        /// The four bytes that arrived where the magic belongs.
        magic: [u8; 4],
    },
}

/// Client → daemon: what the dialing peer speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientPreface {
    pub major: u16,
    pub minor: u16,
}

impl ClientPreface {
    /// What this build sends.
    pub const CURRENT: Self = Self {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
    };

    #[must_use]
    pub const fn encode(self) -> [u8; CLIENT_PREFACE_LEN] {
        let major = self.major.to_be_bytes();
        let minor = self.minor.to_be_bytes();
        [
            MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], major[0], major[1], minor[0], minor[1],
        ]
    }

    /// # Errors
    /// [`PrefaceError::NotFelis`] when the magic does not match.
    // By reference so the signature matches the ten-byte reply decoder.
    #[allow(clippy::trivially_copy_pass_by_ref)]
    pub const fn decode(bytes: &[u8; CLIENT_PREFACE_LEN]) -> Result<Self, PrefaceError> {
        if let Err(err) = check_magic(bytes) {
            return Err(err);
        }
        Ok(Self {
            major: u16::from_be_bytes([bytes[4], bytes[5]]),
            minor: u16::from_be_bytes([bytes[6], bytes[7]]),
        })
    }
}

/// Daemon → client, discriminated by its status word alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonPreface {
    /// Status 0. Frames follow, protobuf binary, under `major`.
    Accept {
        /// The agreed protocol major.
        major: u16,
        /// The daemon's own minor; the client mins it with its own.
        minor: u16,
    },
    /// Status 1. The daemon serves majors `min_major..=max_major`, none
    /// of which is the client's, and closes.
    Refuse {
        /// Lowest major the daemon serves.
        min_major: u16,
        /// Highest major the daemon serves.
        max_major: u16,
    },
    /// Any other status: a refusal whose reason postdates this build.
    /// A caller treats it exactly as a refusal.
    Unknown {
        /// The status word as it arrived.
        status: u16,
        /// The two trailing words, uninterpreted.
        words: [u16; 2],
    },
}

/// The accept this build answers a served major with: status 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonAccept {
    /// The agreed protocol major.
    pub major: u16,
    /// The daemon's own minor; the client mins it with its own.
    pub minor: u16,
}

impl DaemonAccept {
    /// Selects the schema version to speak for `client`'s major, or `None`.
    ///
    /// Accepts major and advertises minor atomically (`docs/explanation/architecture/ipc.md`).
    #[must_use]
    pub const fn select(client: ClientPreface) -> Option<Self> {
        match client.major {
            PROTOCOL_MAJOR => Some(Self {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            }),
            _ => None,
        }
    }
}

impl From<DaemonAccept> for DaemonPreface {
    fn from(DaemonAccept { major, minor }: DaemonAccept) -> Self {
        Self::Accept { major, minor }
    }
}

/// The refusal this build answers an unserved major with: status 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DaemonRefuse {
    /// Lowest major the daemon serves.
    pub min_major: u16,
    /// Highest major the daemon serves.
    pub max_major: u16,
}

impl DaemonRefuse {
    /// This build's refusal, carrying the majors it does serve.
    pub const CURRENT: Self = Self {
        min_major: SUPPORTED_MAJOR_MIN,
        max_major: SUPPORTED_MAJOR_MAX,
    };
}

impl From<DaemonRefuse> for DaemonPreface {
    fn from(
        DaemonRefuse {
            min_major,
            max_major,
        }: DaemonRefuse,
    ) -> Self {
        Self::Refuse {
            min_major,
            max_major,
        }
    }
}

impl DaemonPreface {
    #[must_use]
    pub const fn encode(self) -> [u8; DAEMON_PREFACE_LEN] {
        let (status, w1, w2) = match self {
            Self::Accept { major, minor } => (STATUS_ACCEPT, major, minor),
            Self::Refuse {
                min_major,
                max_major,
            } => (STATUS_REFUSE, min_major, max_major),
            Self::Unknown { status, words } => (status, words[0], words[1]),
        };
        let status = status.to_be_bytes();
        let w1 = w1.to_be_bytes();
        let w2 = w2.to_be_bytes();
        [
            MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], status[0], status[1], w1[0], w1[1], w2[0],
            w2[1],
        ]
    }

    /// # Errors
    /// [`PrefaceError::NotFelis`] when the magic does not match.
    pub const fn decode(bytes: &[u8; DAEMON_PREFACE_LEN]) -> Result<Self, PrefaceError> {
        if let Err(err) = check_magic(bytes) {
            return Err(err);
        }
        let status = u16::from_be_bytes([bytes[4], bytes[5]]);
        let w1 = u16::from_be_bytes([bytes[6], bytes[7]]);
        let w2 = u16::from_be_bytes([bytes[8], bytes[9]]);
        Ok(match status {
            STATUS_ACCEPT => Self::Accept {
                major: w1,
                minor: w2,
            },
            STATUS_REFUSE => Self::Refuse {
                min_major: w1,
                max_major: w2,
            },
            other => Self::Unknown {
                status: other,
                words: [w1, w2],
            },
        })
    }
}

const fn check_magic(bytes: &[u8]) -> Result<(), PrefaceError> {
    let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
    if magic[0] == MAGIC[0] && magic[1] == MAGIC[1] && magic[2] == MAGIC[2] && magic[3] == MAGIC[3]
    {
        Ok(())
    } else {
        Err(PrefaceError::NotFelis { magic })
    }
}

/// Whether this build serves `major`. Derived from
/// [`DaemonAccept::select`] rather than from the supported range, so
/// the predicate and the reply cannot disagree.
#[must_use]
pub const fn supports_major(major: u16) -> bool {
    DaemonAccept::select(ClientPreface { major, minor: 0 }).is_some()
}

/// The minor both peers can rely on: the lower of the two.
#[must_use]
pub const fn effective_minor(client: u16, daemon: u16) -> u16 {
    if client < daemon { client } else { daemon }
}

/// The reply did not accept the offer, or accepted something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum NegotiationError {
    /// Status 0 naming a major the client never offered. Not a
    /// refusal: a peer that answers this way is not running felis's
    /// negotiation, and the frames after it would be decoded under a
    /// schema it did not agree to.
    #[error("accepted protocol major {accepted}, but the offer was {offered}")]
    AcceptedUnofferedMajor {
        /// The major the client sent.
        offered: u16,
        /// The major the accept named.
        accepted: u16,
    },
    /// Status 1: the daemon serves `min..=max`, which excludes the offer.
    #[error("refused: the daemon serves protocol majors {min}-{max}")]
    Refused {
        /// Lowest major the daemon serves.
        min: u16,
        /// Highest major the daemon serves.
        max: u16,
    },
    /// A status this build cannot name; the words are reported as they
    /// arrived, never read as a major range.
    #[error("refused with preface status {status} (words {}, {})", words[0], words[1])]
    Unknown {
        /// The status word as it arrived.
        status: u16,
        /// The two trailing words, uninterpreted.
        words: [u16; 2],
    },
}

/// Confirms preface negotiation on the client and returns the effective minor.
///
/// # Errors
/// Returns [`NegotiationError`] if the daemon accepted an unoffered major or refused.
pub const fn confirm_accept(
    offered: ClientPreface,
    reply: DaemonPreface,
) -> Result<u16, NegotiationError> {
    match reply {
        DaemonPreface::Accept { major, minor } if major == offered.major => {
            Ok(effective_minor(offered.minor, minor))
        }
        DaemonPreface::Accept { major, .. } => Err(NegotiationError::AcceptedUnofferedMajor {
            offered: offered.major,
            accepted: major,
        }),
        DaemonPreface::Refuse {
            min_major,
            max_major,
        } => Err(NegotiationError::Refused {
            min: min_major,
            max: max_major,
        }),
        DaemonPreface::Unknown { status, words } => {
            Err(NegotiationError::Unknown { status, words })
        }
    }
}

/// Whether a connection at `effective` may carry an addition introduced
/// in `added_in`: the send-side gate every minor addition asks before
/// writing itself onto the wire.
#[must_use]
pub const fn minor_defines(effective: u16, added_in: u16) -> bool {
    effective >= added_in
}

/// Leading bytes of the optional relay carrier block: `FRLY`, ASCII.
/// Without the block a create carried over SSH has no host-correct
/// environment base: a warm remote daemon's own environment descends
/// from an earlier SSH connection and carries a dead `SSH_AUTH_SOCK`.
pub const CARRIER_MAGIC: [u8; 4] = *b"FRLY";

/// Size of the carrier block's fixed header: [`CARRIER_MAGIC`] plus the
/// big-endian `u32` payload length. Frozen for all time, so that a
/// payload in any format stays skippable by length.
pub const CARRIER_HEADER_LEN: usize = 8;

/// Carrier payload format this build writes. It leads the payload, not
/// the header: an older daemon must be able to read the length word at
/// the offset it already knows, or a widened header would leave both
/// peers waiting on a length that is not there
/// (`docs/explanation/architecture/ipc.md` "Handshake bootstrap").
pub const CARRIER_FORMAT_VERSION: u16 = 1;

/// Most entries a carrier block may carry under
/// [`CARRIER_FORMAT_VERSION`], frozen with that version: a cap a peer
/// picked for itself would reintroduce the skew the frozen layer exists
/// to remove. Checked by the relay on send and the daemon on read
/// (`docs/reference/ipc.md` "Relay carrier block").
pub const MAX_CARRIER_ENTRIES: u32 = 4096;

/// Largest carrier payload, in bytes, whatever format version it
/// declares. Checked before the payload is read so a bogus length word
/// costs a close rather than an allocation
/// (`docs/reference/ipc.md` "Relay carrier block").
pub const MAX_CARRIER_PAYLOAD_BYTES: u32 = 1 << 20;

/// A carrier block that did not parse, or a snapshot too big to send.
/// Every variant closes the connection like a wrong magic does. No
/// variant carries an environment name or value: diagnostics never
/// carry the environment (`docs/explanation/security-model.md`
/// "Process and environment boundary").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CarrierError {
    /// The first four bytes were not [`CARRIER_MAGIC`].
    #[error("not a felis relay carrier block (magic {magic:02x?})")]
    NotCarrier {
        /// The four bytes that arrived where the magic belongs.
        magic: [u8; 4],
    },
    /// The payload length word or the entry count exceeded its cap.
    #[error("carrier block over cap ({found} > {cap})")]
    OverCap {
        /// What the block declared.
        found: u32,
        /// The frozen limit it broke.
        cap: u32,
    },
    /// An inner length ran past the end of the payload.
    #[error("carrier block truncated")]
    Truncated,
    /// The payload declared format version 0, which no format ever
    /// uses: it is what a writer from before the version word puts
    /// there, in the high half of its entry count.
    #[error("carrier block declares format version 0")]
    ZeroVersion,
    /// Bytes were left over after the entries. Trailing bytes are
    /// malformed, not slack: the client preface starts at the byte
    /// after the payload.
    #[error("carrier block has {0} trailing bytes")]
    TrailingBytes(usize),
}

/// The relay's environment snapshot: name/value pairs in the capturing
/// host's raw platform representation (`SpawnArgs.env_base`'s encoding:
/// Unix bytes, Windows little-endian `u16` code units). The receiving
/// daemon is authoritative for validity and for the denylist.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct CarrierBlock {
    /// The captured pairs, in capture order.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Deliberately partial: environment names and values must never reach
/// a log (`docs/explanation/security-model.md` "Process and
/// environment boundary"), and a `?block` in a tracing call is one
/// keystroke away.
impl core::fmt::Debug for CarrierBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CarrierBlock")
            .field("entries", &self.env.len())
            .finish()
    }
}

impl CarrierBlock {
    /// Encode the whole block, header and payload.
    ///
    /// # Errors
    /// [`CarrierError::OverCap`] when the snapshot breaks either cap.
    pub fn encode(&self) -> Result<Vec<u8>, CarrierError> {
        let count = u32::try_from(self.env.len()).unwrap_or(u32::MAX);
        if count > MAX_CARRIER_ENTRIES {
            return Err(CarrierError::OverCap {
                found: count,
                cap: MAX_CARRIER_ENTRIES,
            });
        }
        let mut payload = Vec::new();
        payload.extend_from_slice(&CARRIER_FORMAT_VERSION.to_be_bytes());
        payload.extend_from_slice(&count.to_be_bytes());
        for (name, value) in &self.env {
            // Saturating: a field this long breaks the payload cap below
            // anyway, and that is the check with a number the operator
            // can act on.
            let name_len = u32::try_from(name.len()).unwrap_or(u32::MAX);
            let value_len = u32::try_from(value.len()).unwrap_or(u32::MAX);
            payload.extend_from_slice(&name_len.to_be_bytes());
            payload.extend_from_slice(name);
            payload.extend_from_slice(&value_len.to_be_bytes());
            payload.extend_from_slice(value);
        }
        let payload_len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
        if payload_len > MAX_CARRIER_PAYLOAD_BYTES {
            return Err(CarrierError::OverCap {
                found: payload_len,
                cap: MAX_CARRIER_PAYLOAD_BYTES,
            });
        }
        let mut block = Vec::with_capacity(CARRIER_HEADER_LEN + payload.len());
        block.extend_from_slice(&CARRIER_MAGIC);
        block.extend_from_slice(&payload_len.to_be_bytes());
        block.extend_from_slice(&payload);
        Ok(block)
    }

    /// Decodes a full payload of length reported by [`carrier_payload_len`].
    ///
    /// # Errors
    /// Returns [`CarrierError`] on version 0, over-cap counts,
    /// truncation, or trailing bytes.
    pub fn decode_payload(payload: &[u8]) -> Result<CarrierPayload, CarrierError> {
        let mut cursor = Cursor { bytes: payload };
        let version = cursor.u16()?;
        match version {
            0 => Err(CarrierError::ZeroVersion),
            CARRIER_FORMAT_VERSION => Ok(CarrierPayload::Block(Self::decode_v1(cursor)?)),
            other => Ok(CarrierPayload::UnknownVersion(other)),
        }
    }

    fn decode_v1(mut cursor: Cursor<'_>) -> Result<Self, CarrierError> {
        let count = cursor.u32()?;
        if count > MAX_CARRIER_ENTRIES {
            return Err(CarrierError::OverCap {
                found: count,
                cap: MAX_CARRIER_ENTRIES,
            });
        }
        let mut env = Vec::new();
        for _ in 0..count {
            let name = cursor.bytes()?;
            let value = cursor.bytes()?;
            env.push((name.to_vec(), value.to_vec()));
        }
        if !cursor.bytes.is_empty() {
            return Err(CarrierError::TrailingBytes(cursor.bytes.len()));
        }
        Ok(Self { env })
    }
}

/// What a carrier payload held once its format version was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CarrierPayload {
    /// A payload in a format version this build decodes.
    Block(CarrierBlock),
    /// A format version this build has no decoder for. The payload was
    /// already consumed by its length word, so the client preface still
    /// begins at the byte after it and the receiver degrades to the
    /// no-block case (`docs/reference/ipc.md` "Relay carrier block").
    UnknownVersion(u16),
}

/// Reads carrier header magic and returns validated payload length.
///
/// # Errors
/// Returns [`CarrierError::NotCarrier`] or [`CarrierError::OverCap`].
// By reference so the signature matches the preface decoders.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub const fn carrier_payload_len(header: &[u8; CARRIER_HEADER_LEN]) -> Result<u32, CarrierError> {
    let magic = [header[0], header[1], header[2], header[3]];
    if magic[0] != CARRIER_MAGIC[0]
        || magic[1] != CARRIER_MAGIC[1]
        || magic[2] != CARRIER_MAGIC[2]
        || magic[3] != CARRIER_MAGIC[3]
    {
        return Err(CarrierError::NotCarrier { magic });
    }
    let len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if len > MAX_CARRIER_PAYLOAD_BYTES {
        return Err(CarrierError::OverCap {
            found: len,
            cap: MAX_CARRIER_PAYLOAD_BYTES,
        });
    }
    Ok(len)
}

struct Cursor<'a> {
    bytes: &'a [u8],
}

impl<'a> Cursor<'a> {
    fn u16(&mut self) -> Result<u16, CarrierError> {
        let (head, rest) = self
            .bytes
            .split_at_checked(2)
            .ok_or(CarrierError::Truncated)?;
        self.bytes = rest;
        Ok(u16::from_be_bytes([head[0], head[1]]))
    }

    fn u32(&mut self) -> Result<u32, CarrierError> {
        let (head, rest) = self
            .bytes
            .split_at_checked(4)
            .ok_or(CarrierError::Truncated)?;
        self.bytes = rest;
        Ok(u32::from_be_bytes([head[0], head[1], head[2], head[3]]))
    }

    fn bytes(&mut self) -> Result<&'a [u8], CarrierError> {
        let len = self.u32()? as usize;
        let (head, rest) = self
            .bytes
            .split_at_checked(len)
            .ok_or(CarrierError::Truncated)?;
        self.bytes = rest;
        Ok(head)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client preface's bytes are pinned literally, big-endian.
    #[test]
    fn client_preface_byte_layout_is_frozen() {
        let bytes = ClientPreface {
            major: 0x0102,
            minor: 0x0304,
        }
        .encode();
        assert_eq!(bytes, [b'F', b'L', b'I', b'S', 0x01, 0x02, 0x03, 0x04]);
        assert_eq!(
            ClientPreface::CURRENT.encode(),
            [b'F', b'L', b'I', b'S', 0x00, 0x01, 0x00, 0x00]
        );
    }

    #[test]
    fn daemon_preface_byte_layouts_are_frozen() {
        assert_eq!(
            DaemonPreface::Accept {
                major: 0x0102,
                minor: 0x0304,
            }
            .encode(),
            [b'F', b'L', b'I', b'S', 0x00, 0x00, 0x01, 0x02, 0x03, 0x04]
        );
        assert_eq!(
            DaemonPreface::Refuse {
                min_major: 2,
                max_major: 3,
            }
            .encode(),
            [b'F', b'L', b'I', b'S', 0x00, 0x01, 0x00, 0x02, 0x00, 0x03]
        );
        assert_eq!(
            DaemonPreface::from(DaemonAccept::select(ClientPreface::CURRENT).unwrap()).encode(),
            [b'F', b'L', b'I', b'S', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00]
        );
        assert_eq!(
            DaemonPreface::from(DaemonRefuse::CURRENT).encode(),
            [b'F', b'L', b'I', b'S', 0x00, 0x01, 0x00, 0x01, 0x00, 0x01]
        );
    }

    #[test]
    fn a_non_felis_peer_is_typed_not_guessed() {
        let err = ClientPreface::decode(b"GET /HTT").unwrap_err();
        assert_eq!(err, PrefaceError::NotFelis { magic: *b"GET " });
        let err = DaemonPreface::decode(b"SSH-2.0-op").unwrap_err();
        assert_eq!(err, PrefaceError::NotFelis { magic: *b"SSH-" });
    }

    #[test]
    fn the_effective_minor_is_the_lower_of_the_two() {
        assert_eq!(effective_minor(0, 0), 0);
        assert_eq!(effective_minor(3, 7), 3);
        assert_eq!(effective_minor(7, 3), 3);
        assert!(minor_defines(3, 3));
        assert!(minor_defines(4, 3));
        assert!(!minor_defines(2, 3));
    }

    /// The golden carrier block: two entries, one holding non-UTF-8
    /// bytes.
    fn golden_carrier() -> (CarrierBlock, Vec<u8>) {
        let block = CarrierBlock {
            env: vec![
                (b"PATH".to_vec(), b"/bin".to_vec()),
                (b"SSH_AUTH_SOCK".to_vec(), vec![0x2f, 0xff, 0xfe]),
            ],
        };
        #[rustfmt::skip]
        let bytes = vec![
            b'F', b'R', b'L', b'Y',
            0x00, 0x00, 0x00, 0x2e, // payload length, big-endian
            0x00, 0x01, // payload format version
            0x00, 0x00, 0x00, 0x02, // entry count
            0x00, 0x00, 0x00, 0x04, b'P', b'A', b'T', b'H',
            0x00, 0x00, 0x00, 0x04, b'/', b'b', b'i', b'n',
            0x00, 0x00, 0x00, 0x0d,
            b'S', b'S', b'H', b'_', b'A', b'U', b'T', b'H', b'_', b'S', b'O', b'C', b'K',
            0x00, 0x00, 0x00, 0x03, 0x2f, 0xff, 0xfe,
        ];
        (block, bytes)
    }

    fn decoded(payload: &[u8]) -> CarrierBlock {
        match CarrierBlock::decode_payload(payload).unwrap() {
            CarrierPayload::Block(block) => block,
            CarrierPayload::UnknownVersion(v) => panic!("format version {v}"),
        }
    }

    /// The frozen byte layout, pinned literally in both directions.
    #[test]
    fn the_carrier_block_matches_its_golden_vector() {
        let (block, bytes) = golden_carrier();
        assert_eq!(block.encode().unwrap(), bytes);
        let header: [u8; CARRIER_HEADER_LEN] = bytes[..CARRIER_HEADER_LEN].try_into().unwrap();
        let len = carrier_payload_len(&header).unwrap() as usize;
        assert_eq!(len, bytes.len() - CARRIER_HEADER_LEN);
        assert_eq!(decoded(&bytes[CARRIER_HEADER_LEN..]), block);
    }

    /// An empty snapshot is a well-formed block, not an omitted one.
    #[test]
    fn an_empty_carrier_block_is_well_formed() {
        let block = CarrierBlock::default();
        let bytes = block.encode().unwrap();
        assert_eq!(
            bytes,
            [b'F', b'R', b'L', b'Y', 0, 0, 0, 6, 0, 1, 0, 0, 0, 0],
            "header plus the format version and a zero entry count"
        );
        assert_eq!(decoded(&bytes[CARRIER_HEADER_LEN..]), block);
    }

    /// A payload whose version this build has no decoder for is
    /// reported as skipped, not as an error: the length word already
    /// consumed it and the client preface follows.
    #[test]
    fn an_unknown_format_version_is_reported_as_skippable() {
        let mut payload = vec![0x00, 0x09];
        payload.extend_from_slice(b"whatever this format says");
        assert_eq!(
            CarrierBlock::decode_payload(&payload).unwrap(),
            CarrierPayload::UnknownVersion(9)
        );
    }

    /// Version 0 is malformed, which is what makes a writer from before
    /// the version word fail fast: its entry count's high half lands in
    /// the version word.
    #[test]
    fn a_versionless_payload_is_refused_as_version_zero() {
        let mut versionless = Vec::new();
        versionless.extend_from_slice(&1u32.to_be_bytes());
        versionless.extend_from_slice(&4u32.to_be_bytes());
        versionless.extend_from_slice(b"PATH");
        versionless.extend_from_slice(&4u32.to_be_bytes());
        versionless.extend_from_slice(b"/bin");
        assert_eq!(
            CarrierBlock::decode_payload(&versionless).unwrap_err(),
            CarrierError::ZeroVersion
        );
        assert_eq!(
            CarrierBlock::decode_payload(&[0x00, 0x00]).unwrap_err(),
            CarrierError::ZeroVersion
        );
    }

    /// The other skew direction: a reader that predates the version
    /// word reads it as the high half of the entry count and closes on
    /// the frozen cap before it touches an entry.
    #[test]
    fn a_versioned_payload_breaks_the_entry_cap_a_versionless_reader_applies() {
        fn versionless_count(payload: &[u8]) -> u32 {
            u32::from_be_bytes(payload[..4].try_into().unwrap())
        }

        let payload = &golden_carrier().1[CARRIER_HEADER_LEN..];
        assert!(
            versionless_count(payload) > MAX_CARRIER_ENTRIES,
            "a versionless reader must refuse this payload on the entry cap"
        );
    }

    /// A bare `FLIS` stream is told apart from a carrier block by magic.
    #[test]
    fn a_bare_preface_is_not_a_carrier_block() {
        let mut header = [0u8; CARRIER_HEADER_LEN];
        header.copy_from_slice(&ClientPreface::CURRENT.encode());
        assert_eq!(
            carrier_payload_len(&header).unwrap_err(),
            CarrierError::NotCarrier { magic: MAGIC }
        );
    }

    /// An overrunning length, a payload ending mid-entry, and trailing
    /// bytes are each rejected.
    #[test]
    fn malformed_carrier_payloads_are_rejected() {
        let (_, bytes) = golden_carrier();
        let payload = &bytes[CARRIER_HEADER_LEN..];

        let mut overrun = payload.to_vec();
        overrun[6..10].copy_from_slice(&0xFFFF_u32.to_be_bytes());
        assert_eq!(
            CarrierBlock::decode_payload(&overrun).unwrap_err(),
            CarrierError::Truncated
        );

        let truncated = &payload[..payload.len() - 1];
        assert_eq!(
            CarrierBlock::decode_payload(truncated).unwrap_err(),
            CarrierError::Truncated
        );

        assert_eq!(
            CarrierBlock::decode_payload(&[0, 1, 0, 0, 0, 1]).unwrap_err(),
            CarrierError::Truncated
        );

        assert_eq!(
            CarrierBlock::decode_payload(&[0, 1]).unwrap_err(),
            CarrierError::Truncated
        );

        let mut trailing = payload.to_vec();
        trailing.push(0);
        assert_eq!(
            CarrierBlock::decode_payload(&trailing).unwrap_err(),
            CarrierError::TrailingBytes(1)
        );
    }

    /// Both caps are enforced on send and on read, a lying length word
    /// included.
    #[test]
    fn the_frozen_caps_are_enforced_in_both_directions() {
        let too_many = CarrierBlock {
            env: (0..=MAX_CARRIER_ENTRIES)
                .map(|i| (i.to_string().into_bytes(), Vec::new()))
                .collect(),
        };
        assert_eq!(
            too_many.encode().unwrap_err(),
            CarrierError::OverCap {
                found: MAX_CARRIER_ENTRIES + 1,
                cap: MAX_CARRIER_ENTRIES,
            }
        );

        let too_big = CarrierBlock {
            env: vec![(
                b"BIG".to_vec(),
                vec![b'x'; MAX_CARRIER_PAYLOAD_BYTES as usize],
            )],
        };
        assert!(matches!(
            too_big.encode().unwrap_err(),
            CarrierError::OverCap {
                cap: MAX_CARRIER_PAYLOAD_BYTES,
                ..
            }
        ));

        let mut header = [0u8; CARRIER_HEADER_LEN];
        header[..4].copy_from_slice(&CARRIER_MAGIC);
        header[4..].copy_from_slice(&(MAX_CARRIER_PAYLOAD_BYTES + 1).to_be_bytes());
        assert_eq!(
            carrier_payload_len(&header).unwrap_err(),
            CarrierError::OverCap {
                found: MAX_CARRIER_PAYLOAD_BYTES + 1,
                cap: MAX_CARRIER_PAYLOAD_BYTES,
            }
        );

        let mut over_count = CARRIER_FORMAT_VERSION.to_be_bytes().to_vec();
        over_count.extend_from_slice(&(MAX_CARRIER_ENTRIES + 1).to_be_bytes());
        assert_eq!(
            CarrierBlock::decode_payload(&over_count).unwrap_err(),
            CarrierError::OverCap {
                found: MAX_CARRIER_ENTRIES + 1,
                cap: MAX_CARRIER_ENTRIES,
            }
        );
    }

    /// A carrier refusal's rendered text carries no environment name or
    /// value (`docs/explanation/security-model.md`).
    #[test]
    fn a_carrier_refusal_names_no_environment_content() {
        let mut trailing = golden_carrier().1[CARRIER_HEADER_LEN..].to_vec();
        trailing.push(0);
        for err in [
            CarrierBlock::decode_payload(&trailing).unwrap_err(),
            CarrierBlock::decode_payload(&[0, 1, 0, 0, 0, 1]).unwrap_err(),
            CarrierBlock::decode_payload(&[0, 0]).unwrap_err(),
            CarrierError::OverCap {
                found: 9,
                cap: MAX_CARRIER_ENTRIES,
            },
        ] {
            let rendered = err.to_string();
            assert!(!rendered.contains("PATH"), "{rendered}");
            assert!(!rendered.contains("SSH_AUTH_SOCK"), "{rendered}");
            assert!(!rendered.contains("/bin"), "{rendered}");
        }
    }

    /// The protocol constants match what `felis.proto` declares.
    #[test]
    fn the_protocol_version_matches_the_schema() {
        let schema = include_str!("../proto/felis.proto");
        let declared = |name: &str| -> u16 {
            let line = schema
                .lines()
                .filter_map(|l| Some(l.trim_start().strip_prefix("//")?.trim_start()))
                .find_map(|l| l.strip_prefix(name))
                .unwrap_or_else(|| panic!("felis.proto must declare {name}"));
            line.trim_start()
                .strip_prefix('=')
                .expect("declared as `<NAME> = <value>`")
                .trim()
                .parse()
                .expect("a u16 version")
        };
        assert_eq!(PROTOCOL_MAJOR, declared("PROTOCOL_MAJOR"));
        assert_eq!(PROTOCOL_MINOR, declared("PROTOCOL_MINOR"));
    }

    /// Every minor from 0 to the current one has exactly one ledger
    /// entry, and the reference doc's ledger table lists the same set of
    /// minors in the same order: a bump recorded in one place but not
    /// the other is the drift this test exists to catch.
    #[test]
    fn the_minor_ledger_matches_the_reference_doc() {
        let minors: Vec<u16> = MINOR_LEDGER.iter().map(|(minor, _)| *minor).collect();
        assert_eq!(minors, (0..=PROTOCOL_MINOR).collect::<Vec<_>>());

        // Read at test time rather than `include_str!`: the doc lives
        // outside the crate, and a compile-time include would make the
        // crate unpackageable on its own.
        let doc = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../docs/reference/ipc.md"
        ))
        .expect("docs/reference/ipc.md beside the workspace");
        let table = doc
            .split("### The minor ledger")
            .nth(1)
            .expect("ipc.md has a \"The minor ledger\" section");
        let documented: Vec<u16> = table
            .lines()
            .skip_while(|l| !l.starts_with("| Minor |"))
            .skip(2)
            .take_while(|l| l.starts_with('|'))
            .map(|row| {
                row.trim_start_matches('|')
                    .split('|')
                    .next()
                    .expect("a table row has a first cell")
                    .trim()
                    .parse()
                    .expect("the ledger's first column is the minor")
            })
            .collect();
        assert_eq!(
            documented, minors,
            "the minor ledger table in docs/reference/ipc.md and MINOR_LEDGER disagree"
        );
    }

    #[test]
    fn this_build_serves_exactly_its_own_major() {
        assert!(supports_major(PROTOCOL_MAJOR));
        assert!(!supports_major(PROTOCOL_MAJOR - 1));
        assert!(!supports_major(PROTOCOL_MAJOR + 1));
    }

    /// The selection table: an offered major maps to the accept that
    /// echoes it with this build's minor for it, or to no reply; the
    /// advertised range in a refusal names exactly the majors `select`
    /// accepts.
    #[test]
    fn selection_echoes_the_offered_major_with_its_own_minor() {
        for (offered, want) in [
            (
                PROTOCOL_MAJOR,
                Some(DaemonAccept {
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR,
                }),
            ),
            (PROTOCOL_MAJOR - 1, None),
            (PROTOCOL_MAJOR + 1, None),
            (u16::MAX, None),
        ] {
            let client = ClientPreface {
                major: offered,
                minor: 0,
            };
            assert_eq!(DaemonAccept::select(client), want, "offered {offered}");
        }
        for major in 0..=u16::MAX {
            let in_range = (SUPPORTED_MAJOR_MIN..=SUPPORTED_MAJOR_MAX).contains(&major);
            assert_eq!(
                supports_major(major),
                in_range,
                "the refusal's advertised range must match what select serves (major {major})"
            );
        }
        let client_minor_is_irrelevant = ClientPreface {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR + 7,
        };
        assert_eq!(
            DaemonAccept::select(client_minor_is_irrelevant),
            Some(DaemonAccept {
                major: PROTOCOL_MAJOR,
                minor: PROTOCOL_MINOR,
            }),
            "the daemon advertises its own minor, never the client's"
        );
    }

    /// Negotiation is exact where decoding is lenient: an accept that
    /// echoes the offer yields the effective minor, an accept naming
    /// any other major is its own error class, and both refusals keep
    /// their words.
    #[test]
    fn confirm_accept_takes_only_the_offered_major() {
        let offered = ClientPreface::CURRENT;
        assert_eq!(
            confirm_accept(
                offered,
                DaemonPreface::Accept {
                    major: PROTOCOL_MAJOR,
                    minor: PROTOCOL_MINOR + 3,
                },
            ),
            Ok(PROTOCOL_MINOR),
            "a newer daemon minor clamps to the client's"
        );
        assert_eq!(
            confirm_accept(
                offered,
                DaemonPreface::Accept {
                    major: PROTOCOL_MAJOR + 8,
                    minor: PROTOCOL_MINOR,
                },
            ),
            Err(NegotiationError::AcceptedUnofferedMajor {
                offered: PROTOCOL_MAJOR,
                accepted: PROTOCOL_MAJOR + 8,
            }),
        );
        assert_eq!(
            confirm_accept(
                offered,
                DaemonPreface::Refuse {
                    min_major: 2,
                    max_major: 3,
                },
            ),
            Err(NegotiationError::Refused { min: 2, max: 3 }),
        );
        assert_eq!(
            confirm_accept(
                offered,
                DaemonPreface::Unknown {
                    status: 7,
                    words: [9, 9],
                },
            ),
            Err(NegotiationError::Unknown {
                status: 7,
                words: [9, 9],
            }),
        );
    }
}
