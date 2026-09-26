//! Kitty keyboard protocol flag bits.
//!
//! Documented in `docs/reference/protocols/key-encoding.md`; key encoding
//! logic is in `felis-daemon`'s `serve/key_encode.rs`.

use bitflags::bitflags;
use serde::{Deserialize, Serialize};

bitflags! {
    /// Progressive-enhancement bitmap: the active top of the daemon's
    /// per-grid push/pop stack. Empty means the legacy xterm encoding is
    /// active. Bits above [`KittyKbdFlags::all`] are reserved by the spec;
    /// `from_bits_truncate` at the wire boundary drops them.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub struct KittyKbdFlags: u8 {
        /// Bit 0: disambiguate escape codes.
        const DISAMBIGUATE           = 1 << 0;
        /// Bit 1: report event types. A release on a plain typed
        /// character has no defined encoding and is dropped.
        const REPORT_EVENT_TYPES     = 1 << 1;
        /// Bit 2: report alternate keys (`CSI keycode:alt;mod u`).
        const REPORT_ALTERNATE_KEYS  = 1 << 2;
        /// Bit 3: report all keys as escape codes. Implies bit 0.
        const REPORT_ALL_AS_ESCAPES  = 1 << 3;
        /// Bit 4: report associated text as a third parameter
        /// (`:`-joined when multi-codepoint).
        const REPORT_ASSOCIATED_TEXT = 1 << 4;
    }
}
