//! Platform-free key encoder: converts the structured key events
//! [`InputMsg::Key`](felis_protocol::messages::InputMsg) carries into
//! the bytes forwarded to the PTY, against the keyboard modes this
//! daemon owns. Uses legacy xterm encoding by default and Kitty
//! keyboard encoding when flags are active.

use felis_protocol::messages::{Key, KeyEventKind, KeyLocation, KeyMods as Modifiers, NamedKey};

pub(crate) use felis_protocol::kitty_keyboard::KittyKbdFlags;
pub(crate) use felis_protocol::messages::ModifyOtherKeys;

const EVENT_PRESS: u8 = 1;
const EVENT_REPEAT: u8 = 2;
const EVENT_RELEASE: u8 = 3;

/// `None` when the event has no terminal encoding: modifier-only press,
/// dead key, IME composition, or a release without event types enabled.
#[allow(clippy::fn_params_excessive_bools)]
pub(crate) fn encode(
    key: &Key,
    text: Option<&str>,
    mods: Modifiers,
    kind: KeyEventKind,
    kitty_kbd_flags: KittyKbdFlags,
    modify_other_keys: ModifyOtherKeys,
    application_cursor: bool,
    application_keypad: bool,
    win32_input_mode: bool,
    key_location: KeyLocation,
) -> Option<Vec<u8>> {
    let is_press = !matches!(kind, KeyEventKind::Release);
    // win32-input-mode (`?9001`) supersedes every VT / Kitty encoding:
    // ConPTY reconstructs garbage from anything else. Checked ahead of
    // the release-swallowing Kitty logic because it reports releases.
    if win32_input_mode {
        return encode_win32_input(key, text, mods, is_press);
    }
    let kitty_disambiguate = kitty_kbd_flags.contains(KittyKbdFlags::DISAMBIGUATE);
    let kitty_all_as_escapes = kitty_kbd_flags.contains(KittyKbdFlags::REPORT_ALL_AS_ESCAPES);
    // Bit 3 (all-as-escapes) implies bit 0 (disambiguate).
    let kitty_csi_u_on = kitty_disambiguate || kitty_all_as_escapes;
    // A repeat is reported as its own event type only while bit 2 is
    // active; the Kitty spec has no way to say "repeat" without it, so
    // it reports as a press, which is what every other encoding does.
    let event = match kind {
        KeyEventKind::Release => EVENT_RELEASE,
        KeyEventKind::Repeat if kitty_kbd_flags.contains(KittyKbdFlags::REPORT_EVENT_TYPES) => {
            EVENT_REPEAT
        }
        KeyEventKind::Press | KeyEventKind::Repeat => EVENT_PRESS,
    };
    // Associated text and alternate keycodes ride on presses only.
    let kitty_text = if is_press && kitty_kbd_flags.contains(KittyKbdFlags::REPORT_ASSOCIATED_TEXT)
    {
        text
    } else {
        None
    };
    let kitty_alt = if is_press && kitty_kbd_flags.contains(KittyKbdFlags::REPORT_ALTERNATE_KEYS) {
        kitty_alt_keycode(key, mods)
    } else {
        None
    };
    if !is_press {
        let event_types_on = kitty_kbd_flags.contains(KittyKbdFlags::REPORT_EVENT_TYPES);
        if !event_types_on || !kitty_csi_u_on {
            return None;
        }
        let keycode = kitty_encoded_keycode(key, mods, kitty_all_as_escapes)?;
        return Some(kitty_kbd_csi_u(keycode, None, mods, event, None));
    }
    if kitty_csi_u_on && let Some(keycode) = kitty_encoded_keycode(key, mods, kitty_all_as_escapes)
    {
        return Some(kitty_kbd_csi_u(keycode, kitty_alt, mods, event, kitty_text));
    }
    // xterm modifyOtherKeys (REQ-506), superseded by any Kitty flag.
    // Unlike the Kitty path it never escapes a bare key, so plain Escape
    // / Enter / typed text stay byte-identical when it is flipped on.
    if kitty_kbd_flags.is_empty()
        && modify_other_keys != ModifyOtherKeys::Off
        && let Some(keycode) =
            modify_other_keys_keycode(key, mods, modify_other_keys == ModifyOtherKeys::Level2)
    {
        return Some(kitty_kbd_csi_u(keycode, None, mods, EVENT_PRESS, None));
    }
    // DECKPAM: unmodified numpad keys emit the SS3 finals terminfo's
    // application-keypad caps expect. Not under Kitty flags, which
    // report the numpad themselves.
    if kitty_kbd_flags.is_empty()
        && application_keypad
        && key_location == KeyLocation::Numpad
        && !mods.control_key()
        && !mods.alt_key()
        && !mods.super_key()
        && !mods.shift_key()
        && let Some(final_byte) = application_keypad_ss3(key)
    {
        return Some(vec![0x1b, b'O', final_byte]);
    }
    if let Key::Named(named) = key
        && let Some(bytes) = encode_named(*named, mods, application_cursor)
    {
        return Some(bytes);
    }
    // metaSendsEscape: bash readline binds Alt+B / Alt+F / Alt+. against
    // it. Non-ASCII text falls through to the passthrough rather than
    // fabricating a meta form.
    if mods.alt_key()
        && !mods.super_key()
        && let Key::Character(s) = key
    {
        let body: Option<u8> = if mods.control_key() {
            legacy_ctrl_byte(s)
        } else {
            single_ascii(s)
        };
        if let Some(byte) = body {
            return Some(vec![0x1b, byte]);
        }
    }
    if mods.control_key()
        && !mods.alt_key()
        && !mods.super_key()
        && let Key::Character(s) = key
        && let Some(byte) = legacy_ctrl_byte(s)
    {
        return Some(vec![byte]);
    }
    text.filter(|s| !s.is_empty())
        .map(|s| s.as_bytes().to_vec())
}

