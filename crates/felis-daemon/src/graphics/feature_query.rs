//! Feature queries (`a=q` with `e=<action>`): which keys and values felis
//! honors for an action. The key order and the per-action key sets follow
//! kitty's `supported_keys_for_query` (`kitty/graphics.c`), minus what
//! felis does not implement: `f=8`/`f=16`, `N=`, the placement's `X=`/`Y=`
//! cell offsets, and relative placements (`P=`, `Q=`, `H=`, `V=`).

use felis_vt::kitty_graphics::CompleteCommand;

use super::{control_byte, control_i32, control_u32};

const DELETE_MODES: &[u8] = b"aAiIcCfFnNpPqQrRxXyYzZ";

/// The keys to echo after `i=`, or `None` when `e=` is not one letter
/// (kitty's parser rejects the whole command then, so it gets no reply).
pub(super) fn supported_keys(complete: &CompleteCommand) -> Option<Vec<(u8, String)>> {
    let action = control_byte(complete, b'e').filter(u8::is_ascii_alphabetic)?;
    let mut keys = Keys {
        complete,
        out: Vec::new(),
    };
    if !b"tTqpdfac".contains(&action) {
        return Some(keys.out);
    }
    keys.out.push((b'e', char::from(action).to_string()));
    match action {
        b't' | b'T' | b'q' | b'f' => {
            keys.transmission();
            if action == b'f' {
                keys.uint(b"xycr");
                keys.int(b'z');
                keys.one(b'X');
                keys.uint(b"Y");
            }
            if action == b'T' {
                keys.placement();
            }
        }
        b'p' => keys.placement(),
        b'd' => {
            keys.flag(b'd', DELETE_MODES);
            keys.uint(b"p");
            // `d=r`/`R` read an image id range from x=/y=; the rest a cell.
            if matches!(control_byte(complete, b'd'), Some(b'r' | b'R')) {
                keys.uint(b"xy");
            } else {
                keys.cell(b"xy");
            }
            keys.int(b'z');
        }
        b'a' => {
            if let Some(s) = control_u32(complete, b's').filter(|s| (1..=3).contains(s)) {
                keys.out.push((b's', s.to_string()));
            }
            keys.uint(b"vrc");
            keys.int(b'z');
        }
        b'c' => {
            keys.uint(b"rcxywhXY");
            keys.one(b'C');
        }
        _ => unreachable!("filtered by the action set above"),
    }
    Some(keys.out)
}

struct Keys<'a> {
    complete: &'a CompleteCommand,
    out: Vec<(u8, String)>,
}

impl Keys<'_> {
    fn transmission(&mut self) {
        let media: &[u8] = if cfg!(unix) { b"dfts" } else { b"d" };
        self.flag(b't', media);
        self.flag(b'o', b"z");
        let format = control_u32(self.complete, b'f');
        if let Some(f) = format.filter(|f| [24, 32, 100].contains(f)) {
            self.out.push((b'f', f.to_string()));
        }
        // PNG carries its own size. The byte caps are left to a plain
        // query: the spec scopes a feature query to whether a key is
        // understood, not whether a given load fits.
        if format != Some(100) {
            self.uint(b"sv");
        }
        if matches!(control_byte(self.complete, b't'), Some(b'f' | b't' | b's')) && cfg!(unix) {
            self.uint(b"SO");
        }
    }

    fn placement(&mut self) {
        self.uint(b"pxywh");
        self.cell(b"cr");
        self.one(b'C');
        self.one(b'U');
        self.int(b'z');
    }

    fn flag(&mut self, key: u8, allowed: &[u8]) {
        if let Some(v) = control_byte(self.complete, key).filter(|v| allowed.contains(v)) {
            self.out.push((key, char::from(v).to_string()));
        }
    }

    fn uint(&mut self, keys: &[u8]) {
        self.up_to(keys, u32::MAX);
    }

    /// A cell coordinate or extent, which the handlers read as `u16`.
    fn cell(&mut self, keys: &[u8]) {
        self.up_to(keys, u32::from(u16::MAX));
    }

    fn up_to(&mut self, keys: &[u8], max: u32) {
        for &key in keys {
            if let Some(v) = control_u32(self.complete, key).filter(|&v| v != 0 && v <= max) {
                self.out.push((key, v.to_string()));
            }
        }
    }

    fn int(&mut self, key: u8) {
        if let Some(v) = control_i32(self.complete, key).filter(|&v| v != 0) {
            self.out.push((key, v.to_string()));
        }
    }

    fn one(&mut self, key: u8) {
        if control_u32(self.complete, key) == Some(1) {
            self.out.push((key, "1".to_owned()));
        }
    }
}
