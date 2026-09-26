//! Byte ceilings on the input path (`docs/reference/ipc.md` "Backpressure").
//!
//! Bounds unwritten input held by client send queues and daemon admission.

/// Unwritten input bytes one session admits from its connections
/// before the pump that carries them stops reading its socket. Large
/// enough that a person cannot type or paste past it, small enough
/// that a stalled child costs one session's worth of memory.
pub const PTY_INPUT_BUDGET: usize = 16 * 1024 * 1024;

/// `ESC [ 200~` + `ESC [ 201~`: what bracketed-paste mode adds around a
/// payload. Reserved for every paste regardless of mode, because the
/// mode is read in the session actor and the child can flip it between
/// admission and the write.
pub const PASTE_BRACKET_OVERHEAD: usize = 12;

/// The largest paste payload the daemon accepts. Pinned below the
/// budget with room for the brackets: a reservation larger than the
/// budget could never be granted, so an oversized paste would wedge the
/// connection's pump instead of being refused.
pub const MAX_PASTE_BYTES: usize = PTY_INPUT_BUDGET - 64;

const _: () = assert!(MAX_PASTE_BYTES + PASTE_BRACKET_OVERHEAD <= PTY_INPUT_BUDGET);

/// Maximum bytes for an encoded mouse report (up to 19 bytes for SGR, 6 for X10).
///
/// Reserved whole for every `InputMsg::Mouse` because the child process can
/// flip mouse protocol mode dynamically between admission and write.
pub const MAX_MOUSE_REPORT_BYTES: usize = 32;

/// Maximum UTF-8 bytes a [`Key::Character`](crate::messages::Key) may
/// carry: one key's own character, which a dead-key or IME chain can
/// compose out of several codepoints but never out of a sentence.
/// Anything longer is `Paste` or `KeyBytes` territory.
pub const MAX_KEY_CHARACTER_BYTES: usize = 32;

/// Maximum UTF-8 bytes a [`KeyEvent::text`](crate::messages::KeyEvent)
/// may carry, sized as [`MAX_KEY_CHARACTER_BYTES`]: the OS-composed
/// text of one keystroke.
pub const MAX_KEY_TEXT_BYTES: usize = MAX_KEY_CHARACTER_BYTES;

/// What a key report costs besides its text: `ESC [`, two 7-digit
/// keycodes, a 2-digit modifier field, a 1-digit event type, their
/// separators and the final `u`
/// (`docs/reference/protocols/key-encoding.md` "Report size").
const KEY_REPORT_FIXED_BYTES: usize = 2 + 7 + 1 + 7 + 1 + 2 + 1 + 1 + 1 + 1;

/// Maximum bytes an encoded key report reaches in any keyboard mode,
/// reserved whole per `InputMsg::Key` because the child can flip modes
/// between admission and the write. The widest form is Kitty CSI u with
/// alternate keys and associated text, which spells each text codepoint
/// as at most 7 digits plus a separator.
pub const MAX_KEY_REPORT_BYTES: usize = KEY_REPORT_FIXED_BYTES + 8 * MAX_KEY_TEXT_BYTES;

const _: () = assert!(MAX_KEY_REPORT_BYTES < PTY_INPUT_BUDGET);

/// Backlog a window may hold on top of one largest single message
/// before it declares the carrier dead.
pub const CLIENT_BACKLOG_HEADROOM: usize = 4 * 1024 * 1024;

/// Unwritten client-to-daemon bytes a window queues before declaring carrier dead.
///
/// Clears the largest acceptable message plus headroom so a valid max-size
/// paste does not abort the connection (`docs/explanation/architecture/session-lifecycle.md`).
pub const CLIENT_OUTGOING_CAP: usize =
    MAX_PASTE_BYTES + PASTE_BRACKET_OVERHEAD + CLIENT_BACKLOG_HEADROOM;

const _: () = assert!(CLIENT_OUTGOING_CAP > MAX_PASTE_BYTES + PASTE_BRACKET_OVERHEAD);