/// `CSI Vk ; Sc ; Uc ; Kd ; Cs ; Rc _`. `Sc` is always `0`: a scan code
/// cannot be derived from a logical key, and `ConPTY` / `PSReadLine` key
/// off `Vk` + `Uc`. `Rc` is always `1`: winit delivers each auto-repeat
/// as its own event.
fn encode_win32_input(
    key: &Key,
    text: Option<&str>,
    mods: Modifiers,
    is_press: bool,
) -> Option<Vec<u8>> {
    let vk = win32_virtual_key(key);
    let uc = win32_unicode_char(key, text);
    if vk == 0 && uc == 0 {
        return None;
    }
    let cs = win32_control_key_state(mods);
    let kd = u8::from(is_press);
    let mut out = Vec::with_capacity(24);
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(vk.to_string().as_bytes());
    out.extend_from_slice(b";0;");
    out.extend_from_slice(uc.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(kd.to_string().as_bytes());
    out.push(b';');
    out.extend_from_slice(cs.to_string().as_bytes());
    out.extend_from_slice(b";1_");
    Some(out)
}

/// `VK_A`..`VK_Z` / `VK_0`..`VK_9` equal the uppercase ASCII byte.
/// Punctuation has a layout-dependent OEM `VK_` a logical key cannot
/// recover, so it stays `0` and rides on `Uc`.
fn win32_virtual_key(key: &Key) -> u16 {
    match key {
        Key::Named(named) => match named {
            NamedKey::Enter => 0x0D,
            NamedKey::Tab => 0x09,
            NamedKey::Escape => 0x1B,
            NamedKey::Space => 0x20,
            NamedKey::Backspace => 0x08,
            NamedKey::Insert => 0x2D,
            NamedKey::Delete => 0x2E,
            NamedKey::Home => 0x24,
            NamedKey::End => 0x23,
            NamedKey::PageUp => 0x21,
            NamedKey::PageDown => 0x22,
            NamedKey::ArrowUp => 0x26,
            NamedKey::ArrowDown => 0x28,
            NamedKey::ArrowLeft => 0x25,
            NamedKey::ArrowRight => 0x27,
            // Windows has no VK beyond F24.
            NamedKey::F(fk) if fk.get() <= 24 => 0x6F + u16::from(fk.get()),
            NamedKey::F(_) => 0,
        },
        Key::Character(s) => {
            let mut chars = s.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => {
                    u16::from(c.to_ascii_uppercase() as u8)
                }
                _ => 0,
            }
        }
        Key::Other => 0,
    }
}

/// The `Uc` field. A non-BMP grapheme contributes only its high
/// surrogate.
fn win32_unicode_char(key: &Key, text: Option<&str>) -> u16 {
    if let Some(c) = text.and_then(|s| s.chars().next()) {
        let mut buf = [0u16; 2];
        return c.encode_utf16(&mut buf)[0];
    }
    match key {
        Key::Named(NamedKey::Enter) => 0x0D,
        Key::Named(NamedKey::Tab) => 0x09,
        Key::Named(NamedKey::Backspace) => 0x08,
        Key::Named(NamedKey::Escape) => 0x1B,
        Key::Named(NamedKey::Space) => 0x20,
        _ => 0,
    }
}

