//! `send --mouse EVENT`: `[MODS+]ACTION[-BUTTON]:COL,ROW` with 0-based
//! cells (docs/reference/cli.md "Verb details").

use std::fmt;
use std::str::FromStr;

use felis_protocol::messages::{
    GridDims, InputMods, MouseAction, MouseButton, MouseEvent, MouseProtocol,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gesture {
    Click,
    Press,
    Drag,
    Release,
    Wheel(MouseButton),
}

/// One parsed `--mouse` argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MouseArg {
    gesture: Gesture,
    button: MouseButton,
    mods: InputMods,
    col: u16,
    row: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParseMouseError {
    MissingCell,
    BadCell(String),
    UnknownAction(String),
    UnknownButton(String),
    ButtonOnWheel(String),
    UnknownModifier(String),
    DuplicateModifier(String),
    Super,
}

impl fmt::Display for ParseMouseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCell => f.write_str("expected `ACTION:COL,ROW`, e.g. `click:12,4`"),
            Self::BadCell(cell) => {
                write!(f, "`{cell}` is not `COL,ROW` (two 0-based cell numbers)")
            }
            Self::UnknownAction(a) => write!(
                f,
                "unknown action `{a}` (expected click, press, drag, release, wheel-up, \
                 wheel-down, wheel-left or wheel-right)"
            ),
            Self::UnknownButton(b) => {
                write!(f, "unknown button `{b}` (expected left, middle or right)")
            }
            Self::ButtonOnWheel(a) => write!(f, "`{a}` takes no button"),
            Self::UnknownModifier(m) => {
                write!(f, "unknown modifier `{m}` (expected shift, alt or ctrl)")
            }
            Self::DuplicateModifier(m) => write!(f, "modifier `{m}` appears twice"),
            Self::Super => f.write_str("no mouse report carries `super`"),
        }
    }
}

impl std::error::Error for ParseMouseError {}

const MODIFIER_NAMES: [(&str, InputMods); 3] = [
    ("shift", InputMods::SHIFT),
    ("alt", InputMods::ALT),
    ("ctrl", InputMods::CTRL),
];

fn gesture(name: &str) -> Option<Gesture> {
    Some(match name.to_ascii_lowercase().as_str() {
        "click" => Gesture::Click,
        "press" => Gesture::Press,
        "drag" => Gesture::Drag,
        "release" => Gesture::Release,
        "wheel-up" => Gesture::Wheel(MouseButton::WheelUp),
        "wheel-down" => Gesture::Wheel(MouseButton::WheelDown),
        "wheel-left" => Gesture::Wheel(MouseButton::WheelLeft),
        "wheel-right" => Gesture::Wheel(MouseButton::WheelRight),
        _ => return None,
    })
}

fn button(name: &str) -> Option<MouseButton> {
    Some(match name.to_ascii_lowercase().as_str() {
        "left" => MouseButton::Left,
        "middle" => MouseButton::Middle,
        "right" => MouseButton::Right,
        _ => return None,
    })
}

impl FromStr for MouseArg {
    type Err = ParseMouseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (head, cell) = s.rsplit_once(':').ok_or(ParseMouseError::MissingCell)?;
        let bad_cell = || ParseMouseError::BadCell(cell.to_owned());
        let (col, row) = cell.split_once(',').ok_or_else(bad_cell)?;
        let col = col.trim().parse().map_err(|_| bad_cell())?;
        let row = row.trim().parse().map_err(|_| bad_cell())?;

        let mut tokens: Vec<&str> = head.split('+').collect();
        let action = tokens.pop().unwrap_or_default();
        let mut mods = InputMods::empty();
        for token in tokens {
            if token.eq_ignore_ascii_case("super") {
                return Err(ParseMouseError::Super);
            }
            let (_, bit) = MODIFIER_NAMES
                .iter()
                .find(|(name, _)| token.eq_ignore_ascii_case(name))
                .ok_or_else(|| ParseMouseError::UnknownModifier(token.to_owned()))?;
            if mods.contains(*bit) {
                return Err(ParseMouseError::DuplicateModifier(token.to_owned()));
            }
            mods |= *bit;
        }

