//! `Action`: the closed set of things a client can do in response to user
//! input. Variants carry typed parameters, never expressions, callbacks,
//! or shell strings (principle 1; docs/explanation/input.md "Action
//! mapping").

use std::path::PathBuf;

use felis_protocol::messages::{PromptJump, RegionSource};
use serde::{Deserialize, Serialize};

/// Backslash-escape mode for a `send_string` binding's `text`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Escapes {
    /// `text` is sent byte-for-byte.
    None,
    /// Recognize `\n`, `\r`, `\t`, `\\`, `\0`, `\xNN`, `\e`. Any
    /// other backslash sequence is a config error caught at parse
    /// time, not silently passed through.
    #[default]
    CStyle,
}

/// Every variant is a config error: the keymap entry is dropped at
/// `compile` time, so an unrecognized escape never reaches the PTY as its
/// literal spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscapeError {
    TrailingBackslash,
    BadHexEscape,
    UnknownEscape(char),
}

impl core::fmt::Display for EscapeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TrailingBackslash => f.write_str("trailing backslash with nothing to escape"),
            Self::BadHexEscape => f.write_str("`\\x` needs exactly two hex digits"),
            Self::UnknownEscape(c) => write!(f, "unknown escape `\\{c}`"),
        }
    }
}

impl core::error::Error for EscapeError {}

impl Escapes {
    /// Bytes rather than a `String`: `\xNN` can name a byte that is not
    /// valid UTF-8 on its own, and the PTY is a byte stream.
    pub fn decode(self, text: &str) -> Result<Vec<u8>, EscapeError> {
        match self {
            Self::None => Ok(text.as_bytes().to_vec()),
            Self::CStyle => decode_c_style(text),
        }
    }
}

/// The closed escape set of docs/explanation/input.md "Action mapping",
/// spelled out rather than delegated to a general unescaper so growing it
/// stays a deliberate edit (principle 1).
fn decode_c_style(text: &str) -> Result<Vec<u8>, EscapeError> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        let escaped = chars.next().ok_or(EscapeError::TrailingBackslash)?;
        match escaped {
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            'e' => out.push(0x1b),
            '0' => out.push(0),
            '\\' => out.push(b'\\'),
            'x' => {
                let hi = chars.next().ok_or(EscapeError::BadHexEscape)?;
                let lo = chars.next().ok_or(EscapeError::BadHexEscape)?;
                let (hi, lo) = hi
                    .to_digit(16)
                    .zip(lo.to_digit(16))
                    .ok_or(EscapeError::BadHexEscape)?;
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "two base-16 digits compose a value <= 0xFF"
                )]
                out.push(((hi << 4) | lo) as u8);
            }
            other => return Err(EscapeError::UnknownEscape(other)),
        }
    }
    Ok(out)
}

/// Region of the terminal buffer a `pipe` binding serializes (its
/// `source` field).
// Not a wire type: a client-side superset of `RegionSource` adding
// `Selection`, which is client-owned (principle 3) and extracted here
// rather than named to a daemon that holds no selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PipeRegionSource {
    /// Entire retained scrollback plus the live screen.
    Scrollback,
    /// The current viewport only (what is on screen now).
    Visible,
    /// The active selection in this window.
    Selection,
    /// The last command's output: the most recent `OSC 133 C → D` range.
    CommandOutput,
    /// The last command line plus its output: the most recent
    /// `OSC 133 B → D` range.
    LastCommand,
}

/// Client-side like [`PipeRegionSource`]: every sink runs on the machine
/// whose keymap named it, so none of them travels
/// (docs/explanation/data-model/scrollback.md "Piping to an external
/// command").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipeTarget {
    /// Argv with the temp-file path appended; never a shell string
    /// (principle 1). Empty selects the default pager (`$PAGER`, else
    /// `less -R`).
    Command(Vec<String>),
    Clipboard,
    /// Already resolved against the config's directory. `None` is a
    /// client-chosen temp file whose path is logged.
    File(Option<PathBuf>),
    /// Bracketed paste back into the session.
    Paste,
}

impl PipeRegionSource {
    #[must_use]
    pub const fn wire_source(self) -> Option<RegionSource> {
        match self {
            Self::Scrollback => Some(RegionSource::Scrollback),
            Self::Visible => Some(RegionSource::Visible),
            Self::CommandOutput => Some(RegionSource::CommandOutput),
            Self::LastCommand => Some(RegionSource::LastCommand),
            Self::Selection => None,
        }
    }
}

/// Where a paste / copy reads from / writes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ClipboardScope {
    /// OS clipboard (the Cmd/Ctrl-C/V buffer).
    System,
    /// X11 PRIMARY selection. Falls back to System on platforms
    /// without a primary selection.
    Primary,
}

/// Scrollback navigation steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ScrollStep {
    /// One row up.
    LineUp,
    /// One row down.
    LineDown,
    /// Half a screen up (kitty's `scroll_half_page`: a whole screen
    /// leaves no overlapping row to reorient against).
    HalfPageUp,
    /// Half a screen down.
    HalfPageDown,
    /// Top of scrollback.
    Home,
    /// Bottom of scrollback (live view).
    End,
}