/// `dwControlKeyState` (`wincon.h`). [`Modifiers`] track no key side, so
/// Ctrl / Alt map to the `LEFT_*` bit; `PSReadLine` keys off "a Ctrl is
/// down", not the side.
const fn win32_control_key_state(mods: Modifiers) -> u32 {
    const SHIFT_PRESSED: u32 = 0x0010;
    const LEFT_ALT_PRESSED: u32 = 0x0002;
    const LEFT_CTRL_PRESSED: u32 = 0x0008;
    let mut cs = 0;
    if mods.shift_key() {
        cs |= SHIFT_PRESSED;
    }
    if mods.control_key() {
        cs |= LEFT_CTRL_PRESSED;
    }
    if mods.alt_key() {
        cs |= LEFT_ALT_PRESSED;
    }
    cs
}

const fn legacy_kitty_keycode(named: NamedKey) -> Option<u32> {
    match named {
        NamedKey::Enter => Some(13),
        NamedKey::Tab => Some(9),
        NamedKey::Backspace => Some(127),
        // Without this a modified Space collapses to a bare 0x20 the
        // program cannot tell from an ordinary space.
        NamedKey::Space => Some(32),
        _ => None,
    }
}

fn single_ascii(s: &str) -> Option<u8> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii() {
        return None;
    }
    Some(c as u8)
}

/// Lowercase per the Kitty spec ("the lowercase ASCII codepoint of the
/// key").
fn single_ascii_lower(s: &str) -> Option<u8> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii() {
        return None;
    }
    Some(c.to_ascii_lowercase() as u8)
}

/// The VT100 application-keypad finals terminfo's `ka1` / `kb2` / `kent`
/// caps name. Matched on the logical key because winit reports a numpad
/// digit as `Key::Character` once `NumLock` is on.
fn application_keypad_ss3(key: &Key) -> Option<u8> {
    match key {
        Key::Character(s) if s.chars().count() == 1 => match s.chars().next()? {
            '0' => Some(b'p'),
            '1' => Some(b'q'),
            '2' => Some(b'r'),
            '3' => Some(b's'),
            '4' => Some(b't'),
            '5' => Some(b'u'),
            '6' => Some(b'v'),
            '7' => Some(b'w'),
            '8' => Some(b'x'),
            '9' => Some(b'y'),
            '.' => Some(b'n'),
            '+' => Some(b'k'),
            '-' => Some(b'm'),
            '*' => Some(b'j'),
            '/' => Some(b'o'),
            _ => None,
        },
        Key::Named(NamedKey::Enter) => Some(b'M'),
        _ => None,
    }
}

fn kitty_kbd_csi_u(
    keycode: u32,
    alt: Option<u32>,
    mods: Modifiers,
    event_type: u8,
    text: Option<&str>,
) -> Vec<u8> {
    let mut mod_bits: u32 = 0;
    if mods.shift_key() {
        mod_bits |= 1;
    }
    if mods.alt_key() {
        mod_bits |= 2;
    }
    if mods.control_key() {
        mod_bits |= 4;
    }
    if mods.super_key() {
        mod_bits |= 8;
    }
    let text = text.filter(|s| !s.is_empty());
    let has_text = text.is_some();
    let need_mod = mod_bits != 0 || event_type != EVENT_PRESS || has_text;
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(keycode.to_string().as_bytes());
    if let Some(a) = alt {
        out.push(b':');
        out.extend_from_slice(a.to_string().as_bytes());
    }
    // The text param is positional after `;mod[:event]`, so the mod
    // slot must exist whenever text follows.
    if need_mod {
        out.push(b';');
        out.extend_from_slice((mod_bits + 1).to_string().as_bytes());
        if event_type != EVENT_PRESS {
            out.push(b':');
            out.extend_from_slice(event_type.to_string().as_bytes());
        }
    }
    if let Some(t) = text {
        out.push(b';');
        let mut first = true;
        for c in t.chars() {
            if !first {
                out.push(b':');
            }
            out.extend_from_slice((c as u32).to_string().as_bytes());
            first = false;
        }
    }
    out.push(b'u');
    out
}

