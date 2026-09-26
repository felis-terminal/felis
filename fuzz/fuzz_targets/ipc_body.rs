//! IPC body decoder fuzz target.
//!
//! `ipc_frame` covers the outer frame envelope; this target covers the
//! other half of the socket trust boundary: the body decode the daemon
//! runs on every framed payload a connected peer sends — prost into the
//! generated wire types, then the wire->domain conversion layer
//! (`felis_protocol::convert`). A panic anywhere in that stack lets a
//! malicious local peer crash the daemon.
//!
//! For any input, every wrapper's decode must return `Ok` or a typed
//! `CodecError` — never panic — and a body it accepts must re-encode (an
//! accepted value is a real domain value, so encoding it is infallible).
//! The arm walk the driver runs over the same bytes, looking for the
//! other direction's arms, holds to the same rule.

#![no_main]

use felis_protocol::MessageKind;
use felis_protocol::codec;
use felis_protocol::messages::{
    ConnToClientMsg, ConnToDaemonMsg, Direction, GridMsg, ImageMsg, InputMsg, NotifyToClientMsg,
    NotifyToDaemonMsg, OpsToClientMsg, OpsToDaemonMsg, PushMsg, RegionToClientMsg,
    RegionToDaemonMsg, SearchToClientMsg, SearchToDaemonMsg, SessionToClientMsg,
    SessionToDaemonMsg,
};
use libfuzzer_sys::fuzz_target;

// One decode attempt per wrapper over the same bytes: the wrappers share
// the helper paths where the bugs would live (session ids, enum
// sentinels, integer narrowing), so cross-decoding a corpus item
// multiplies coverage at negligible cost.
macro_rules! probe {
    ($body:expr, $($family:ty),+ $(,)?) => {
        $(
            if let Ok(msg) = codec::decode::<$family>($body) {
                let _ = codec::encode(&msg);
            }
        )+
    };
}

fuzz_target!(|data: &[u8]| {
    probe!(
        data,
        ConnToDaemonMsg,
        ConnToClientMsg,
        InputMsg,
        GridMsg,
        ImageMsg,
        SessionToDaemonMsg,
        SessionToClientMsg,
        OpsToDaemonMsg,
        OpsToClientMsg,
        RegionToDaemonMsg,
        RegionToClientMsg,
        NotifyToDaemonMsg,
        NotifyToClientMsg,
        PushMsg,
        SearchToDaemonMsg,
        SearchToClientMsg,
    );
    for kind in (0..=u16::MAX).map_while(MessageKind::from_u16) {
        for direction in [Direction::ToDaemon, Direction::ToClient] {
            let _ = codec::arm_in(kind, direction, data);
        }
    }
});
