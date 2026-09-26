//! OS-clipboard implementation backed by `arboard`.
//!
//! Writes are security-gated against unauthorized `OSC 52` clipboard manipulation.
//! If `arboard` initialization fails, the application falls back to `InMemoryClipboard`.

#![allow(unreachable_pub)]

use std::sync::Mutex;

use arboard::Clipboard as Arboard;
use felis_client_core::{Clipboard, ClipboardError};

pub struct OsClipboard {
    /// `arboard::Clipboard` is `!Sync` (Wayland's data-source proxy holds
    /// non-thread-safe state).
    inner: Mutex<Arboard>,
    /// When false, `write` is a no-op. Reads always hit the system
    /// clipboard: a user-initiated paste is never a hostile-program concern.
    allow_program_writes: bool,
}

impl OsClipboard {
    pub fn new(allow_program_writes: bool) -> Result<Self, ClipboardError> {
        let inner = Arboard::new().map_err(|e| ClipboardError::Unavailable(e.to_string()))?;
        Ok(Self {
            inner: Mutex::new(inner),
            allow_program_writes,
        })
    }

    /// Mutex poisoning becomes a `ClipboardError` instead of a panic.
    fn guard(&self) -> Result<std::sync::MutexGuard<'_, Arboard>, ClipboardError> {
        self.inner
            .lock()
            .map_err(|e| ClipboardError::Backend(format!("poisoned: {e}")))
    }

    /// `utf8_err` labels the payload kind in the error message.
    fn write_to_system(&self, bytes: &[u8], utf8_err: &str) -> Result<(), ClipboardError> {
        let text = std::str::from_utf8(bytes)
            .map_err(|e| ClipboardError::Backend(format!("{utf8_err}: {e}")))?;
        let mut guard = self.guard()?;
        guard
            .set_text(text)
            .map_err(|e| ClipboardError::Backend(e.to_string()))
    }
}

impl Clipboard for OsClipboard {
    fn read(&self) -> Result<Vec<u8>, ClipboardError> {
        let mut guard = self.guard()?;
        // arboard returns `Err` for "clipboard is empty"; an empty Vec lets
        // `paste_from_clipboard` short-circuit as "nothing gets typed".
        match guard.get_text() {
            Ok(text) => Ok(text.into_bytes()),
            Err(arboard::Error::ContentNotAvailable) => Ok(Vec::new()),
            Err(e) => Err(ClipboardError::Backend(e.to_string())),
        }
    }

    fn write(&self, bytes: &[u8]) -> Result<(), ClipboardError> {
        if !self.allow_program_writes {
            return Ok(());
        }
        self.write_to_system(bytes, "OSC 52 payload not UTF-8")
    }

    fn write_user(&self, bytes: &[u8]) -> Result<(), ClipboardError> {
        // Always honored: the OSC 52 gate is hostile-program protection
        // only, and the default `osc_52 = "mirror"` would otherwise drop
        // every Ctrl+Shift+C.
        self.write_to_system(bytes, "selection payload not UTF-8")
    }

    #[cfg(target_os = "linux")]
    fn write_primary(&self, bytes: &[u8]) -> Result<(), ClipboardError> {
        // X11 / Wayland PRIMARY (auto-copy on selection, middle-click
        // paste); macOS / Windows have no PRIMARY and keep the trait
        // default no-op.
        use arboard::{LinuxClipboardKind, SetExtLinux};
        let text = std::str::from_utf8(bytes)
            .map_err(|e| ClipboardError::Backend(format!("PRIMARY payload not UTF-8: {e}")))?;
        let mut guard = self.guard()?;
        guard
            .set()
            .clipboard(LinuxClipboardKind::Primary)
            .text(text.to_owned())
            .map_err(|e| ClipboardError::Backend(e.to_string()))
    }

    #[cfg(target_os = "linux")]
    fn write_primary_program(&self, bytes: &[u8]) -> Result<(), ClipboardError> {
        if !self.allow_program_writes {
            // A program's OSC 52 `p` write must not seed PRIMARY (the
            // user's next middle-click paste) with attacker bytes; the
            // user-driven chord uses `write_primary`, which stays ungated.
            return Ok(());
        }
        self.write_primary(bytes)
    }

