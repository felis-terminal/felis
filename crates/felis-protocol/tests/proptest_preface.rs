//! Property-based version-preface invariants: decoding is total and
//! classifies a reply from the status word alone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_protocol::preface::{
    CLIENT_PREFACE_LEN, ClientPreface, DAEMON_PREFACE_LEN, DaemonPreface, MAGIC, PrefaceError,
    effective_minor,
};
use proptest::prelude::*;

proptest! {
    /// Arbitrary 10-byte replies never panic.
    #[test]
    fn decoding_an_arbitrary_daemon_reply_never_panics(bytes in any::<[u8; DAEMON_PREFACE_LEN]>()) {
        match DaemonPreface::decode(&bytes) {
            Ok(_) => prop_assert_eq!(&bytes[..4], &MAGIC[..]),
            Err(PrefaceError::NotFelis { magic }) => {
                prop_assert_ne!(magic, MAGIC);
                prop_assert_eq!(&magic[..], &bytes[..4]);
            }
        }
    }

    /// Arbitrary 8-byte client prefaces never panic.
    #[test]
    fn decoding_an_arbitrary_client_preface_never_panics(
        bytes in any::<[u8; CLIENT_PREFACE_LEN]>(),
    ) {
        match ClientPreface::decode(&bytes) {
            Ok(p) => {
                prop_assert_eq!(&bytes[..4], &MAGIC[..]);
                prop_assert_eq!(p.major, u16::from_be_bytes([bytes[4], bytes[5]]));
                prop_assert_eq!(p.minor, u16::from_be_bytes([bytes[6], bytes[7]]));
            }
            Err(PrefaceError::NotFelis { magic }) => prop_assert_ne!(magic, MAGIC),
        }
    }

    /// The status word alone decides the class, whatever the payload
    /// words hold.
    #[test]
    fn the_status_word_alone_decides_the_class(
        status in any::<u16>(),
        word1 in any::<u16>(),
        word2 in any::<u16>(),
    ) {
        let mut bytes = [0u8; DAEMON_PREFACE_LEN];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4..6].copy_from_slice(&status.to_be_bytes());
        bytes[6..8].copy_from_slice(&word1.to_be_bytes());
        bytes[8..10].copy_from_slice(&word2.to_be_bytes());
        let decoded = DaemonPreface::decode(&bytes).unwrap();
        match status {
            0 => prop_assert_eq!(decoded, DaemonPreface::Accept { major: word1, minor: word2 }),
            1 => prop_assert_eq!(
                decoded,
                DaemonPreface::Refuse { min_major: word1, max_major: word2 }
            ),
            other => prop_assert_eq!(
                decoded,
                DaemonPreface::Unknown { status: other, words: [word1, word2] }
            ),
        }
    }

    /// Every decodable reply re-encodes to the same bytes, `Unknown`
    /// included, so a proxy can forward a status it does not understand.
    #[test]
    fn a_decodable_reply_re_encodes_to_the_same_bytes(
        status in any::<u16>(),
        word1 in any::<u16>(),
        word2 in any::<u16>(),
    ) {
        let mut bytes = [0u8; DAEMON_PREFACE_LEN];
        bytes[..4].copy_from_slice(&MAGIC);
        bytes[4..6].copy_from_slice(&status.to_be_bytes());
        bytes[6..8].copy_from_slice(&word1.to_be_bytes());
        bytes[8..10].copy_from_slice(&word2.to_be_bytes());
        prop_assert_eq!(DaemonPreface::decode(&bytes).unwrap().encode(), bytes);
    }

    /// The client half round-trips over the whole version space.
    #[test]
    fn a_client_preface_round_trips(major in any::<u16>(), minor in any::<u16>()) {
        let p = ClientPreface { major, minor };
        prop_assert_eq!(ClientPreface::decode(&p.encode()).unwrap(), p);
    }

    /// The effective minor is the lower of the two and symmetric.
    #[test]
    fn the_effective_minor_is_the_symmetric_lower_bound(a in any::<u16>(), b in any::<u16>()) {
        let m = effective_minor(a, b);
        prop_assert_eq!(m, effective_minor(b, a));
        prop_assert!(m <= a && m <= b);
        prop_assert!(m == a || m == b);
    }
}
