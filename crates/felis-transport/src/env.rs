//! The addressing variables the daemon writes into every session's child
//! environment and reserves against caller overrides
//! (docs/reference/terminal-identity.md).

/// The owning session's id, as 32 hex digits.
pub const SESSION_ID_ENV: &str = "FELIS_SESSION_ID";

/// The spawning daemon's endpoint; absent when that daemon serves none.
pub const SOCKET_ENV: &str = "FELIS_SOCKET";
