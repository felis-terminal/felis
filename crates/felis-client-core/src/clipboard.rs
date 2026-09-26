//! Clipboard trait; `felis-client` supplies the OS-specific impls, which
//! pull in display-server crates this crate must avoid.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ClipboardError {
    #[error("clipboard unavailable: {0}")]
    Unavailable(String),
    /// The string is for logs; matching on it is unsupported.
    #[error("clipboard backend error: {0}")]
    Backend(String),
}

/// Payloads are UTF-8 bytes; impls do any platform charset translation.
///
/// Each surface has a program-driven write (OSC 52), which an impl may
/// gate through the user's `clipboard.osc_52` dial, and a user-driven
/// write (a copy chord), which is always honored.
pub trait Clipboard: Send + Sync {
    fn read(&self) -> Result<Vec<u8>, ClipboardError>;
    /// Program-driven; may be gated.
    fn write(&self, bytes: &[u8]) -> Result<(), ClipboardError>;
    /// User-driven; always honored.
    fn write_user(&self, bytes: &[u8]) -> Result<(), ClipboardError> {
        self.write(bytes)
    }
    /// User-driven write to the X11 / Wayland PRIMARY selection; a no-op
    /// where there is no PRIMARY. A failure must not abort the user's
    /// selection gesture.
    fn write_primary(&self, _bytes: &[u8]) -> Result<(), ClipboardError> {
        Ok(())
    }
    /// Program-driven (`OSC 52` with the `p` target); may be gated.
    fn write_primary_program(&self, bytes: &[u8]) -> Result<(), ClipboardError> {
        self.write_primary(bytes)
    }
    /// Empty where there is no PRIMARY, so middle-click pastes nothing
    /// there, matching native terminals.
    fn read_primary(&self) -> Result<Vec<u8>, ClipboardError> {
        Ok(Vec::new())
    }
}

/// For tests and the headless path (no display server).
#[derive(Debug, Default)]
pub struct InMemoryClipboard {
    cell: std::sync::Mutex<Vec<u8>>,
}

impl InMemoryClipboard {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Clipboard for InMemoryClipboard {
    fn read(&self) -> Result<Vec<u8>, ClipboardError> {
        self.cell
            .lock()
            .map(|guard| guard.clone())
            .map_err(|e| ClipboardError::Backend(format!("poisoned: {e}")))
    }
    fn write(&self, bytes: &[u8]) -> Result<(), ClipboardError> {
        let mut guard = self
            .cell
            .lock()
            .map_err(|e| ClipboardError::Backend(format!("poisoned: {e}")))?;
        *guard = bytes.to_vec();
        drop(guard);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_replaces_previous_contents() {
        let cb = InMemoryClipboard::new();
        cb.write(b"first").unwrap();
        cb.write(b"second").unwrap();
        assert_eq!(cb.read().unwrap(), b"second");
    }

    #[test]
    fn fresh_clipboard_reads_empty() {
        let cb = InMemoryClipboard::new();
        assert_eq!(cb.read().unwrap(), b"");
    }

    #[test]
    fn clipboard_is_object_safe() {
        let cb: Box<dyn Clipboard> = Box::new(InMemoryClipboard::new());
        cb.write(b"x").unwrap();
        assert_eq!(cb.read().unwrap(), b"x");
    }

    #[test]
    fn write_user_default_routes_through_write() {
        let cb = InMemoryClipboard::new();
        cb.write_user(b"selection").unwrap();
        assert_eq!(cb.read().unwrap(), b"selection");
    }

    #[derive(Default)]
    struct RecordingClipboard {
        last: std::sync::Arc<std::sync::Mutex<&'static str>>,
    }
    impl Clipboard for RecordingClipboard {
        fn read(&self) -> Result<Vec<u8>, ClipboardError> {
            Ok(Vec::new())
        }
        fn write(&self, _bytes: &[u8]) -> Result<(), ClipboardError> {
            *self.last.lock().unwrap() = "write";
            Ok(())
        }
    }

    #[test]
    fn write_user_default_dispatch_observed_through_dyn() {
        let last = std::sync::Arc::new(std::sync::Mutex::new(""));
        let cb: Box<dyn Clipboard> = Box::new(RecordingClipboard { last: last.clone() });
        cb.write_user(b"x").unwrap();
        assert_eq!(*last.lock().unwrap(), "write");
    }

    #[test]
    fn write_primary_default_is_a_silent_noop() {
        let cb = InMemoryClipboard::new();
        cb.write_primary(b"would-be-primary").unwrap();
        assert_eq!(
            cb.read().unwrap(),
            b"",
            "default write_primary must not pollute the regular clipboard"
        );
    }

    #[test]
    fn read_primary_default_is_an_empty_payload() {
        let cb = InMemoryClipboard::new();
        cb.write(b"regular clipboard").unwrap();
        assert_eq!(
            cb.read_primary().unwrap(),
            b"",
            "default read_primary must not surface the regular clipboard"
        );
    }
}
