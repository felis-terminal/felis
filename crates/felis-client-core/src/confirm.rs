//! Confirmation state behind the two destructive gates: the
//! `kill_session` chord and the unbracketed control-byte paste (REQ-804,
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
                if lines > 1 {
                    format!(
                        "Paste {lines} lines? The program did not request bracketed paste. [y/N]"
                    )
                } else {
                    "Paste text containing control characters? The program did not request bracketed paste. [y/N]"
                        .to_owned()
                }
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

/// The REQ-804 gate. HT is exempt: pasted code is full of tabs, and
/// completion runs nothing.
#[must_use]
pub fn paste_needs_confirmation(bracketed_paste: bool, bytes: &[u8]) -> bool {
    !bracketed_paste && bytes.iter().any(|&b| b < 0x20 && b != b'\t')
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

    use proptest::prelude::*;

    #[test]
    fn paste_gate_fires_only_unbracketed_with_control_bytes() {
        assert!(!paste_needs_confirmation(true, b"rm -rf /\nyes\n"));
        assert!(!paste_needs_confirmation(false, b"echo hello"));
        assert!(!paste_needs_confirmation(false, b""));
        assert!(paste_needs_confirmation(false, b"a\nb"));
        assert!(paste_needs_confirmation(false, b"a\rb"));
    }

    #[test]
    fn unbracketed_paste_with_ctrl_o_esc_or_ctrl_c_and_no_newline_needs_confirmation() {
        assert!(paste_needs_confirmation(false, b"rm -rf ~/x\x0f"));
        assert!(paste_needs_confirmation(false, b"\x1b[A"));
        assert!(paste_needs_confirmation(false, b"echo\x03"));
    }

    #[test]
    fn unbracketed_paste_with_only_tabs_does_not_need_confirmation() {
        assert!(!paste_needs_confirmation(false, b"a\tb\t"));
    }

    #[test]
    fn non_ascii_text_and_del_do_not_need_confirmation() {
        assert!(!paste_needs_confirmation(false, "猫 café".as_bytes()));
        assert!(!paste_needs_confirmation(false, b"ab\x7f"));
    }

    proptest! {
        #[test]
        fn unbracketed_gate_fires_exactly_on_a_c0_byte_other_than_tab(
            bytes in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let expected = bytes.iter().any(|&b| b.is_ascii_control() && b != 0x7f && b != b'\t');
            prop_assert_eq!(paste_needs_confirmation(false, &bytes), expected);
        }

        #[test]
        fn bracketed_paste_never_needs_confirmation(
            bytes in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            prop_assert!(!paste_needs_confirmation(true, &bytes));
        }
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

    #[test]
    fn single_line_control_byte_paste_label_names_control_characters() {
        assert_eq!(
            PendingConfirm::Paste(b"ls\x0f".to_vec()).label(),
            "Paste text containing control characters? The program did not request bracketed paste. [y/N]",
        );
    }
}