/// ASCII letters only: a shifted symbol is layout-dependent (shift-1 is
/// `!` on US, `+` on DE) and needs winit's physical-key surface.
fn kitty_alt_keycode(key: &Key, mods: Modifiers) -> Option<u32> {
    if !mods.shift_key() {
        return None;
    }
    let Key::Character(s) = key else { return None };
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() || !c.is_ascii_alphabetic() {
        return None;
    }
    Some(u32::from(c.to_ascii_uppercase()))
}

/// `force_all` (bit 3, all-as-escapes) drops bit 0's requirement that a
/// modifier be held.
fn kitty_encoded_keycode(key: &Key, mods: Modifiers, force_all: bool) -> Option<u32> {
    if matches!(key, Key::Named(NamedKey::Escape)) {
        return Some(27);
    }
    let any_mod = mods.control_key() || mods.alt_key() || mods.super_key();
    if let Key::Named(named) = key {
        let keycode = legacy_kitty_keycode(*named)?;
        // Shift+Enter / Shift+Backspace have no distinct legacy byte, so
        // kitty / wezterm / Ghostty / iTerm2 all route them through CSI u
        // beyond what the bit-0 spec text requires; Shift+Tab keeps its
        // unambiguous `CSI Z`, as kitty does.
        let shift_counts = mods.shift_key() && !matches!(named, NamedKey::Tab);
        if !any_mod && !shift_counts && !force_all {
            return None;
        }
        return Some(keycode);
    }
    if !any_mod && !force_all {
        return None;
    }
    if let Key::Character(s) = key
        && let Some(cp) = single_ascii_lower(s)
    {
        return Some(u32::from(cp));
    }
    None
}

/// Separate from [`kitty_encoded_keycode`]: modifyOtherKeys never
/// escapes a bare Escape. xterm's split: level 1 escapes only
/// combinations with no legacy byte (Ctrl+digit, Ctrl+punctuation,
/// Super+key), so Ctrl+C still sends `0x03`; level 2 escapes every
/// modified key.
fn modify_other_keys_keycode(key: &Key, mods: Modifiers, level2: bool) -> Option<u32> {
    let ctrl = mods.control_key();
    let alt = mods.alt_key();
    let sup = mods.super_key();
    let any_mod = ctrl || alt || sup;

    if matches!(key, Key::Named(NamedKey::Escape)) {
        return any_mod.then_some(27);
    }
    // Arrows, F-keys, Home, ... fall through to the modifyCursorKeys /
    // modifyFunctionKeys CSI forms felis emits unconditionally.
    if let Key::Named(named) = key {
        let keycode = legacy_kitty_keycode(*named)?;
        if !level2 {
            return None;
        }
        let shift_counts = mods.shift_key() && !matches!(named, NamedKey::Tab);
        return (any_mod || shift_counts).then_some(keycode);
    }
    if !any_mod {
        return None;
    }
    let Key::Character(s) = key else { return None };
    let cp = u32::from(single_ascii_lower(s)?);
    if level2 {
        return Some(cp);
    }
    if alt || (ctrl && ctrl_byte(s).is_some()) {
        return None;
    }
    Some(cp)
}

/// xterm's `1 + bits` modifier param; `None` when no modifier is held
/// so the bare legacy sequence goes out.
const fn xterm_mod_param(mods: Modifiers) -> Option<u32> {
    let mut bits: u32 = 0;
    if mods.shift_key() {
        bits |= 1;
    }
    if mods.alt_key() {
        bits |= 2;
    }
    if mods.control_key() {
        bits |= 4;
    }
    if mods.super_key() {
        bits |= 8;
    }
    if bits == 0 { None } else { Some(bits + 1) }
}

const fn xterm_csi_letter(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::ArrowUp => Some(b'A'),
        NamedKey::ArrowDown => Some(b'B'),
        NamedKey::ArrowRight => Some(b'C'),
        NamedKey::ArrowLeft => Some(b'D'),
        NamedKey::End => Some(b'F'),
        NamedKey::Home => Some(b'H'),
        _ => None,
    }
}

