//! `Keymap` (`Chord` -> [`Action`] lookup) and the [`BindingValue`]
//! config-layer enum (docs/reference/keybindings.md "Binding kinds").

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use felis_protocol::messages::PromptJump;

use crate::action::{
    Action, ClipboardScope, Escapes, FontSizeStep, IpcAction, PipeRegionSource, PipeTarget,
    ScrollStep, SwitchDirection,
};

use super::chord::Chord;

/// Which sink a `pipe` binding sends the region to (`target` field).
/// Uses `target = "clipboard"` for payload-free sinks and
/// `target = { command = ["bat"] }` or `target = { file = "/tmp/x" }`.

// Config twin of [`PipeTarget`]: `PipeTarget::File(None)` has no
// TOML spelling, so `temp_file` is its own token here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PipeSink {
    /// Spawn this argv (resolved on the client's host) with the region.
    /// A list of strings, never a shell string; wrap a pipeline in your
    /// own script. Empty → the default pager (`$PAGER`, else `less -R`),
    /// which is also what omitting `target` selects.
    Command(Vec<String>),
    /// Load the region onto the client clipboard.
    Clipboard,
    /// Write the region to this path, resolved against the
    /// `config.toml` that names it like every other file-valued key.
    File(PathBuf),
    /// Write the region to a client-chosen temp file whose path is
    /// logged.
    TempFile,
    /// Feed the region back into the session as bracketed paste.
    Paste,
}

impl Default for PipeSink {
    fn default() -> Self {
        Self::Command(Vec::new())
    }
}

impl PipeSink {
    fn into_target(self) -> PipeTarget {
        match self {
            Self::Command(argv) => PipeTarget::Command(argv),
            Self::Clipboard => PipeTarget::Clipboard,
            Self::File(path) => PipeTarget::File(Some(path)),
            Self::TempFile => PipeTarget::File(None),
            Self::Paste => PipeTarget::Paste,
        }
    }
}

/// One `[keymap]` binding value, discriminated by its `kind` field.
///
/// `unbind` removes the default binding for that chord.
/// See docs/reference/keybindings.md for every binding kind and its fields.
// An unbound chord forwards keystrokes to the terminal directly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BindingValue {
    /// Remove the default binding for this chord (config-layer only).
    Unbind,
    /// Send a literal text sequence to the PTY.
    SendString {
        /// Text to send. Backslash handling per `escapes`.
        text: String,
        /// Backslash-escape mode. Default `c_style`; `none` sends the
        /// backslashes as payload.
        #[serde(default)]
        escapes: Escapes,
    },
    /// Paste from a clipboard surface.
    Paste {
        /// Clipboard surface to read from.
        from: ClipboardScope,
    },
    /// Copy selection to a clipboard surface.
    Copy {
        /// Clipboard surface to write to.
        what: ClipboardScope,
    },
    /// Reload the client config.
    Reload,
    /// Detach from the current session and close the window.
    Detach,
    /// Step the font size.
    FontSize {
        /// Larger, smaller, or back to the configured size.
        step: FontSizeStep,
    },
    /// Scrollback navigation.
    Scroll {
        /// Direction / magnitude.
        step: ScrollStep,
    },
    /// Jump the viewport to a neighboring shell prompt (needs
    /// `OSC 133` shell integration).
    ScrollToPrompt {
        /// Which way to jump.
        to: PromptJump,
    },
    /// Toggle native fullscreen.
    ToggleFullscreen,
    /// Re-attach this client to a neighboring session in the daemon's
    /// order.
    SwitchSession {
        /// Which way to move in the daemon's session list.
        to: SwitchDirection,
    },
    /// Tear down the attached session: the keybind equivalent of
    /// `felis sessions kill`. Destructive; the client asks for
    /// confirmation before sending.
    KillSession,
    /// Open the scrollback search overlay.
    OpenScrollbackSearch,
    /// Create a new session on the daemon and switch to it in-window
    /// (tab-like), distinct from the CLI's detached
    /// `felis sessions spawn`.
    NewSession,
    /// Pipe a region of the terminal buffer somewhere. `target` picks
    /// the sink and carries whatever that sink needs: see
    /// [`PipeSink`].
    Pipe {
        /// Region of the buffer to pipe.
        source: PipeRegionSource,
        /// Which sink receives the region, plus its payload. Omitted →
        /// the default pager.
        #[serde(default)]
        target: PipeSink,
        /// Reconstruct SGR color/style (`true`) or emit plain text
        /// (`false`, default). Omit for hint pickers, editors, and tools
        /// that expect plain text. Set `true` for color-rendering pagers
        /// or `fzf --ansi`. Mirrors `felis sessions capture --ansi`.
        #[serde(default)]
        ansi: bool,
    },
    /// Launch an external command in a transient session sized to the
    /// live grid, with no piped region (for tools that supply their own data).
    /// Runs with `FELIS_ORIGIN_SESSION_ID`, `FELIS_HOST`, and `FELIS_CWD`
    /// exported, in the focused session's `OSC 7` directory when that
    /// names a directory on this machine.
    Run {
        /// Argv for the external command: a required, non-empty list
        /// of strings, never a shell string (wrap a pipeline in your
        /// own script).
        command: Vec<String>,
    },
}