        // `wheel-up` itself contains a `-`, so the whole token is tried
        // as an action before a trailing `-BUTTON` is split off.
        let (gesture, button) = if let Some(g) = gesture(action) {
            (g, MouseButton::Left)
        } else {
            let (name, button_name) = action
                .rsplit_once('-')
                .ok_or_else(|| ParseMouseError::UnknownAction(action.to_owned()))?;
            let g = gesture(name).ok_or_else(|| ParseMouseError::UnknownAction(name.to_owned()))?;
            if matches!(g, Gesture::Wheel(_)) {
                return Err(ParseMouseError::ButtonOnWheel(name.to_owned()));
            }
            let b = button(button_name)
                .ok_or_else(|| ParseMouseError::UnknownButton(button_name.to_owned()))?;
            (g, b)
        };
        Ok(Self {
            gesture,
            button,
            mods,
            col,
            row,
        })
    }
}

impl fmt::Display for MouseArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, bit) in MODIFIER_NAMES {
            if self.mods.contains(bit) {
                write!(f, "{name}+")?;
            }
        }
        let action = match self.gesture {
            Gesture::Click => "click",
            Gesture::Press => "press",
            Gesture::Drag => "drag",
            Gesture::Release => "release",
            Gesture::Wheel(MouseButton::WheelUp) => "wheel-up",
            Gesture::Wheel(MouseButton::WheelDown) => "wheel-down",
            Gesture::Wheel(MouseButton::WheelLeft) => "wheel-left",
            Gesture::Wheel(_) => "wheel-right",
        };
        f.write_str(action)?;
        match (self.gesture, self.button) {
            (Gesture::Wheel(_), _) | (_, MouseButton::Left) => {}
            (_, MouseButton::Middle) => f.write_str("-middle")?,
            (_, _) => f.write_str("-right")?,
        }
        write!(f, ":{},{}", self.col, self.row)
    }
}

/// Why a `--mouse` event would not reach the program as a report.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Unreported {
    OutsideGrid { rows: u16, cols: u16 },
    NoMouseReporting,
    NoDragReporting,
}

impl fmt::Display for Unreported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutsideGrid { rows, cols } => write!(
                f,
                "the cell is outside the {cols}x{rows} grid (COL and ROW are 0-based)"
            ),
            Self::NoMouseReporting => {
                f.write_str("the program has no mouse reporting on; nothing was sent")
            }
            Self::NoDragReporting => f.write_str(
                "the program reports clicks but not drags (?1000 without ?1002); nothing was sent",
            ),
        }
    }
}

impl MouseArg {
    /// The wire events this argument sends, in order, with 1-based
    /// cells and no pixels: the daemon places those at the cell's
    /// center.
    pub(crate) fn events(self) -> Vec<MouseEvent> {
        let event = |button, action| MouseEvent {
            button: Some(button),
            action,
            mods: self.mods,
            x: self.col.saturating_add(1),
            y: self.row.saturating_add(1),
            px: 0,
            py: 0,
        };
        match self.gesture {
            Gesture::Click => vec![
                event(self.button, MouseAction::Press),
                event(self.button, MouseAction::Release),
            ],
            Gesture::Press => vec![event(self.button, MouseAction::Press)],
            Gesture::Drag => vec![event(self.button, MouseAction::Drag)],
            Gesture::Release => vec![event(self.button, MouseAction::Release)],
            Gesture::Wheel(wheel) => vec![event(wheel, MouseAction::Press)],
        }
    }