/// Font-size steps for a `font_size` binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FontSizeStep {
    /// One config-defined step larger.
    Increase,
    /// One config-defined step smaller.
    Decrease,
    /// Back to the configured size.
    Reset,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum IpcAction {
    SwitchSession {
        to: SwitchDirection,
    },
    /// Destructive; the client prompts for confirmation before sending.
    KillSession,
    OpenScrollbackSearch,
    /// Create a session and switch to it in-window, unlike the CLI's
    /// detached `spawn` (docs/explanation/architecture/control-surfaces.md).
    NewSession,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SwitchDirection {
    /// Previous session in the daemon's order.
    Previous,
    /// Next session in the daemon's order.
    Next,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Bracketed-paste wrapping is not applied: a typed sequence, not a
    /// paste (docs/explanation/input.md "Action mapping").
    SendString {
        text: String,
        escapes: Escapes,
    },
    /// Bracketed when the program enabled `?2004h`; raw otherwise.
    Paste {
        from: ClipboardScope,
    },
    Copy {
        what: ClipboardScope,
    },
    Reload,
    /// Closes the window; the session survives in the daemon pool.
    Detach,
    FontSize(FontSizeStep),
    Scroll(ScrollStep),
    /// Carries only the direction; the daemon resolves the target offset
    /// from its `OSC 133` marks.
    ScrollToPrompt(PromptJump),
    ToggleFullscreen,
    Ipc(IpcAction),
    /// The `pipe` keymap token (docs/explanation/data-model/scrollback.md
    /// "Piping to an external command"). `target` never leaves this
    /// machine.
    PipeRegion {
        source: PipeRegionSource,
        target: PipeTarget,
        /// Reconstruct SGR color/style rather than emit plain text.
        ansi: bool,
    },
    /// The `run` keymap token (docs/explanation/input.md "Action
    /// mapping"): a transient session sized to the live grid, fed no
    /// region. Argv, never a shell string (principle 1).
    RunCommand {
        command: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_escapes_pass_the_text_through_byte_for_byte() {
        assert_eq!(Escapes::None.decode(r"\").unwrap(), br"\");
        assert_eq!(Escapes::None.decode(r"a\nb").unwrap(), br"a\nb");
        assert_eq!(Escapes::None.decode("").unwrap(), b"");
    }

    #[test]
    fn c_style_decodes_the_documented_escape_set() {
        assert_eq!(Escapes::CStyle.decode(r"\n\r\t").unwrap(), b"\n\r\t");
        assert_eq!(Escapes::CStyle.decode(r"\e").unwrap(), b"\x1b");
        assert_eq!(Escapes::CStyle.decode(r"\0").unwrap(), b"\0");
        assert_eq!(Escapes::CStyle.decode(r"\\").unwrap(), br"\");
        assert_eq!(Escapes::CStyle.decode(r"\e\r").unwrap(), b"\x1b\r");
    }

    #[test]
    fn c_style_hex_escapes_take_exactly_two_digits() {
        assert_eq!(Escapes::CStyle.decode(r"\x1b").unwrap(), b"\x1b");
        assert_eq!(Escapes::CStyle.decode(r"\xFF").unwrap(), b"\xff");
        assert_eq!(Escapes::CStyle.decode(r"\x0a7").unwrap(), b"\n7");
    }

    #[test]
    fn c_style_hex_escape_can_name_a_non_utf8_byte() {
        assert_eq!(Escapes::CStyle.decode(r"\x80").unwrap(), b"\x80");
    }

    #[test]
    fn c_style_preserves_multibyte_literals() {
        assert_eq!(Escapes::CStyle.decode("あ\\n").unwrap(), "あ\n".as_bytes());
    }

    #[test]
    fn c_style_rejects_malformed_escapes() {
        assert_eq!(
            Escapes::CStyle.decode(r"\"),
            Err(EscapeError::TrailingBackslash),
        );
        assert_eq!(
            Escapes::CStyle.decode(r"\q"),
            Err(EscapeError::UnknownEscape('q')),
        );
        assert_eq!(
            Escapes::CStyle.decode("\\u3042"),
            Err(EscapeError::UnknownEscape('u')),
        );
        assert_eq!(
            Escapes::CStyle.decode(r"\xZZ"),
            Err(EscapeError::BadHexEscape)
        );
        assert_eq!(
            Escapes::CStyle.decode(r"\x1"),
            Err(EscapeError::BadHexEscape)
        );
    }

    #[test]
    fn pipe_region_source_lowers_selection_to_no_wire_source() {
        assert_eq!(PipeRegionSource::Selection.wire_source(), None);
        assert_eq!(
            PipeRegionSource::Scrollback.wire_source(),
            Some(RegionSource::Scrollback),
        );
        assert_eq!(
            PipeRegionSource::Visible.wire_source(),
            Some(RegionSource::Visible),
        );
        assert_eq!(
            PipeRegionSource::CommandOutput.wire_source(),
            Some(RegionSource::CommandOutput),
        );
        assert_eq!(
            PipeRegionSource::LastCommand.wire_source(),
            Some(RegionSource::LastCommand),
        );
    }
}