    #[cfg(target_os = "linux")]
    fn read_primary(&self) -> Result<Vec<u8>, ClipboardError> {
        // arboard returns ContentNotAvailable for an empty selection; an
        // empty payload lets the paste path short-circuit quietly.
        use arboard::{GetExtLinux, LinuxClipboardKind};
        let mut guard = self.guard()?;
        match guard.get().clipboard(LinuxClipboardKind::Primary).text() {
            Ok(text) => Ok(text.into_bytes()),
            Err(arboard::Error::ContentNotAvailable) => Ok(Vec::new()),
            Err(e) => Err(ClipboardError::Backend(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The OS clipboard / PRIMARY selection is one shared global surface,
    /// so tests touching it race on nextest's thread pool.
    static CLIPBOARD_LOCK: Mutex<()> = Mutex::new(());

    /// A poisoned lock still serializes correctly.
    fn clipboard_guard() -> std::sync::MutexGuard<'static, ()> {
        CLIPBOARD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// arboard requires a real clipboard surface; CI runners don't have
    /// one, so construction failure is a skip-and-pass.
    fn os_clipboard_or_skip(allow_writes: bool) -> Option<OsClipboard> {
        match OsClipboard::new(allow_writes) {
            Ok(cb) => Some(cb),
            Err(err) => {
                eprintln!("OsClipboard unavailable in this environment ({err}); skipping");
                None
            }
        }
    }

    #[test]
    fn write_is_dropped_when_program_writes_disallowed() {
        // With allow_program_writes=false, the OSC 52 drain's `write()`
        // must not reach the OS clipboard.
        let _guard = clipboard_guard();
        let Some(cb) = os_clipboard_or_skip(false) else {
            return;
        };
        // Skip (not pass vacuously) when read() itself fails: with both
        // sides defaulting to empty, the assert would pass even if the
        // write landed.
        let Ok(before) = cb.read() else {
            eprintln!("clipboard read unavailable in this environment; skipping");
            return;
        };
        cb.write(b"hostile-program-payload").unwrap();
        let after = cb.read().unwrap_or_default();
        assert_eq!(
            before, after,
            "gate=false must leave the system clipboard untouched"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn read_primary_round_trips_a_write_primary() {
        // Without the round-trip, middle-click pastes nothing even when
        // the user just selected text in felis.
        let _guard = clipboard_guard();
        let Some(cb) = os_clipboard_or_skip(false) else {
            return;
        };
        let primary_before = cb.read_primary().unwrap_or_default();
        let payload: &[u8] = b"middle-click-paste-payload";
        cb.write_primary(payload).unwrap();
        let read_back = cb.read_primary().unwrap();
        // Best-effort restore of the dev hardware's PRIMARY.
        drop(cb.write_primary(&primary_before));
        assert_eq!(
            read_back, payload,
            "write_primary → read_primary must round-trip"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_primary_program_is_dropped_when_program_writes_disallowed() {
        // The OSC 52 `p` target obeys the same hostile-program gate as `c`.
        let _guard = clipboard_guard();
        let Some(cb) = os_clipboard_or_skip(false) else {
            return;
        };
        let before = cb.read_primary().unwrap_or_default();
        cb.write_primary_program(b"hostile-primary-payload")
            .unwrap();
        let after = cb.read_primary().unwrap_or_default();
        assert_eq!(
            before, after,
            "gate=false must leave PRIMARY untouched for a program write"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_primary_program_lands_when_program_writes_allowed() {
        // The mirror of `write` propagating when allow_program_writes=true.
        let _guard = clipboard_guard();
        let Some(cb) = os_clipboard_or_skip(true) else {
            return;
        };
        let before = cb.read_primary().unwrap_or_default();
        let payload: &[u8] = b"allowed-primary-osc52";
        if let Err(err) = cb.write_primary_program(payload) {
            eprintln!("OsClipboard PRIMARY unavailable here ({err}); skipping");
            return;
        }
        let after = cb.read_primary().unwrap();
        drop(cb.write_primary(&before));
        assert_eq!(
            after, payload,
            "gate=true must let a program OSC 52 `p` write land on PRIMARY"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_primary_lands_on_x11_wayland_primary_selection() {
        // A write_primary must land on arboard's Primary, independently of
        // the regular clipboard.
        let _guard = clipboard_guard();
        let Some(cb) = os_clipboard_or_skip(false) else {
            return;
        };
        let regular_before = cb.read().unwrap_or_default();
        let primary_before = {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            let mut g = cb.inner.lock().unwrap();
            g.get()
                .clipboard(LinuxClipboardKind::Primary)
                .text()
                .map(String::into_bytes)
                .unwrap_or_default()
        };
        let payload: &[u8] = b"primary-auto-copy";
        cb.write_primary(payload).unwrap();
        // PRIMARY must hold the payload, NOT the regular clipboard's content.
        let primary_after = {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            let mut g = cb.inner.lock().unwrap();
            g.get()
                .clipboard(LinuxClipboardKind::Primary)
                .text()
                .map(String::into_bytes)
                .unwrap_or_default()
        };
        let regular_after = cb.read().unwrap_or_default();
        // Best-effort restore of the user's selections on dev hardware.
        {
            use arboard::{LinuxClipboardKind, SetExtLinux};
            let mut g = cb.inner.lock().unwrap();
            drop(
                g.set()
                    .clipboard(LinuxClipboardKind::Primary)
                    .text(String::from_utf8_lossy(&primary_before).into_owned()),
            );
        }
        drop(cb.write_user(&regular_before));
        assert_eq!(
            primary_after, payload,
            "PRIMARY must receive the auto-copy payload"
        );
        assert_eq!(
            regular_after, regular_before,
            "PRIMARY auto-copy must not touch the regular clipboard"
        );
    }

    #[test]
    fn write_user_bypasses_the_program_writes_gate() {
        // Even with allow_program_writes=false, a user-driven copy
        // (Ctrl+Shift+C) lands on the system clipboard.
        let _guard = clipboard_guard();
        let Some(cb) = os_clipboard_or_skip(false) else {
            return;
        };
        let before = cb.read().unwrap_or_default();
        let payload: &[u8] = b"user-driven-copy-via-ctrl-shift-c";
        // `Arboard::new` succeeds lazily on Windows without opening the
        // clipboard, so `os_clipboard_or_skip` cannot detect a window
        // station that forbids clipboard access (a test binary launched
        // outside an interactive session); the first real operation's
        // failure is the same skip.
        if let Err(err) = cb.write_user(payload) {
            eprintln!("OsClipboard write unavailable in this environment ({err}); skipping");
            return;
        }
        let after = cb.read().unwrap_or_default();
        // Best-effort restore of the user's clipboard on dev hardware.
        drop(cb.write_user(&before));
        assert_eq!(
            after, payload,
            "write_user must bypass the OSC 52 gate so Ctrl+Shift+C actually copies"
        );
    }
}
