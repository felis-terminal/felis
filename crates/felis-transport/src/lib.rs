//! Wire framing and carriers (Unix socket, Windows named pipe, SSH stdio).
//!
//! Enforces security invariants per `docs/explanation/security-model.md`.
//! The OS-specific local carrier stays behind the [`local`] facade
//! exposing `local::{ReadHalf, WriteHalf}`.

// `deny`, not `forbid`: `peer::peer_uid` opts back in with a fn-scoped
// allow for `getpeereid(3)`, which `forbid` would reject.
#![cfg_attr(all(not(test), not(windows)), deny(unsafe_code))]
#![cfg_attr(windows, allow(unsafe_code))]

pub mod driver;
pub mod framing;
pub mod local;
pub mod logging;
pub mod peer;
pub mod preface;
pub mod retry;
pub mod socket;
pub mod stdio;
#[cfg(unix)]
pub(crate) mod unix;
#[cfg(windows)]
pub(crate) mod windows;

pub use driver::{
    ClientDriver, ClientSide, ConnectionDriver, DaemonDriver, DaemonSide, Delivered, Delivery,
    DriverError, Incoming, MAX_OUTSTANDING_STREAMS, Payload, Phase, Role, Side, StreamClass,
};
pub use framing::{CheckedFrame, FrameReader, FrameWriter, OwnedFrame, TransportError};
pub use local::{Endpoint, Listener, connect, server_split};
pub use preface::PrefaceExchangeError;
pub use retry::{RetryError, RetryPolicy, retry_while, retry_with_backoff};
pub use stdio::{StdioSession, spawn_command};
