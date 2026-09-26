//! Confirmation state behind the two destructive gates: the
//! `kill_session` chord and the multi-line unbracketed paste (REQ-804,
//! docs/explanation/input.md "Confirmation bar"). One pending
//! question at a time; `y` / `Y` confirms, any other key cancels.

/// A chord that would arm a second one is ignored while the first is on
/// screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingConfirm {
    KillSession,
    /// The bytes are shipped verbatim on confirm.
    Paste(Vec<u8>),
}

impl PendingConfirm {
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::KillSession => {
                "Kill this session? The program running in it will be terminated. [y/N]".to_owned()
            }
            Self::Paste(bytes) => {
                let lines = paste_line_count(bytes);
                format!("Paste {lines} lines? The program did not request bracketed paste. [y/N]")
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmDecision {
    Confirm,
    Cancel,
}

/// Enter is not a confirm: the prompt interrupts typing, and the likeliest
/// in-flight keystroke is the newline that made the paste dangerous.
#[must_use]
pub fn decide_confirm_key(text: Option<&str>) -> ConfirmDecision {
    match text {
        Some("y" | "Y") => ConfirmDecision::Confirm,
        _ => ConfirmDecision::Cancel,
    }
}

/// The REQ-804 gate. A bare `\r` counts: it acts as Enter at the shell
/// exactly like `\n`.
#[must_use]
pub fn paste_needs_confirmation(bracketed_paste: bool, bytes: &[u8]) -> bool {
    !bracketed_paste && bytes.iter().any(|&b| b == b'\n' || b == b'\r')
}

fn paste_line_count(bytes: &[u8]) -> usize {
    let mut breaks = 0usize;
    let mut prev_cr = false;
    for &b in bytes {
        match b {
            b'\r' => {
                breaks += 1;
                prev_cr = true;
            }
            b'\n' => {
                if !prev_cr {
                    breaks += 1;
                }
                prev_cr = false;
            }
            _ => prev_cr = false,
        }
    }
    breaks + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_gate_fires_only_unbracketed_with_line_breaks() {
        assert!(!paste_needs_confirmation(true, b"rm -rf /\nyes\n"));
        assert!(!paste_needs_confirmation(false, b"echo hello"));
        assert!(!paste_needs_confirmation(false, b""));
        assert!(paste_needs_confirmation(false, b"a\nb"));
        assert!(paste_needs_confirmation(false, b"a\rb"));
    }

    #[test]
    fn only_y_confirms_everything_else_cancels() {
        assert_eq!(decide_confirm_key(Some("y")), ConfirmDecision::Confirm);
        assert_eq!(decide_confirm_key(Some("Y")), ConfirmDecision::Confirm);
        assert_eq!(decide_confirm_key(Some("\r")), ConfirmDecision::Cancel);
        assert_eq!(decide_confirm_key(Some("n")), ConfirmDecision::Cancel);
        assert_eq!(decide_confirm_key(None), ConfirmDecision::Cancel);
    }

    #[test]
    fn paste_label_counts_logical_lines() {
        assert_eq!(
            PendingConfirm::Paste(b"a\r\nb\nc".to_vec()).label(),
            "Paste 3 lines? The program did not request bracketed paste. [y/N]",
        );
    }
}