/// The gaps at 16 and 22 are xterm's: VT220 reserved them for "Help" and
/// "Do".
const fn xterm_csi_tilde_num(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::Insert => Some(2),
        NamedKey::Delete => Some(3),
        NamedKey::PageUp => Some(5),
        NamedKey::PageDown => Some(6),
        NamedKey::F(k) => match k.get() {
            5 => Some(15),
            6 => Some(17),
            7 => Some(18),
            8 => Some(19),
            9 => Some(20),
            10 => Some(21),
            11 => Some(23),
            12 => Some(24),
            _ => None,
        },
        _ => None,
    }
}

/// F1-F4 are SS3 bare: terminfo's `kf1`-`kf4` expect `ESC O <L>`.
const fn xterm_ss3_final(named: NamedKey) -> Option<u8> {
    match named {
        NamedKey::F(k) => match k.get() {
            1 => Some(b'P'),
            2 => Some(b'Q'),
            3 => Some(b'R'),
            4 => Some(b'S'),
            _ => None,
        },
        _ => None,
    }
}

/// xterm's modifyCursorKeys shape; modifiers force the SS3 family onto
/// CSI too.
fn csi_modified_letter(letter: u8, m: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(b"\x1b[1;");
    out.extend_from_slice(m.to_string().as_bytes());
    out.push(letter);
    out
}

fn xterm_ss3_letter_seq(letter: u8, mods: Modifiers) -> Vec<u8> {
    xterm_mod_param(mods).map_or_else(
        || vec![0x1b, b'O', letter],
        |m| csi_modified_letter(letter, m),
    )
}

/// Under DECCKM the bare form is SS3, which vim / less / emacs read out
/// of terminfo's `kcuu1`; the modified form stays CSI regardless, as in
/// xterm.
fn xterm_letter_seq(letter: u8, mods: Modifiers, application_cursor: bool) -> Vec<u8> {
    xterm_mod_param(mods).map_or_else(
        || {
            let intro = if application_cursor { b'O' } else { b'[' };
            vec![0x1b, intro, letter]
        },
        |m| csi_modified_letter(letter, m),
    )
}

fn xterm_tilde_seq(num: u8, mods: Modifiers) -> Vec<u8> {
    let mut out = Vec::with_capacity(8);
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(num.to_string().as_bytes());
    if let Some(m) = xterm_mod_param(mods) {
        out.push(b';');
        out.extend_from_slice(m.to_string().as_bytes());
    }
    out.push(b'~');
    out
}

fn encode_named(named: NamedKey, mods: Modifiers, application_cursor: bool) -> Option<Vec<u8>> {
    // DECCKM swings only the letter-final family, as in xterm's table.
    if let Some(letter) = xterm_csi_letter(named) {
        return Some(xterm_letter_seq(letter, mods, application_cursor));
    }
    if let Some(num) = xterm_csi_tilde_num(named) {
        return Some(xterm_tilde_seq(num, mods));
    }
    if let Some(letter) = xterm_ss3_final(named) {
        return Some(xterm_ss3_letter_seq(letter, mods));
    }
    // `CSI Z` is its own escape, not `\t` with a meta prefix.
    if matches!(named, NamedKey::Tab) && mods.shift_key() {
        return Some(b"\x1b[Z".to_vec());
    }
    let ctrl = mods.control_key() && !mods.super_key();
    let bare: Vec<u8> = match named {
        NamedKey::Enter => b"\r".to_vec(),
        NamedKey::Backspace if ctrl => vec![0x08],
        NamedKey::Backspace => vec![0x7f],
        NamedKey::Tab => b"\t".to_vec(),
        NamedKey::Escape => vec![0x1b],
        NamedKey::Space if ctrl => vec![0x00],
        NamedKey::Space => b" ".to_vec(),
        _ => return None,
    };
    if mods.alt_key() && !mods.super_key() {
        let mut out = Vec::with_capacity(bare.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&bare);
        Some(out)
    } else {
        Some(bare)
    }
}

fn ctrl_byte(s: &str) -> Option<u8> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' => Some(0x00),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

/// Not folded into [`ctrl_byte`]: `modifyOtherKeys` level 1 reads that
/// as "has a legacy byte" and escapes Ctrl+digit / Ctrl+punctuation.
fn legacy_ctrl_byte(s: &str) -> Option<u8> {
    match s {
        "2" => Some(0x00),
        "3" => Some(0x1b),
        "4" => Some(0x1c),
        "5" => Some(0x1d),
        "6" => Some(0x1e),
        "7" | "/" => Some(0x1f),
        "8" | "?" => Some(0x7f),
        _ => ctrl_byte(s),
    }
}

#[cfg(test)]
mod tests;
