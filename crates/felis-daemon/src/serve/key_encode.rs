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
            ctrl_byte(s)
        } else {
            single_ascii_lower(s)
        };
        if let Some(byte) = body {
            return Some(vec![0x1b, byte]);
        }
    }
    if mods.control_key()
        && !mods.alt_key()
        && !mods.super_key()
        && let Key::Character(s) = key
        && let Some(byte) = ctrl_byte(s)
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
    let bare: Vec<u8> = match named {
        NamedKey::Enter => b"\r".to_vec(),
        NamedKey::Backspace => vec![0x7f],
        NamedKey::Tab => b"\t".to_vec(),
        NamedKey::Escape => vec![0x1b],
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

#[cfg(test)]
mod tests {
    use felis_protocol::messages::FKey;

    use super::*;

    fn lit(s: &str) -> Key {
        Key::Character(s.into())
    }

    /// Every argument of [`super::encode`], with the event kind as the
    /// press flag the legacy encodings are specified in terms of.
    #[allow(clippy::fn_params_excessive_bools)]
    fn encode(
        key: &Key,
        text: Option<&str>,
        mods: Modifiers,
        is_press: bool,
        kitty_kbd_flags: KittyKbdFlags,
        modify_other_keys: ModifyOtherKeys,
        application_cursor: bool,
        application_keypad: bool,
        win32_input_mode: bool,
        key_location: KeyLocation,
    ) -> Option<Vec<u8>> {
        super::encode(
            key,
            text,
            mods,
            if is_press {
                KeyEventKind::Press
            } else {
                KeyEventKind::Release
            },
            kitty_kbd_flags,
            modify_other_keys,
            application_cursor,
            application_keypad,
            win32_input_mode,
            key_location,
        )
    }

    fn encode_no_app_cursor(
        key: &Key,
        text: Option<&str>,
        mods: Modifiers,
        is_press: bool,
        kitty_kbd_flags: KittyKbdFlags,
    ) -> Option<Vec<u8>> {
        encode(
            key,
            text,
            mods,
            is_press,
            kitty_kbd_flags,
            /* modify_other_keys = */ ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ false,
            /* win32_input_mode = */ false,
            /* key_location = */ KeyLocation::Standard,
        )
    }

    fn encode_mok(
        key: &Key,
        text: Option<&str>,
        mods: Modifiers,
        modify_other_keys: ModifyOtherKeys,
    ) -> Option<Vec<u8>> {
        encode(
            key,
            text,
            mods,
            /* is_press = */ true,
            /* kitty_kbd_flags = */ KittyKbdFlags::empty(),
            modify_other_keys,
            /* application_cursor = */ false,
            /* application_keypad = */ false,
            /* win32_input_mode = */ false,
            /* key_location = */ KeyLocation::Standard,
        )
    }

    #[test]
    fn ascii_letter_passes_text_through() {
        let bytes = encode_no_app_cursor(
            &lit("a"),
            Some("a"),
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(b"a".to_vec()));
    }

    #[test]
    fn ctrl_a_maps_to_soh() {
        let bytes = encode_no_app_cursor(
            &lit("a"),
            Some("a"),
            Modifiers::CONTROL,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x01]));
    }

    #[test]
    fn ctrl_uppercase_a_also_maps_to_soh() {
        let bytes = encode_no_app_cursor(
            &lit("A"),
            Some("A"),
            Modifiers::CONTROL,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x01]));
    }

    #[test]
    fn ctrl_left_bracket_maps_to_esc() {
        let bytes = encode_no_app_cursor(
            &lit("["),
            Some("["),
            Modifiers::CONTROL,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x1b]));
    }

    #[test]
    fn alt_letter_emits_meta_prefixed_lowercase() {
        let bytes = encode_no_app_cursor(
            &lit("b"),
            Some("b"),
            Modifiers::ALT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x1b, b'b']));
    }

    #[test]
    fn alt_uppercase_letter_normalizes_to_lowercase_meta_prefix() {
        let bytes = encode_no_app_cursor(
            &lit("A"),
            Some("A"),
            Modifiers::ALT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x1b, b'a']));
    }

    #[test]
    fn alt_period_emits_meta_prefixed_dot() {
        let bytes = encode_no_app_cursor(
            &lit("."),
            Some("."),
            Modifiers::ALT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x1b, b'.']));
    }

    #[test]
    fn ctrl_alt_letter_emits_meta_prefixed_c0() {
        let mods = Modifiers::CONTROL | Modifiers::ALT;
        let bytes = encode_no_app_cursor(&lit("a"), Some("a"), mods, true, KittyKbdFlags::empty());
        assert_eq!(bytes, Some(vec![0x1b, 0x01]));
    }

    #[test]
    fn alt_non_ascii_falls_through_to_text_passthrough() {
        let bytes = encode_no_app_cursor(
            &lit("α"),
            Some("α"),
            Modifiers::ALT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(
            bytes,
            Some("α".as_bytes().to_vec()),
            "non-ASCII Alt+letter must pass through, no spurious ESC"
        );
    }

    #[test]
    fn alt_named_keys_meta_prefix_their_bare_body() {
        let cases: [(NamedKey, &[u8]); 4] = [
            (NamedKey::Backspace, &[0x1b, 0x7f]),
            (NamedKey::Space, b"\x1b "),
            (NamedKey::Escape, &[0x1b, 0x1b]),
            (NamedKey::Tab, b"\x1b\t"),
        ];
        for (key, expected) in cases {
            let bytes = encode_no_app_cursor(
                &Key::Named(key),
                None,
                Modifiers::ALT,
                true,
                KittyKbdFlags::empty(),
            );
            assert_eq!(
                bytes.as_deref(),
                Some(expected),
                "{key:?} under Alt must meta-prefix"
            );
        }
    }

    #[test]
    fn alt_named_keys_super_held_skips_meta_prefix() {
        let mods = Modifiers::ALT | Modifiers::SUPER;
        assert_eq!(
            encode_no_app_cursor(
                &Key::Named(NamedKey::Space),
                None,
                mods,
                true,
                KittyKbdFlags::empty()
            )
            .as_deref(),
            Some(&b" "[..]),
            "Super must short-circuit Space meta-prefix"
        );
    }

    #[test]
    fn shift_tab_takes_priority_over_alt_meta_prefix() {
        let mods = Modifiers::ALT | Modifiers::SHIFT;
        assert_eq!(
            encode_no_app_cursor(
                &Key::Named(NamedKey::Tab),
                None,
                mods,
                true,
                KittyKbdFlags::empty()
            )
            .as_deref(),
            Some(&b"\x1b[Z"[..]),
            "Shift-Tab path must outrank the Alt meta-prefix"
        );
    }

    #[test]
    fn unmodified_named_keys_keep_their_bare_byte() {
        for (key, expected) in [
            (NamedKey::Enter, &b"\r"[..]),
            (NamedKey::Backspace, &[0x7f][..]),
            (NamedKey::Tab, &b"\t"[..]),
            (NamedKey::Escape, &[0x1b][..]),
            (NamedKey::Space, &b" "[..]),
        ] {
            let bytes = encode_no_app_cursor(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{key:?} bare form");
        }
    }

    #[test]
    fn alt_with_super_falls_through_to_text() {
        let mods = Modifiers::ALT | Modifiers::SUPER;
        let bytes = encode_no_app_cursor(&lit("a"), Some("a"), mods, true, KittyKbdFlags::empty());
        assert_eq!(
            bytes,
            Some(b"a".to_vec()),
            "Super present must short-circuit the meta-prefix path"
        );
    }

    #[test]
    fn enter_yields_carriage_return() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn alt_enter_prepends_escape() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::ALT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(b"\x1b\r".to_vec()));
    }

    #[test]
    fn backspace_yields_del() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Backspace),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x7f]));
    }

    #[test]
    fn arrow_keys_emit_csi_cursor_sequences() {
        let cases = [
            (NamedKey::ArrowUp, &b"\x1b[A"[..]),
            (NamedKey::ArrowDown, &b"\x1b[B"[..]),
            (NamedKey::ArrowRight, &b"\x1b[C"[..]),
            (NamedKey::ArrowLeft, &b"\x1b[D"[..]),
        ];
        for (key, expected) in cases {
            let bytes = encode_no_app_cursor(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
        }
    }

    #[test]
    fn unmodified_arrow_keys_keep_bare_legacy_form() {
        for (key, expected) in [
            (NamedKey::ArrowUp, &b"\x1b[A"[..]),
            (NamedKey::ArrowDown, &b"\x1b[B"[..]),
            (NamedKey::Home, &b"\x1b[H"[..]),
            (NamedKey::End, &b"\x1b[F"[..]),
            (NamedKey::PageUp, &b"\x1b[5~"[..]),
            (NamedKey::Delete, &b"\x1b[3~"[..]),
        ] {
            let bytes = encode_no_app_cursor(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
        }
    }

    #[test]
    fn f1_through_f4_emit_ss3_bare_form() {
        let cases: [(NamedKey, &[u8]); 4] = [
            (NamedKey::F(FKey::lit(1)), b"\x1bOP"),
            (NamedKey::F(FKey::lit(2)), b"\x1bOQ"),
            (NamedKey::F(FKey::lit(3)), b"\x1bOR"),
            (NamedKey::F(FKey::lit(4)), b"\x1bOS"),
        ];
        for (key, expected) in cases {
            let bytes = encode_no_app_cursor(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
        }
    }

    #[test]
    fn f5_through_f12_emit_csi_tilde_with_xterm_numbers() {
        let cases: [(NamedKey, &[u8]); 8] = [
            (NamedKey::F(FKey::lit(5)), b"\x1b[15~"),
            (NamedKey::F(FKey::lit(6)), b"\x1b[17~"),
            (NamedKey::F(FKey::lit(7)), b"\x1b[18~"),
            (NamedKey::F(FKey::lit(8)), b"\x1b[19~"),
            (NamedKey::F(FKey::lit(9)), b"\x1b[20~"),
            (NamedKey::F(FKey::lit(10)), b"\x1b[21~"),
            (NamedKey::F(FKey::lit(11)), b"\x1b[23~"),
            (NamedKey::F(FKey::lit(12)), b"\x1b[24~"),
        ];
        for (key, expected) in cases {
            let bytes = encode_no_app_cursor(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
        }
    }

    #[test]
    fn decckm_swaps_bare_arrows_to_ss3_form() {
        let cases: [(NamedKey, &[u8]); 4] = [
            (NamedKey::ArrowUp, b"\x1bOA"),
            (NamedKey::ArrowDown, b"\x1bOB"),
            (NamedKey::ArrowRight, b"\x1bOC"),
            (NamedKey::ArrowLeft, b"\x1bOD"),
        ];
        for (key, expected) in cases {
            let bytes = encode(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
                /* modify_other_keys = */ ModifyOtherKeys::Off,
                /* application_cursor = */ true,
                /* application_keypad = */ false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{key:?}");
        }
    }

    #[test]
    fn decckm_swaps_bare_home_end_to_ss3_form() {
        assert_eq!(
            encode(
                &Key::Named(NamedKey::Home),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
                ModifyOtherKeys::Off,
                true,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            )
            .as_deref(),
            Some(&b"\x1bOH"[..])
        );
        assert_eq!(
            encode(
                &Key::Named(NamedKey::End),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
                ModifyOtherKeys::Off,
                true,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            )
            .as_deref(),
            Some(&b"\x1bOF"[..])
        );
    }

    #[test]
    fn decckm_does_not_affect_tilde_or_function_keys() {
        let pgup = encode(
            &Key::Named(NamedKey::PageUp),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            true,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        assert_eq!(
            pgup.as_deref(),
            Some(&b"\x1b[5~"[..]),
            "tilde-final key unchanged by DECCKM"
        );
        let f1 = encode(
            &Key::Named(NamedKey::F(FKey::lit(1))),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            true,
            false,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        assert_eq!(
            f1.as_deref(),
            Some(&b"\x1bOP"[..]),
            "F1 stays on its native SS3 form regardless of DECCKM"
        );
    }

    #[test]
    fn application_keypad_numpad_digits_emit_ss3() {
        let digits: [(&str, &[u8]); 11] = [
            ("0", b"\x1bOp"),
            ("1", b"\x1bOq"),
            ("2", b"\x1bOr"),
            ("3", b"\x1bOs"),
            ("4", b"\x1bOt"),
            ("5", b"\x1bOu"),
            ("6", b"\x1bOv"),
            ("7", b"\x1bOw"),
            ("8", b"\x1bOx"),
            ("9", b"\x1bOy"),
            (".", b"\x1bOn"),
        ];
        for (ch, expected) in digits {
            let out = encode(
                &Key::Character(ch.into()),
                Some(ch),
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
                ModifyOtherKeys::Off,
                /* application_cursor = */ false,
                /* application_keypad = */ true,
                /* win32_input_mode = */ false,
                KeyLocation::Numpad,
            );
            assert_eq!(out.as_deref(), Some(expected), "numpad {ch} under DECKPAM");
        }
        let enter = encode(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ true,
            /* win32_input_mode = */ false,
            KeyLocation::Numpad,
        );
        assert_eq!(
            enter.as_deref(),
            Some(&b"\x1bOM"[..]),
            "numpad Enter under DECKPAM"
        );
    }

    #[test]
    fn application_keypad_off_numpad_digit_passes_through() {
        let digit = encode(
            &Key::Character("0".into()),
            Some("0"),
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ false,
            /* win32_input_mode = */ false,
            KeyLocation::Numpad,
        );
        assert_eq!(digit.as_deref(), Some(&b"0"[..]));
        let enter = encode(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ false,
            /* win32_input_mode = */ false,
            KeyLocation::Numpad,
        );
        assert_eq!(enter.as_deref(), Some(&b"\r"[..]));
    }

    #[test]
    fn application_keypad_ignored_without_numpad_location() {
        let out = encode(
            &Key::Character("1".into()),
            Some("1"),
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ true,
            /* win32_input_mode = */ false,
            KeyLocation::Standard,
        );
        assert_eq!(out.as_deref(), Some(&b"1"[..]));
    }

    #[test]
    fn kitty_flags_supersede_application_keypad() {
        let out = encode(
            &Key::Character("1".into()),
            Some("1"),
            Modifiers::empty(),
            true,
            KittyKbdFlags::DISAMBIGUATE,
            ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ true,
            /* win32_input_mode = */ false,
            KeyLocation::Numpad,
        );
        assert_eq!(out.as_deref(), Some(&b"1"[..]));
    }

    #[test]
    fn xterm_mod_param_truth_table() {
        assert_eq!(xterm_mod_param(Modifiers::empty()), None);
        assert_eq!(xterm_mod_param(Modifiers::SHIFT), Some(2));
        assert_eq!(xterm_mod_param(Modifiers::ALT), Some(3));
        assert_eq!(xterm_mod_param(Modifiers::SHIFT | Modifiers::ALT), Some(4));
        assert_eq!(xterm_mod_param(Modifiers::CONTROL), Some(5));
        assert_eq!(
            xterm_mod_param(Modifiers::CONTROL | Modifiers::SHIFT),
            Some(6)
        );
        assert_eq!(
            xterm_mod_param(Modifiers::CONTROL | Modifiers::ALT),
            Some(7)
        );
        assert_eq!(
            xterm_mod_param(Modifiers::CONTROL | Modifiers::ALT | Modifiers::SHIFT),
            Some(8)
        );
        assert_eq!(xterm_mod_param(Modifiers::SUPER), Some(9));
    }

    #[test]
    fn shift_tab_emits_csi_z() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Tab),
            None,
            Modifiers::SHIFT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn modifier_only_press_returns_none() {
        let bytes = encode_no_app_cursor(
            &Key::Other,
            None,
            Modifiers::SHIFT,
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, None);
    }

    #[test]
    fn esc_legacy_when_disambiguate_flag_is_off() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::empty(),
        );
        assert_eq!(bytes, Some(vec![0x1b]));
    }

    #[test]
    fn esc_kitty_csi_27_u_when_disambiguate_flag_is_on() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[27u".to_vec()));
    }

    #[test]
    fn esc_kitty_with_alt_modifier_appends_modifier_param() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::ALT,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[27;3u".to_vec()));
    }

    #[test]
    fn esc_kitty_modifier_code_composes_shift_alt_ctrl() {
        let mods = Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL;
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            mods,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[27;8u".to_vec()));
    }

    #[test]
    fn ctrl_letter_under_bit0_uses_csi_u_with_lowercase_codepoint() {
        let bytes = encode_no_app_cursor(
            &lit("I"),
            Some("I"),
            Modifiers::CONTROL,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[105;5u".to_vec()));
        let bytes = encode_no_app_cursor(
            &lit("i"),
            Some("i"),
            Modifiers::CONTROL,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[105;5u".to_vec()));
    }

    #[test]
    fn alt_letter_under_bit0_uses_csi_u_instead_of_esc_prefix() {
        let bytes = encode_no_app_cursor(
            &lit("a"),
            Some("a"),
            Modifiers::ALT,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[97;3u".to_vec()));
    }

    #[test]
    fn ctrl_alt_letter_under_bit0_combines_modifier_bits() {
        let mods = Modifiers::CONTROL | Modifiers::ALT;
        let bytes = encode_no_app_cursor(
            &lit("a"),
            Some("a"),
            mods,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[97;7u".to_vec()));
    }

    #[test]
    fn shift_only_letter_under_bit0_keeps_text_path() {
        let bytes = encode_no_app_cursor(
            &lit("A"),
            Some("A"),
            Modifiers::SHIFT,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"A".to_vec()));
    }

    #[test]
    fn unmodified_tab_enter_backspace_under_bit0_keep_legacy_bytes() {
        let cases: [(NamedKey, &[u8]); 3] = [
            (NamedKey::Tab, b"\t"),
            (NamedKey::Enter, b"\r"),
            (NamedKey::Backspace, &[0x7f]),
        ];
        for (k, expected) in cases {
            let bytes = encode_no_app_cursor(
                &Key::Named(k),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::DISAMBIGUATE,
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{k:?}");
        }
    }

    #[test]
    fn modified_tab_enter_backspace_under_bit0_use_csi_u() {
        let cases: [(NamedKey, &[u8]); 3] = [
            (NamedKey::Tab, b"\x1b[9;5u"),
            (NamedKey::Enter, b"\x1b[13;5u"),
            (NamedKey::Backspace, b"\x1b[127;5u"),
        ];
        for (k, expected) in cases {
            let bytes = encode_no_app_cursor(
                &Key::Named(k),
                None,
                Modifiers::CONTROL,
                true,
                KittyKbdFlags::DISAMBIGUATE,
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{k:?}");
        }
    }

    #[test]
    fn shift_enter_under_bit0_emits_csi_13_2_u() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::SHIFT,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[13;2u".to_vec()));
    }

    #[test]
    fn shift_backspace_under_bit0_emits_csi_127_2_u() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Backspace),
            None,
            Modifiers::SHIFT,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[127;2u".to_vec()));
    }

    #[test]
    fn shift_tab_under_bit0_keeps_csi_z_legacy_form() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Tab),
            None,
            Modifiers::SHIFT,
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn shift_tab_under_bit3_does_route_through_csi_u() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Tab),
            None,
            Modifiers::SHIFT,
            true,
            KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
        );
        assert_eq!(bytes, Some(b"\x1b[9;2u".to_vec()));
    }

    #[test]
    fn release_drops_when_event_types_flag_is_off() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::empty(),
            false,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, None);
    }

    #[test]
    fn release_drops_when_disambiguate_is_off_even_with_event_types() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::empty(),
            false,
            KittyKbdFlags::REPORT_EVENT_TYPES,
        );
        assert_eq!(bytes, None);
    }

    #[test]
    fn esc_release_under_bits_0_and_1_emits_csi_with_event_subparam() {
        let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::empty(),
            false,
            flags,
        );
        assert_eq!(bytes, Some(b"\x1b[27;1:3u".to_vec()));
    }

    #[test]
    fn esc_release_with_modifier_carries_mod_and_event() {
        let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::ALT,
            false,
            flags,
        );
        assert_eq!(bytes, Some(b"\x1b[27;3:3u".to_vec()));
    }

    #[test]
    fn ctrl_letter_release_uses_csi_u_with_event_subparam() {
        let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
        let bytes = encode_no_app_cursor(&lit("i"), Some("i"), Modifiers::CONTROL, false, flags);
        assert_eq!(bytes, Some(b"\x1b[105;5:3u".to_vec()));
    }

    #[test]
    fn release_of_text_path_key_is_dropped() {
        let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_EVENT_TYPES;
        let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::empty(), false, flags);
        assert_eq!(bytes, None);
    }

    #[test]
    fn ctrl_letter_with_associated_text_appends_codepoint() {
        let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
        let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::CONTROL, true, flags);
        assert_eq!(bytes, Some(b"\x1b[97;5;97u".to_vec()));
    }

    #[test]
    fn esc_press_with_associated_text_off_omits_codepoints() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            Some("\x1b"),
            Modifiers::empty(),
            true,
            KittyKbdFlags::DISAMBIGUATE,
        );
        assert_eq!(bytes, Some(b"\x1b[27u".to_vec()));
    }

    #[test]
    fn esc_press_with_associated_text_forces_mod_field() {
        let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            Some("X"),
            Modifiers::empty(),
            true,
            flags,
        );
        assert_eq!(bytes, Some(b"\x1b[27;1;88u".to_vec()));
    }

    #[test]
    fn associated_text_release_does_not_emit_text() {
        let flags = KittyKbdFlags::DISAMBIGUATE
            | KittyKbdFlags::REPORT_EVENT_TYPES
            | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
        let bytes = encode_no_app_cursor(&lit("i"), Some("i"), Modifiers::CONTROL, false, flags);
        assert_eq!(bytes, Some(b"\x1b[105;5:3u".to_vec()));
    }

    #[test]
    fn bit3_routes_unmodified_letter_through_csi_u() {
        let bytes = encode_no_app_cursor(
            &lit("a"),
            Some("a"),
            Modifiers::empty(),
            true,
            KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
        );
        assert_eq!(bytes, Some(b"\x1b[97u".to_vec()));
    }

    #[test]
    fn bit3_implies_bit0_so_unmodified_esc_csi_u() {
        let bytes = encode_no_app_cursor(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::empty(),
            true,
            KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
        );
        assert_eq!(bytes, Some(b"\x1b[27u".to_vec()));
    }

    #[test]
    fn bit3_unmodified_enter_tab_backspace_use_csi_u() {
        let cases: [(NamedKey, &[u8]); 3] = [
            (NamedKey::Tab, b"\x1b[9u"),
            (NamedKey::Enter, b"\x1b[13u"),
            (NamedKey::Backspace, b"\x1b[127u"),
        ];
        for (k, expected) in cases {
            let bytes = encode_no_app_cursor(
                &Key::Named(k),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{k:?}");
        }
    }

    #[test]
    fn bit3_with_shift_letter_keeps_lowercase_keycode_with_shift_mod() {
        let bytes = encode_no_app_cursor(
            &lit("A"),
            Some("A"),
            Modifiers::SHIFT,
            true,
            KittyKbdFlags::REPORT_ALL_AS_ESCAPES,
        );
        assert_eq!(bytes, Some(b"\x1b[97;2u".to_vec()));
    }

    #[test]
    fn bit3_with_associated_text_appends_codepoints() {
        let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
        let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::empty(), true, flags);
        assert_eq!(bytes, Some(b"\x1b[97;1;97u".to_vec()));
    }

    #[test]
    fn bit2_shift_letter_appends_uppercase_alt_keycode() {
        let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_ALTERNATE_KEYS;
        let bytes = encode_no_app_cursor(&lit("A"), Some("A"), Modifiers::SHIFT, true, flags);
        assert_eq!(bytes, Some(b"\x1b[97:65;2u".to_vec()));
    }

    #[test]
    fn bit2_unshifted_letter_omits_alt() {
        let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_ALTERNATE_KEYS;
        let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::empty(), true, flags);
        assert_eq!(bytes, Some(b"\x1b[97u".to_vec()));
    }

    #[test]
    fn bit2_non_letter_shift_omits_alt() {
        let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_ALTERNATE_KEYS;
        let bytes = encode_no_app_cursor(&lit("!"), Some("!"), Modifiers::SHIFT, true, flags);
        assert_eq!(bytes, Some(b"\x1b[33;2u".to_vec()));
    }

    #[test]
    fn bit2_release_drops_alt_subparam() {
        let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES
            | KittyKbdFlags::REPORT_ALTERNATE_KEYS
            | KittyKbdFlags::REPORT_EVENT_TYPES;
        let bytes = encode_no_app_cursor(&lit("A"), Some("A"), Modifiers::SHIFT, false, flags);
        assert_eq!(bytes, Some(b"\x1b[97;2:3u".to_vec()));
    }

    #[test]
    fn bit3_release_under_event_types_emits_csi_u_with_event_subparam() {
        let flags = KittyKbdFlags::REPORT_ALL_AS_ESCAPES | KittyKbdFlags::REPORT_EVENT_TYPES;
        let bytes = encode_no_app_cursor(&lit("a"), Some("a"), Modifiers::empty(), false, flags);
        assert_eq!(bytes, Some(b"\x1b[97;1:3u".to_vec()));
    }

    #[test]
    fn multi_codepoint_associated_text_uses_colon_separator() {
        let flags = KittyKbdFlags::DISAMBIGUATE | KittyKbdFlags::REPORT_ASSOCIATED_TEXT;
        let bytes = encode_no_app_cursor(&lit("a"), Some("AB"), Modifiers::CONTROL, true, flags);
        assert_eq!(bytes, Some(b"\x1b[97;5;65:66u".to_vec()));
    }

    // modifyOtherKeys: a program that could not enable the Kitty protocol
    // (Claude Code gates it on a TERM_PROGRAM allowlist) falls back to
    // level 2 and expects Shift+Enter as a distinct sequence (REQ-506).

    #[test]
    fn shift_enter_under_mok_level_2_emits_csi_13_2_u() {
        let bytes = encode_mok(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::SHIFT,
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(b"\x1b[13;2u".to_vec()));
    }

    #[test]
    fn shift_enter_under_mok_level_1_stays_carriage_return() {
        let bytes = encode_mok(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::SHIFT,
            ModifyOtherKeys::Level1,
        );
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn shift_enter_under_mok_level_0_stays_carriage_return() {
        let bytes = encode_mok(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::SHIFT,
            ModifyOtherKeys::Off,
        );
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn bare_enter_under_mok_level_2_stays_carriage_return() {
        let bytes = encode_mok(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::empty(),
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn bare_escape_under_mok_level_2_stays_esc() {
        let bytes = encode_mok(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::empty(),
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(vec![0x1b]));
    }

    #[test]
    fn modified_escape_under_mok_level_2_emits_csi_27_u() {
        let bytes = encode_mok(
            &Key::Named(NamedKey::Escape),
            None,
            Modifiers::CONTROL,
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(b"\x1b[27;5u".to_vec()));
    }

    #[test]
    fn ctrl_c_under_mok_level_1_keeps_its_control_byte() {
        let bytes = encode_mok(
            &lit("c"),
            Some("c"),
            Modifiers::CONTROL,
            ModifyOtherKeys::Level1,
        );
        assert_eq!(bytes, Some(vec![0x03]));
    }

    #[test]
    fn ctrl_c_under_mok_level_2_routes_csi_u() {
        let bytes = encode_mok(
            &lit("c"),
            Some("c"),
            Modifiers::CONTROL,
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(b"\x1b[99;5u".to_vec()));
    }

    #[test]
    fn ctrl_digit_under_mok_level_1_routes_csi_u() {
        let bytes = encode_mok(
            &lit("1"),
            Some("1"),
            Modifiers::CONTROL,
            ModifyOtherKeys::Level1,
        );
        assert_eq!(bytes, Some(b"\x1b[49;5u".to_vec()));
    }

    #[test]
    fn alt_letter_under_mok_level_1_keeps_meta_prefix() {
        let bytes = encode_mok(
            &lit("b"),
            Some("b"),
            Modifiers::ALT,
            ModifyOtherKeys::Level1,
        );
        assert_eq!(bytes, Some(vec![0x1b, b'b']));
    }

    #[test]
    fn alt_letter_under_mok_level_2_routes_csi_u() {
        let bytes = encode_mok(
            &lit("b"),
            Some("b"),
            Modifiers::ALT,
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(b"\x1b[98;3u".to_vec()));
    }

    #[test]
    fn shift_only_letter_under_mok_level_2_stays_shifted_char() {
        let bytes = encode_mok(
            &lit("A"),
            Some("A"),
            Modifiers::SHIFT,
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(b"A".to_vec()));
    }

    #[test]
    fn ctrl_tab_backspace_space_under_mok_level_2_route_csi_u() {
        let cases: [(NamedKey, &[u8]); 3] = [
            (NamedKey::Tab, b"\x1b[9;5u"),
            (NamedKey::Backspace, b"\x1b[127;5u"),
            (NamedKey::Space, b"\x1b[32;5u"),
        ];
        for (k, expected) in cases {
            let bytes = encode_mok(
                &Key::Named(k),
                None,
                Modifiers::CONTROL,
                ModifyOtherKeys::Level2,
            );
            assert_eq!(bytes.as_deref(), Some(expected), "{k:?}");
        }
    }

    #[test]
    fn shift_tab_under_mok_level_2_keeps_csi_z() {
        let bytes = encode_mok(
            &Key::Named(NamedKey::Tab),
            None,
            Modifiers::SHIFT,
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn modified_arrow_under_mok_stays_xterm_csi_form() {
        let bytes = encode_mok(
            &Key::Named(NamedKey::ArrowRight),
            None,
            Modifiers::CONTROL,
            ModifyOtherKeys::Level2,
        );
        assert_eq!(bytes, Some(b"\x1b[1;5C".to_vec()));
    }

    #[test]
    fn kitty_flags_supersede_modify_other_keys() {
        let bytes = encode(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::SHIFT,
            /* is_press = */ true,
            KittyKbdFlags::REPORT_EVENT_TYPES,
            /* modify_other_keys = */ ModifyOtherKeys::Level2,
            /* application_cursor = */ false,
            /* application_keypad = */ false,
            /* win32_input_mode = */ false,
            /* key_location = */ KeyLocation::Standard,
        );
        assert_eq!(bytes, Some(b"\r".to_vec()));
    }

    #[test]
    fn mok_release_events_produce_nothing() {
        let bytes = encode(
            &Key::Named(NamedKey::Enter),
            None,
            Modifiers::SHIFT,
            false,
            KittyKbdFlags::empty(),
            ModifyOtherKeys::Level2,
            false,
            false,
            false,
            KeyLocation::Standard,
        );
        assert_eq!(bytes, None);
    }

    /// A string of at most `cap` UTF-8 bytes, which is what admission
    /// lets through for a character or its composed text.
    fn capped_utf8(cap: usize) -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::strategy::Strategy as _;
        proptest::collection::vec(proptest::char::any(), 0..=cap).prop_map(move |chars| {
            let mut out = String::new();
            for c in chars {
                if out.len() + c.len_utf8() > cap {
                    break;
                }
                out.push(c);
            }
            out
        })
    }

    fn every_named_key() -> Vec<NamedKey> {
        let mut keys = vec![
            NamedKey::Enter,
            NamedKey::Tab,
            NamedKey::Escape,
            NamedKey::Space,
            NamedKey::Backspace,
            NamedKey::Insert,
            NamedKey::Delete,
            NamedKey::Home,
            NamedKey::End,
            NamedKey::PageUp,
            NamedKey::PageDown,
            NamedKey::ArrowUp,
            NamedKey::ArrowDown,
            NamedKey::ArrowLeft,
            NamedKey::ArrowRight,
        ];
        keys.extend((1..=35).map(|n| NamedKey::F(FKey::lit(n))));
        keys
    }

    proptest::proptest! {

        /// `MAX_KEY_REPORT_BYTES` is what admission reserves before the
        /// mode is known, so no mode may encode past it.
        #[test]
        fn no_mode_encodes_a_capped_key_past_the_reserved_report_size(
            key_idx in 0_usize..52,
            character in capped_utf8(felis_protocol::limits::MAX_KEY_CHARACTER_BYTES),
            text in proptest::option::of(capped_utf8(
                felis_protocol::limits::MAX_KEY_TEXT_BYTES,
            )),
            mods_bits in 0_u8..16,
            kind_idx in 0_usize..3,
            kbd in proptest::num::u8::ANY,
            mok_idx in 0_usize..3,
            application_cursor in proptest::bool::ANY,
            application_keypad in proptest::bool::ANY,
            win32_input_mode in proptest::bool::ANY,
            location_idx in 0_usize..4,
        ) {
            let named = every_named_key();
            let key = match key_idx {
                i if i < named.len() => Key::Named(named[i]),
                i if i == named.len() => Key::Character(character),
                _ => Key::Other,
            };
            let kind = match kind_idx {
                0 => KeyEventKind::Press,
                1 => KeyEventKind::Repeat,
                _ => KeyEventKind::Release,
            };
            let location = match location_idx {
                0 => KeyLocation::Standard,
                1 => KeyLocation::Left,
                2 => KeyLocation::Right,
                _ => KeyLocation::Numpad,
            };
            let modify_other_keys = match mok_idx {
                0 => ModifyOtherKeys::Off,
                1 => ModifyOtherKeys::Level1,
                _ => ModifyOtherKeys::Level2,
            };
            let encoded = super::encode(
                &key,
                text.as_deref(),
                Modifiers::from_bits_truncate(mods_bits),
                kind,
                KittyKbdFlags::from_bits_truncate(kbd),
                modify_other_keys,
                application_cursor,
                application_keypad,
                win32_input_mode,
                location,
            );
            let len = encoded.as_ref().map_or(0, Vec::len);
            proptest::prop_assert!(
                len <= felis_protocol::limits::MAX_KEY_REPORT_BYTES,
                "{key:?} under kbd={kbd} mok={modify_other_keys:?} win32={win32_input_mode} \
                 encoded to {len} bytes, past the reserved \
                 {}",
                felis_protocol::limits::MAX_KEY_REPORT_BYTES,
            );
        }

        #[test]
        fn encode_named_total_across_modifiers_and_modes(
            key_idx in 0_usize..15,
            shift in proptest::bool::ANY,
            alt in proptest::bool::ANY,
            ctrl in proptest::bool::ANY,
            sup in proptest::bool::ANY,
            kbd in proptest::num::u8::ANY,
            mok_idx in 0_usize..3,
            app_cursor in proptest::bool::ANY,
            is_press in proptest::bool::ANY,
        ) {
            let key = match key_idx {
                0 => NamedKey::ArrowUp,
                1 => NamedKey::ArrowDown,
                2 => NamedKey::ArrowLeft,
                3 => NamedKey::ArrowRight,
                4 => NamedKey::Home,
                5 => NamedKey::End,
                6 => NamedKey::PageUp,
                7 => NamedKey::PageDown,
                8 => NamedKey::Insert,
                9 => NamedKey::Delete,
                10 => NamedKey::F(FKey::lit(1)),
                11 => NamedKey::F(FKey::lit(4)),
                12 => NamedKey::F(FKey::lit(5)),
                13 => NamedKey::F(FKey::lit(12)),
                _ => NamedKey::Escape,
            };
            let mut mods = Modifiers::empty();
            if shift { mods |= Modifiers::SHIFT; }
            if alt { mods |= Modifiers::ALT; }
            if ctrl { mods |= Modifiers::CONTROL; }
            if sup { mods |= Modifiers::SUPER; }
            let mok = match mok_idx {
                0 => ModifyOtherKeys::Off,
                1 => ModifyOtherKeys::Level1,
                _ => ModifyOtherKeys::Level2,
            };
            drop(encode(
                &Key::Named(key),
                None,
                mods,
                is_press,
                KittyKbdFlags::from_bits_truncate(kbd),
                mok,
                app_cursor,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            ));
        }

        #[test]
        fn bare_letter_cursor_keys_pick_intro_byte_by_decckm(
            key_idx in 0_usize..6,
            app_cursor in proptest::bool::ANY,
        ) {
            let key = match key_idx {
                0 => NamedKey::ArrowUp,
                1 => NamedKey::ArrowDown,
                2 => NamedKey::ArrowLeft,
                3 => NamedKey::ArrowRight,
                4 => NamedKey::Home,
                _ => NamedKey::End,
            };
            let bytes = encode(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                /* is_press = */ true,
                /* kitty_kbd_flags = */ KittyKbdFlags::empty(),
                /* modify_other_keys = */ ModifyOtherKeys::Off,
                app_cursor,
                /* application_keypad = */ false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            )
            .expect("named cursor key always encodes");
            let intro = if app_cursor { 0x4f } else { 0x5b }; // 'O' or '['
            proptest::prop_assert_eq!(bytes[0], 0x1b);
            proptest::prop_assert_eq!(bytes[1], intro, "intro byte under DECCKM={}", app_cursor);
            proptest::prop_assert_eq!(bytes.len(), 3, "bare form is exactly 3 bytes");
        }

        /// xterm's modified form is the bare form with the modifier
        /// parameter spliced in: the family and the final byte come from
        /// the bare tables, so only the parameter is new. DECCKM swings
        /// the bare form alone.
        #[test]
        fn modified_named_keys_splice_the_modifier_into_the_bare_form(
            key_idx in 0_usize..14,
            shift in proptest::bool::ANY,
            alt in proptest::bool::ANY,
            ctrl in proptest::bool::ANY,
            sup in proptest::bool::ANY,
            app_cursor in proptest::bool::ANY,
        ) {
            proptest::prop_assume!(shift || alt || ctrl || sup);
            let named = match key_idx {
                0 => NamedKey::ArrowUp,
                1 => NamedKey::ArrowDown,
                2 => NamedKey::ArrowLeft,
                3 => NamedKey::ArrowRight,
                4 => NamedKey::Home,
                5 => NamedKey::End,
                6 => NamedKey::PageUp,
                7 => NamedKey::PageDown,
                8 => NamedKey::Insert,
                9 => NamedKey::Delete,
                10 => NamedKey::F(FKey::lit(1)),
                11 => NamedKey::F(FKey::lit(4)),
                12 => NamedKey::F(FKey::lit(5)),
                _ => NamedKey::F(FKey::lit(12)),
            };
            let key = Key::Named(named);
            let mut mods = Modifiers::empty();
            if shift { mods |= Modifiers::SHIFT; }
            if alt { mods |= Modifiers::ALT; }
            if ctrl { mods |= Modifiers::CONTROL; }
            if sup { mods |= Modifiers::SUPER; }

            let bare = encode_no_app_cursor(&key, None, Modifiers::empty(), true, KittyKbdFlags::empty())
                .expect("every key in the xterm tables encodes bare");
            let modified = encode_no_app_cursor(&key, None, mods, true, KittyKbdFlags::empty())
                .expect("every key in the xterm tables encodes modified");
            let under_decckm = encode(
                &key,
                None,
                mods,
                true,
                KittyKbdFlags::empty(),
                ModifyOtherKeys::Off,
                app_cursor,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            );
            proptest::prop_assert_eq!(under_decckm.as_deref(), Some(&modified[..]));

            let m = xterm_mod_param(mods).expect("a held modifier has an xterm parameter");
            let final_byte = *bare.last().expect("a non-empty encoding");
            let leading = if final_byte == b'~' {
                String::from_utf8(bare[2..bare.len() - 1].to_vec()).expect("digits")
            } else {
                "1".to_owned()
            };
            let expected = format!("\x1b[{leading};{m}{}", char::from(final_byte));
            proptest::prop_assert_eq!(
                String::from_utf8_lossy(&modified),
                expected,
            );
        }

        #[test]
        fn modified_letter_cursor_keys_stay_csi_under_decckm(
            key_idx in 0_usize..6,
            shift in proptest::bool::ANY,
            alt in proptest::bool::ANY,
            ctrl in proptest::bool::ANY,
            app_cursor in proptest::bool::ANY,
        ) {
            proptest::prop_assume!(shift || alt || ctrl);
            let key = match key_idx {
                0 => NamedKey::ArrowUp,
                1 => NamedKey::ArrowDown,
                2 => NamedKey::ArrowLeft,
                3 => NamedKey::ArrowRight,
                4 => NamedKey::Home,
                _ => NamedKey::End,
            };
            let mut mods = Modifiers::empty();
            if shift { mods |= Modifiers::SHIFT; }
            if alt { mods |= Modifiers::ALT; }
            if ctrl { mods |= Modifiers::CONTROL; }
            let bytes = encode(
                &Key::Named(key),
                None,
                mods,
                true,
                KittyKbdFlags::empty(),
                ModifyOtherKeys::Off,
                app_cursor,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            )
            .expect("named cursor key always encodes");
            proptest::prop_assert!(
                bytes.starts_with(b"\x1b[1;"),
                "modified letter-final must stay CSI even under DECCKM, got {:?}",
                bytes
            );
        }

        #[test]
        fn non_letter_cursor_keys_unaffected_by_decckm(
            key_idx in 0_usize..10,
        ) {
            let key = match key_idx {
                0 => NamedKey::PageUp,
                1 => NamedKey::PageDown,
                2 => NamedKey::Insert,
                3 => NamedKey::Delete,
                4 => NamedKey::F(FKey::lit(1)),
                5 => NamedKey::F(FKey::lit(4)),
                6 => NamedKey::F(FKey::lit(5)),
                7 => NamedKey::F(FKey::lit(8)),
                8 => NamedKey::F(FKey::lit(11)),
                _ => NamedKey::F(FKey::lit(12)),
            };
            let off = encode(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
                ModifyOtherKeys::Off,
                false,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            )
            .expect("encode never None for these named keys");
            let on = encode(
                &Key::Named(key),
                None,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
                ModifyOtherKeys::Off,
                true,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            )
            .expect("encode never None for these named keys");
            proptest::prop_assert_eq!(
                off, on,
                "DECCKM must not change tilde-final / F1-F4 byte shape"
            );
        }

        #[test]
        fn alt_ascii_press_meta_prefixes_when_super_off(
            byte in 0x21_u8..=0x7e_u8,
            shift in proptest::bool::ANY,
            ctrl in proptest::bool::ANY,
        ) {
            let s = std::str::from_utf8(&[byte]).unwrap().to_owned();
            let mut mods = Modifiers::ALT;
            if shift { mods |= Modifiers::SHIFT; }
            if ctrl { mods |= Modifiers::CONTROL; }
            let key = Key::Character(s.clone());
            let bytes = encode_no_app_cursor(&key, Some(s.as_str()), mods, true, KittyKbdFlags::empty())
                .expect("printable ASCII always encodes under Alt");
            let body_resolves = if ctrl {
                ctrl_byte(&s).is_some()
            } else {
                single_ascii_lower(&s).is_some()
            };
            if body_resolves {
                proptest::prop_assert_eq!(
                    bytes[0], 0x1b,
                    "Alt+ASCII byte={} mods={:?} must meta-prefix", byte, mods
                );
                proptest::prop_assert_eq!(bytes.len(), 2, "meta-prefixed shape is exactly 2 bytes");
            }
        }

        #[test]
        fn alt_with_super_never_meta_prefixes(
            byte in 0x61_u8..=0x7a_u8, // 'a'..='z'
        ) {
            let s = std::str::from_utf8(&[byte]).unwrap().to_owned();
            let mods = Modifiers::ALT | Modifiers::SUPER;
            let key = Key::Character(s.clone());
            let bytes = encode_no_app_cursor(&key, Some(s.as_str()), mods, true, KittyKbdFlags::empty())
                .expect("ASCII letter always encodes");
            proptest::prop_assert_ne!(
                bytes[0], 0x1b,
                "Super+Alt+{} must not meta-prefix", char::from(byte)
            );
        }

        #[test]
        fn alt_named_press_meta_prefixes_when_super_off(
            key_idx in 0_usize..4,
            shift in proptest::bool::ANY,
            ctrl in proptest::bool::ANY,
        ) {
            let key = match key_idx {
                0 => NamedKey::Backspace,
                1 => NamedKey::Space,
                2 => NamedKey::Escape,
                _ => NamedKey::Tab,
            };
            let mut mods = Modifiers::ALT;
            if shift { mods |= Modifiers::SHIFT; }
            if ctrl { mods |= Modifiers::CONTROL; }
            if matches!(key, NamedKey::Tab) && shift {
                return Ok(());
            }
            let bytes = encode_no_app_cursor(&Key::Named(key), None, mods, true, KittyKbdFlags::empty())
                .expect("named-key arms always encode");
            proptest::prop_assert_eq!(
                bytes[0], 0x1b,
                "Alt+{:?} mods={:?} must meta-prefix", key, mods
            );
            proptest::prop_assert_eq!(
                bytes.len(), 2,
                "Alt+named bare-body shape is exactly 2 bytes (ESC + body)"
            );
        }

        #[test]
        fn mok_bare_keys_match_legacy_across_levels(
            key_idx in 0_usize..16,
            level_idx in 0_usize..3,
        ) {
            let level = match level_idx {
                0 => ModifyOtherKeys::Off,
                1 => ModifyOtherKeys::Level1,
                _ => ModifyOtherKeys::Level2,
            };
            let key = match key_idx {
                0 => Key::Named(NamedKey::Enter),
                1 => Key::Named(NamedKey::Tab),
                2 => Key::Named(NamedKey::Backspace),
                3 => Key::Named(NamedKey::Space),
                4 => Key::Named(NamedKey::Escape),
                5 => Key::Named(NamedKey::ArrowUp),
                6 => Key::Named(NamedKey::Home),
                7 => Key::Named(NamedKey::PageUp),
                8 => Key::Named(NamedKey::F(FKey::lit(1))),
                9 => Key::Named(NamedKey::F(FKey::lit(5))),
                10 => lit("a"),
                11 => lit("Z"),
                12 => lit("1"),
                13 => lit("."),
                14 => lit(" "),
                _ => lit("/"),
            };
            let text = if let Key::Character(s) = &key { Some(s.as_str()) } else { None };
            let legacy = encode(
                &key,
                text,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
                ModifyOtherKeys::Off,
                false,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            );
            let with_mok = encode(
                &key,
                text,
                Modifiers::empty(),
                true,
                KittyKbdFlags::empty(),
                level,
                false,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            );
            proptest::prop_assert_eq!(
                legacy, with_mok,
                "bare {:?} must be identical under modifyOtherKeys level {:?}", key, level
            );
        }

        #[test]
        fn mok_level_2_enter_modifier_param_matches_formula(
            shift in proptest::bool::ANY,
            alt in proptest::bool::ANY,
            ctrl in proptest::bool::ANY,
            sup in proptest::bool::ANY,
        ) {
            proptest::prop_assume!(shift || alt || ctrl || sup);
            let mut mods = Modifiers::empty();
            let mut bits = 0u32;
            if shift { mods |= Modifiers::SHIFT; bits |= 1; }
            if alt { mods |= Modifiers::ALT; bits |= 2; }
            if ctrl { mods |= Modifiers::CONTROL; bits |= 4; }
            if sup { mods |= Modifiers::SUPER; bits |= 8; }
            let bytes = encode_mok(&Key::Named(NamedKey::Enter), None, mods, ModifyOtherKeys::Level2)
                .expect("modified Enter always encodes under level 2");
            let expected = format!("\x1b[13;{}u", bits + 1).into_bytes();
            proptest::prop_assert_eq!(bytes, expected);
        }

        #[test]
        fn nonzero_kitty_flags_make_mok_a_noop(
            key_idx in 0_usize..6,
            kbd in 1_u8..=KittyKbdFlags::all().bits(),
            shift in proptest::bool::ANY,
            ctrl in proptest::bool::ANY,
            is_press in proptest::bool::ANY,
        ) {
            let key = match key_idx {
                0 => Key::Named(NamedKey::Enter),
                1 => Key::Named(NamedKey::Tab),
                2 => Key::Named(NamedKey::Escape),
                3 => lit("c"),
                4 => lit("1"),
                _ => Key::Named(NamedKey::Space),
            };
            let mut mods = Modifiers::empty();
            if shift { mods |= Modifiers::SHIFT; }
            if ctrl { mods |= Modifiers::CONTROL; }
            let text = if let Key::Character(s) = &key { Some(s.as_str()) } else { None };
            let without = encode(
                &key,
                text,
                mods,
                is_press,
                KittyKbdFlags::from_bits_truncate(kbd),
                ModifyOtherKeys::Off,
                false,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            );
            let with = encode(
                &key,
                text,
                mods,
                is_press,
                KittyKbdFlags::from_bits_truncate(kbd),
                ModifyOtherKeys::Level2,
                false,
                false,
                /* win32_input_mode = */ false,
                KeyLocation::Standard,
            );
            proptest::prop_assert_eq!(
                without, with,
                "non-zero Kitty flags {} must make modifyOtherKeys a no-op", kbd
            );
        }
    }

    fn win32(key: &Key, text: Option<&str>, mods: Modifiers, is_press: bool) -> Option<Vec<u8>> {
        encode(
            key,
            text,
            mods,
            is_press,
            /* kitty_kbd_flags = */ KittyKbdFlags::empty(),
            /* modify_other_keys = */ ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ false,
            /* win32_input_mode = */ true,
            KeyLocation::Standard,
        )
    }

    #[test]
    fn win32_input_letter_reports_vk_char_and_keydown() {
        let out = win32(
            &Key::Character("a".into()),
            Some("a"),
            Modifiers::empty(),
            true,
        );
        assert_eq!(out.as_deref(), Some(&b"\x1b[65;0;97;1;0;1_"[..]));
    }

    #[test]
    fn win32_input_reports_release_with_keydown_zero() {
        let out = win32(
            &Key::Character("a".into()),
            Some("a"),
            Modifiers::empty(),
            false,
        );
        assert_eq!(out.as_deref(), Some(&b"\x1b[65;0;97;0;0;1_"[..]));
    }

    #[test]
    fn win32_input_enter_carries_vk_return_and_cr() {
        let out = win32(&Key::Named(NamedKey::Enter), None, Modifiers::empty(), true);
        assert_eq!(out.as_deref(), Some(&b"\x1b[13;0;13;1;0;1_"[..]));
    }

    #[test]
    fn win32_input_ctrl_c_sets_control_key_state() {
        let out = win32(
            &Key::Character("c".into()),
            Some("c"),
            Modifiers::CONTROL,
            true,
        );
        assert_eq!(out.as_deref(), Some(&b"\x1b[67;0;99;1;8;1_"[..]));
    }

    #[test]
    fn win32_input_shift_sets_shift_pressed_bit() {
        let out = win32(
            &Key::Character("A".into()),
            Some("A"),
            Modifiers::SHIFT,
            true,
        );
        assert_eq!(out.as_deref(), Some(&b"\x1b[65;0;65;1;16;1_"[..]));
    }

    #[test]
    fn win32_input_arrow_reports_vk_with_no_char() {
        let out = win32(
            &Key::Named(NamedKey::ArrowUp),
            None,
            Modifiers::empty(),
            true,
        );
        assert_eq!(out.as_deref(), Some(&b"\x1b[38;0;0;1;0;1_"[..]));
    }

    #[test]
    fn win32_input_bare_modifier_emits_nothing() {
        assert_eq!(win32(&Key::Other, None, Modifiers::SHIFT, true), None);
    }

    #[test]
    fn win32_input_supersedes_kitty_flags() {
        let out = encode(
            &Key::Character("a".into()),
            Some("a"),
            Modifiers::empty(),
            true,
            /* kitty_kbd_flags = */ KittyKbdFlags::DISAMBIGUATE,
            /* modify_other_keys = */ ModifyOtherKeys::Off,
            /* application_cursor = */ false,
            /* application_keypad = */ false,
            /* win32_input_mode = */ true,
            KeyLocation::Standard,
        );
        assert_eq!(out.as_deref(), Some(&b"\x1b[65;0;97;1;0;1_"[..]));
    }
}