    /// `Err` when the session as last seen would drop this argument's
    /// events: the same filter the daemon's encoder applies, plus the
    /// grid bounds.
    pub(crate) fn check(self, protocol: MouseProtocol, dims: GridDims) -> Result<(), Unreported> {
        if self.col >= dims.cols || self.row >= dims.rows {
            return Err(Unreported::OutsideGrid {
                rows: dims.rows,
                cols: dims.cols,
            });
        }
        for event in self.events() {
            if !felis_grid::mouse_event_reported(protocol, event.action) {
                return Err(if protocol == MouseProtocol::Off {
                    Unreported::NoMouseReporting
                } else {
                    Unreported::NoDragReporting
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> MouseArg {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn dims(rows: u16, cols: u16) -> GridDims {
        GridDims {
            rows,
            cols,
            pixel_w: 0,
            pixel_h: 0,
        }
    }

    #[test]
    fn a_click_is_a_press_then_a_release_at_the_one_based_cell() {
        let events = parse("click:12,4").events();
        let shape: Vec<_> = events
            .iter()
            .map(|e| (e.button, e.action, e.x, e.y, e.px, e.py))
            .collect();
        assert_eq!(
            shape,
            [
                (Some(MouseButton::Left), MouseAction::Press, 13, 5, 0, 0),
                (Some(MouseButton::Left), MouseAction::Release, 13, 5, 0, 0),
            ]
        );
    }

    #[test]
    fn each_action_sends_its_events() {
        for (arg, expected) in [
            (
                "press-right:0,0",
                vec![(MouseButton::Right, MouseAction::Press)],
            ),
            (
                "drag-middle:0,0",
                vec![(MouseButton::Middle, MouseAction::Drag)],
            ),
            (
                "release:0,0",
                vec![(MouseButton::Left, MouseAction::Release)],
            ),
            (
                "wheel-up:0,0",
                vec![(MouseButton::WheelUp, MouseAction::Press)],
            ),
            (
                "wheel-down:0,0",
                vec![(MouseButton::WheelDown, MouseAction::Press)],
            ),
            (
                "wheel-left:0,0",
                vec![(MouseButton::WheelLeft, MouseAction::Press)],
            ),
            (
                "wheel-right:0,0",
                vec![(MouseButton::WheelRight, MouseAction::Press)],
            ),
        ] {
            let got: Vec<_> = parse(arg)
                .events()
                .iter()
                .map(|e| (e.button.expect("a button"), e.action))
                .collect();
            assert_eq!(got, expected, "{arg}");
        }
    }

    #[test]
    fn modifiers_use_the_chord_spelling_in_any_case() {
        let arg = parse("Ctrl+shift+ALT+click:1,2");
        assert_eq!(
            arg.mods,
            InputMods::CTRL | InputMods::SHIFT | InputMods::ALT
        );
        assert_eq!(arg.to_string(), "shift+alt+ctrl+click:1,2");
    }

    #[test]
    fn malformed_events_are_refused_with_their_reason() {
        for (arg, err) in [
            ("click", ParseMouseError::MissingCell),
            ("click:1", ParseMouseError::BadCell("1".into())),
            ("click:-1,2", ParseMouseError::BadCell("-1,2".into())),
            ("tap:1,2", ParseMouseError::UnknownAction("tap".into())),
            (
                "click-thumb:1,2",
                ParseMouseError::UnknownButton("thumb".into()),
            ),
            (
                "wheel-up-left:1,2",
                ParseMouseError::ButtonOnWheel("wheel-up".into()),
            ),
            (
                "meta+click:1,2",
                ParseMouseError::UnknownModifier("meta".into()),
            ),
            (
                "ctrl+ctrl+click:1,2",
                ParseMouseError::DuplicateModifier("ctrl".into()),
            ),
            ("super+click:1,2", ParseMouseError::Super),
        ] {
            assert_eq!(arg.parse::<MouseArg>(), Err(err), "{arg}");
        }
    }

    #[test]
    fn display_round_trips_through_the_parser() {
        for arg in [
            "click:0,0",
            "ctrl+drag-right:3,9",
            "wheel-down:79,23",
            "release-middle:1,1",
        ] {
            assert_eq!(parse(arg).to_string(), arg);
        }
    }

    #[test]
    fn the_last_cell_of_the_grid_is_in_bounds_and_the_next_is_not() {
        let any = MouseProtocol::AnyMotion;
        assert_eq!(parse("click:79,23").check(any, dims(24, 80)), Ok(()));
        for arg in ["click:80,23", "click:79,24"] {
            assert_eq!(
                parse(arg).check(any, dims(24, 80)),
                Err(Unreported::OutsideGrid { rows: 24, cols: 80 }),
                "{arg}",
            );
        }
    }

    #[test]
    fn an_event_the_protocol_filters_is_refused() {
        let grid = dims(24, 80);
        assert_eq!(
            parse("wheel-up:0,0").check(MouseProtocol::Off, grid),
            Err(Unreported::NoMouseReporting),
        );
        assert_eq!(
            parse("drag:0,0").check(MouseProtocol::ButtonEvents, grid),
            Err(Unreported::NoDragReporting),
        );
        assert_eq!(
            parse("click:0,0").check(MouseProtocol::ButtonEvents, grid),
            Ok(())
        );
        assert_eq!(
            parse("drag:0,0").check(MouseProtocol::ButtonAndDrag, grid),
            Ok(())
        );
    }
}