impl BindingValue {
    /// `None` for [`BindingValue::Unbind`].
    #[must_use]
    pub fn into_action(self) -> Option<Action> {
        match self {
            Self::Unbind => None,
            Self::SendString { text, escapes } => Some(Action::SendString { text, escapes }),
            Self::Paste { from } => Some(Action::Paste { from }),
            Self::Copy { what } => Some(Action::Copy { what }),
            Self::Reload => Some(Action::Reload),
            Self::Detach => Some(Action::Detach),
            Self::FontSize { step } => Some(Action::FontSize(step)),
            Self::Scroll { step } => Some(Action::Scroll(step)),
            Self::ScrollToPrompt { to } => Some(Action::ScrollToPrompt(to)),
            Self::ToggleFullscreen => Some(Action::ToggleFullscreen),
            Self::SwitchSession { to } => Some(Action::Ipc(IpcAction::SwitchSession { to })),
            Self::KillSession => Some(Action::Ipc(IpcAction::KillSession)),
            Self::OpenScrollbackSearch => Some(Action::Ipc(IpcAction::OpenScrollbackSearch)),
            Self::NewSession => Some(Action::Ipc(IpcAction::NewSession)),
            Self::Pipe {
                source,
                target,
                ansi,
            } => Some(Action::PipeRegion {
                source,
                target: target.into_target(),
                ansi,
            }),
            Self::Run { command } => Some(Action::RunCommand { command }),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keymap {
    bindings: BTreeMap<Chord, Action>,
}

impl Keymap {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn resolve(&self, chord: &Chord) -> Option<&Action> {
        self.bindings.get(chord)
    }

    pub fn insert(&mut self, chord: Chord, action: Action) {
        self.bindings.insert(chord, action);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Applied in iteration order; the last entry for a chord wins.
    pub fn apply_overrides<I>(&mut self, overrides: I)
    where
        I: IntoIterator<Item = (Chord, BindingValue)>,
    {
        for (chord, value) in overrides {
            match value.into_action() {
                None => {
                    self.bindings.remove(&chord);
                }
                Some(action) => {
                    self.bindings.insert(chord, action);
                }
            }
        }
    }

    #[must_use]
    pub fn with_overrides<I>(mut self, overrides: I) -> Self
    where
        I: IntoIterator<Item = (Chord, BindingValue)>,
    {
        self.apply_overrides(overrides);
        self
    }
}

impl FromIterator<(Chord, Action)> for Keymap {
    fn from_iter<I: IntoIterator<Item = (Chord, Action)>>(iter: I) -> Self {
        Self {
            bindings: iter.into_iter().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::action::{
        Action, ClipboardScope, Escapes, FontSizeStep, IpcAction, ScrollStep, SwitchDirection,
    };

    use super::*;

    fn chord(s: &str) -> Chord {
        s.parse().unwrap()
    }

    #[test]
    fn empty_keymap_resolves_to_none() {
        let km = Keymap::new();
        assert!(km.resolve(&chord("ctrl+a")).is_none());
        assert!(km.is_empty());
    }

    #[test]
    fn insert_then_resolve_returns_the_action() {
        let mut km = Keymap::new();
        km.insert(chord("ctrl+shift+f"), Action::Reload);
        assert_eq!(km.resolve(&chord("ctrl+shift+f")), Some(&Action::Reload));
        assert_eq!(km.resolve(&chord("shift+ctrl+f")), Some(&Action::Reload));
    }

    #[test]
    fn insert_overwrites_previous_binding() {
        let mut km = Keymap::new();
        km.insert(chord("ctrl+r"), Action::Reload);
        km.insert(chord("ctrl+r"), Action::Detach);
        assert_eq!(km.resolve(&chord("ctrl+r")), Some(&Action::Detach));
        assert_eq!(km.len(), 1);
    }

    #[test]
    fn with_overrides_installs_user_bindings() {
        let km = Keymap::new()
            .with_overrides([(chord("ctrl+shift+f"), BindingValue::OpenScrollbackSearch)]);
        assert_eq!(
            km.resolve(&chord("ctrl+shift+f")),
            Some(&Action::Ipc(IpcAction::OpenScrollbackSearch)),
        );
    }

    #[test]
    fn with_overrides_unbind_removes_default() {
        let mut km = Keymap::new();
        km.insert(chord("ctrl+shift+r"), Action::Reload);
        let merged = km.with_overrides([(chord("ctrl+shift+r"), BindingValue::Unbind)]);
        assert!(merged.resolve(&chord("ctrl+shift+r")).is_none());
    }

    #[test]
    fn with_overrides_unbind_of_unbound_chord_is_a_no_op() {
        let km = Keymap::new().with_overrides([(chord("ctrl+r"), BindingValue::Unbind)]);
        assert!(km.is_empty());
    }

    #[test]
    fn with_overrides_replaces_existing_binding() {
        let mut km = Keymap::new();
        km.insert(chord("ctrl+r"), Action::Reload);
        let merged = km.with_overrides([(chord("ctrl+r"), BindingValue::Detach)]);
        assert_eq!(merged.resolve(&chord("ctrl+r")), Some(&Action::Detach));
        assert_eq!(merged.len(), 1);
    }

    #[test]
    fn with_overrides_applies_entries_in_iteration_order() {
        let km = Keymap::new().with_overrides([
            (chord("ctrl+r"), BindingValue::Reload),
            (chord("ctrl+r"), BindingValue::Unbind),
        ]);
        assert!(km.is_empty());

        let km = Keymap::new().with_overrides([
            (chord("ctrl+r"), BindingValue::Unbind),
            (chord("ctrl+r"), BindingValue::Reload),
        ]);
        assert_eq!(km.resolve(&chord("ctrl+r")), Some(&Action::Reload));
    }

    #[test]
    fn binding_value_unbind_translates_to_none() {
        assert_eq!(BindingValue::Unbind.into_action(), None);
    }

    #[test]
    fn binding_value_into_action_covers_every_non_unbind_variant() {
        assert_eq!(
            BindingValue::SwitchSession {
                to: SwitchDirection::Previous,
            }
            .into_action(),
            Some(Action::Ipc(IpcAction::SwitchSession {
                to: SwitchDirection::Previous,
            })),
        );
        assert_eq!(
            BindingValue::SwitchSession {
                to: SwitchDirection::Next,
            }
            .into_action(),
            Some(Action::Ipc(IpcAction::SwitchSession {
                to: SwitchDirection::Next,
            })),
        );
        assert_eq!(
            BindingValue::KillSession.into_action(),
            Some(Action::Ipc(IpcAction::KillSession)),
        );
        assert_eq!(
            BindingValue::OpenScrollbackSearch.into_action(),
            Some(Action::Ipc(IpcAction::OpenScrollbackSearch)),
        );
        assert_eq!(
            BindingValue::Scroll {
                step: ScrollStep::HalfPageUp,
            }
            .into_action(),
            Some(Action::Scroll(ScrollStep::HalfPageUp)),
        );
        assert_eq!(
            BindingValue::ScrollToPrompt {
                to: PromptJump::Previous,
            }
            .into_action(),
            Some(Action::ScrollToPrompt(PromptJump::Previous)),
        );
        assert_eq!(
            BindingValue::ScrollToPrompt {
                to: PromptJump::Next,
            }
            .into_action(),
            Some(Action::ScrollToPrompt(PromptJump::Next)),
        );
        assert_eq!(
            BindingValue::SendString {
                text: "hi\n".into(),
                escapes: Escapes::CStyle,
            }
            .into_action(),
            Some(Action::SendString {
                text: "hi\n".into(),
                escapes: Escapes::CStyle,
            }),
        );
        assert_eq!(
            BindingValue::Paste {
                from: ClipboardScope::Primary,
            }
            .into_action(),
            Some(Action::Paste {
                from: ClipboardScope::Primary,
            }),
        );
        assert_eq!(BindingValue::Reload.into_action(), Some(Action::Reload));
        assert_eq!(BindingValue::Detach.into_action(), Some(Action::Detach));
        assert_eq!(
            BindingValue::FontSize {
                step: FontSizeStep::Increase,
            }
            .into_action(),
            Some(Action::FontSize(FontSizeStep::Increase)),
        );
        assert_eq!(
            BindingValue::ToggleFullscreen.into_action(),
            Some(Action::ToggleFullscreen),
        );
        assert_eq!(
            BindingValue::NewSession.into_action(),
            Some(Action::Ipc(IpcAction::NewSession)),
        );
        assert_eq!(
            BindingValue::Copy {
                what: ClipboardScope::System,
            }
            .into_action(),
            Some(Action::Copy {
                what: ClipboardScope::System,
            }),
        );
        assert_eq!(
            BindingValue::FontSize {
                step: FontSizeStep::Decrease,
            }
            .into_action(),
            Some(Action::FontSize(FontSizeStep::Decrease)),
        );
        assert_eq!(
            BindingValue::FontSize {
                step: FontSizeStep::Reset,
            }
            .into_action(),
            Some(Action::FontSize(FontSizeStep::Reset)),
        );
        assert_eq!(
            BindingValue::Pipe {
                source: PipeRegionSource::Scrollback,
                target: PipeSink::Command(vec!["less".into(), "-R".into()]),
                ansi: true,
            }
            .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Scrollback,
                target: PipeTarget::Command(vec!["less".into(), "-R".into()]),
                ansi: true,
            }),
        );
        assert_eq!(
            BindingValue::Pipe {
                source: PipeRegionSource::Selection,
                target: PipeSink::default(),
                ansi: false,
            }
            .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Selection,
                target: PipeTarget::Command(vec![]),
                ansi: false,
            }),
        );
        assert_eq!(
            BindingValue::Pipe {
                source: PipeRegionSource::Visible,
                target: PipeSink::Clipboard,
                ansi: false,
            }
            .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Visible,
                target: PipeTarget::Clipboard,
                ansi: false,
            }),
        );
        assert_eq!(
            BindingValue::Pipe {
                source: PipeRegionSource::Selection,
                target: PipeSink::Paste,
                ansi: false,
            }
            .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Selection,
                target: PipeTarget::Paste,
                ansi: false,
            }),
        );
        assert_eq!(
            BindingValue::Pipe {
                source: PipeRegionSource::Scrollback,
                target: PipeSink::TempFile,
                ansi: false,
            }
            .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Scrollback,
                target: PipeTarget::File(None),
                ansi: false,
            }),
        );
        assert_eq!(
            BindingValue::Pipe {
                source: PipeRegionSource::Scrollback,
                target: PipeSink::File("/tmp/felis-dump.txt".into()),
                ansi: false,
            }
            .into_action(),
            Some(Action::PipeRegion {
                source: PipeRegionSource::Scrollback,
                target: PipeTarget::File(Some("/tmp/felis-dump.txt".into())),
                ansi: false,
            }),
        );
        assert_eq!(
            BindingValue::Run {
                command: vec!["felis-session-picker".into()],
            }
            .into_action(),
            Some(Action::RunCommand {
                command: vec!["felis-session-picker".into()],
            }),
        );
    }
}
