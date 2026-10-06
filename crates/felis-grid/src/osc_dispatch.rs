//! OSC dispatch handlers: OSC 8, 66, 4/5/10-12, 52, 9/777/99, and 22.

use super::{
    ClipboardSelection, ClipboardWrite, ClusterText, Grapheme, Grid, NonZeroU16, NonZeroU32,
    SPECIAL_COLOR_COUNT, ThemeChannel, default_dynamic_color, default_palette_color,
    default_special_color, format_osc_52_response, format_osc_color_response, osc8_scheme_allowed,
    parse_osc_8_id, parse_osc_52_selection, parse_palette_index, parse_x_color, sanitize_osc_str,
    special_slot_from_osc4_alias, special_slot_from_osc5,
};
use crate::editing::{continues_cluster, ends_in_pictographic_joiner, is_lone_regional_indicator};
use felis_vt::{split_osc, split_osc_first};

const POINTER_SHAPE_MAX_LEN: usize = 32;

impl Grid {
    pub(crate) fn set_theme_override(&mut self, channel: ThemeChannel, rgb: Option<(u8, u8, u8)>) {
        let idx = channel as usize;
        if self.theme_overrides[idx] != rgb {
            self.theme_overrides[idx] = rgb;
            self.theme_dirty[idx] = true;
        }
    }

    /// `OSC 4 ; index ; spec`: resolve `index` to `rgb` from now on.
    pub(crate) fn set_palette_override(&mut self, index: u8, rgb: (u8, u8, u8)) {
        if self.palette_overrides.insert(index, rgb) != Some(rgb) {
            self.palette_dirty.insert(index);
        }
    }

    /// `OSC 104 ; index`: drop one index's override.
    pub(crate) fn clear_palette_override(&mut self, index: u8) {
        if self.palette_overrides.remove(&index).is_some() {
            self.palette_dirty.insert(index);
        }
    }

    /// Bare `OSC 104` (and DECSCL / RIS). Queued per-index deltas go
    /// with it: the client applies the whole-table reset first.
    pub(crate) fn clear_all_palette_overrides(&mut self) {
        if self.palette_overrides.is_empty() {
            return;
        }
        self.palette_overrides.clear();
        self.palette_dirty.clear();
        self.palette_reset_all_pending = true;
    }

    /// `None` outside any `OSC 8` block.
    #[must_use]
    pub const fn current_link(&self) -> Option<NonZeroU16> {
        self.current_link
    }

    pub(crate) fn intern_cluster(&mut self, text: &str) -> Option<NonZeroU32> {
        self.screen.cluster_table.intern(text)
    }

    pub(crate) fn dispatch_osc_8(&mut self, payload: Option<&[u8]>) {
        let Some(payload) = payload else {
            // Bare `OSC 8` clears, as in xterm.
            self.current_link = None;
            return;
        };
        // REQ-910: every reject clears the pen; leaving the prior link
        // active would let a producer attribute trailing cells to the
        // previous safe URI.
        self.current_link = None;
        let (params_section, uri) = split_osc_first(payload);
        // security-model.md: no control bytes anywhere.
        if params_section.iter().any(|b| *b < 0x20 || *b == 0x7F) {
            return;
        }
        let Some(uri) = sanitize_osc_str(uri.unwrap_or_default()) else {
            return;
        };
        if uri.is_empty() || !osc8_scheme_allowed(uri) {
            return;
        }
        let id = parse_osc_8_id(params_section);
        self.current_link = self.screen.link_table.intern(id.as_deref(), uri);
    }

    /// `OSC 66 ; <metadata> ; <text>`, Kitty text sizing. Skips
    /// silently on a rejected envelope, a full registry, or non-UTF-8
    /// text. Each grapheme cluster is its own sized character, placed
    /// by `place_sized_cluster`.
    pub(crate) fn dispatch_osc_66(&mut self, body: &[u8]) {
        let Some(run) = felis_vt::kitty_text_sizing::parse(body) else {
            return;
        };
        let Ok(text) = std::str::from_utf8(run.text) else {
            return;
        };
        let Some(handle) = self.screen.install_sizing(run.sizing) else {
            return;
        };
        let mut cluster = String::new();
        let mut zwj_armed = false;
        let mut after_base = self.pending_bidi.after_base;
        for c in text.chars() {
            let width = self.screen.grapheme_width(Grapheme::Char(c));
            let armed = std::mem::take(&mut zwj_armed);
            let continues = continues_cluster(
                c,
                width,
                armed,
                after_base,
                || ends_in_pictographic_joiner(&cluster),
                || is_lone_regional_indicator(&cluster),
            );
            if !continues {
                if !cluster.is_empty() {
                    let _ = self.place_sized_cluster(&cluster, run.sizing, handle);
                }
                cluster.clear();
                cluster.push(c);
                cluster.extend(self.pending_bidi.as_slice());
                self.pending_bidi.discard();
                after_base = false;
                continue;
            }
            let is_override = crate::bidi::is_override(c);
            if is_override {
                after_base = true;
            }
            // A payload never extends a cell written before it.
            let fits = !cluster.is_empty() && cluster.len() + c.len_utf8() <= ClusterText::CAP;
            if fits {
                cluster.push(c);
                zwj_armed = c == '\u{200D}';
            } else if is_override {
                self.pending_bidi.push(c);
            }
        }
        if !cluster.is_empty() && self.place_sized_cluster(&cluster, run.sizing, handle) {
            self.zwj_pending = zwj_armed;
        }
        self.pending_bidi.after_base = after_base;
    }

    /// `OSC 10 / 11 / 12` with xterm's multi-spec form: `code` selects
    /// the starting channel and each further spec advances to the next
    /// (fg, bg, cursor). A `?` replies with that channel's own code, so
    /// `OSC 10 ; ? ; ?` produces `OSC 10 ; rgb:...` then `OSC 11 ; rgb:...`.
    pub(crate) fn dispatch_osc_dynamic_color(&mut self, code: u16, specs: Option<&[u8]>) {
        const CHANNELS: [(ThemeChannel, &[u8]); 3] = [
            (ThemeChannel::Foreground, b"10"),
            (ThemeChannel::Background, b"11"),
            (ThemeChannel::Cursor, b"12"),
        ];
        let Some(specs) = specs else {
            return;
        };
        let start = match code {
            10 => 0,
            11 => 1,
            _ => 2,
        };
        for (i, raw) in split_osc(specs).enumerate() {
            let channel_idx = start + i;
            let Some(&(channel, reply_code)) = CHANNELS.get(channel_idx) else {
                break;
            };
            if raw.iter().any(|b| *b < 0x20 || *b == 0x7F) {
                continue;
            }
            if raw == b"?" {
                // Always answered: esctest's `test_ResetSpecialColor_Dynamic`
                // queries before overriding and times out on silence.
                // Override, then the attached client's configured color
                // (so a background detector reads felis's real surface),
                // then the xterm baseline.
                let (r, g, b) = self
                    .theme_override(channel)
                    .or_else(|| self.theme_config_default(channel))
                    .unwrap_or_else(|| default_dynamic_color(channel));
                self.enqueue_response(format_osc_color_response(reply_code, r, g, b));
                continue;
            }
            let Ok(text) = std::str::from_utf8(raw) else {
                continue;
            };
            if let Some(rgb) = parse_x_color(text) {
                self.set_theme_override(channel, Some(rgb));
            }
        }
    }

    /// `OSC 4 / 5 / 104 / 105`. X11 lets one OSC 4 carry many
    /// `idx;spec` pairs and one OSC 104 list many indices. A `?` spec
    /// is a query; unset slots answer with the defaults so a
    /// reset-then-query round-trips.
    pub(crate) fn dispatch_osc_palette(&mut self, code: u16, payload: Option<&[u8]>) {
        // `OSC 104 ST` and `OSC 104 ; ST` both reset the whole table.
        let payload = payload.unwrap_or_default();
        match code {
            4 | 5 => self.dispatch_osc_palette_set_or_query(code, payload),
            104 => self.dispatch_osc_palette_reset(payload, false),
            105 => self.dispatch_osc_palette_reset(payload, true),
            _ => {}
        }
    }

    fn dispatch_osc_palette_set_or_query(&mut self, code: u16, pairs: &[u8]) {
        // A trailing unpaired idx is dropped, as in xterm.
        let mut fields = split_osc(pairs);
        while let (Some(idx_bytes), Some(spec_bytes)) = (fields.next(), fields.next()) {
            let Some(idx) = parse_palette_index(idx_bytes) else {
                continue;
            };
            // xterm accepts a bare `?` only.
            if spec_bytes == b"?" {
                self.respond_palette_query(code, idx);
                continue;
            }
            let Ok(spec_text) = std::str::from_utf8(spec_bytes) else {
                continue;
            };
            let Some(rgb) = parse_x_color(spec_text) else {
                continue;
            };
            self.set_palette_slot(code, idx, rgb);
        }
    }

    fn dispatch_osc_palette_reset(&mut self, indices: &[u8], special: bool) {
        // No idx (the shape esccmd's `ResetColor(c="")` produces) resets
        // the whole table.
        if indices.is_empty() {
            if special {
                self.special_color_overrides = [None; SPECIAL_COLOR_COUNT];
            } else {
                self.clear_all_palette_overrides();
            }
            return;
        }
        for raw in split_osc(indices) {
            let Some(idx) = parse_palette_index(raw) else {
                continue;
            };
            if special {
                if let Some(slot) = special_slot_from_osc5(idx) {
                    self.special_color_overrides[slot] = None;
                }
            } else if u8::try_from(idx).is_ok() {
                self.clear_palette_override(idx as u8);
            } else if let Some(slot) = special_slot_from_osc4_alias(idx) {
                self.special_color_overrides[slot] = None;
            }
        }
    }

    fn set_palette_slot(&mut self, code: u16, idx: u16, rgb: (u8, u8, u8)) {
        // An OSC 4 idx past 255 is xterm's alias for the special colors.
        if code == 5 {
            if let Some(slot) = special_slot_from_osc5(idx) {
                self.special_color_overrides[slot] = Some(rgb);
            }
            return;
        }
        if u8::try_from(idx).is_ok() {
            self.set_palette_override(idx as u8, rgb);
        } else if let Some(slot) = special_slot_from_osc4_alias(idx) {
            self.special_color_overrides[slot] = Some(rgb);
        }
    }

    fn respond_palette_query(&mut self, code: u16, idx: u16) {
        let (r, g, b) = self.palette_lookup(code, idx);
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(b"\x1b]");
        out.extend_from_slice(code.to_string().as_bytes());
        out.push(b';');
        out.extend_from_slice(idx.to_string().as_bytes());
        out.push(b';');
        out.extend_from_slice(b"rgb:");
        out.extend_from_slice(format!("{r:02x}{r:02x}").as_bytes());
        out.push(b'/');
        out.extend_from_slice(format!("{g:02x}{g:02x}").as_bytes());
        out.push(b'/');
        out.extend_from_slice(format!("{b:02x}{b:02x}").as_bytes());
        out.extend_from_slice(b"\x1b\\");
        self.enqueue_response(out);
    }

    fn palette_lookup(&self, code: u16, idx: u16) -> (u8, u8, u8) {
        if code == 5 {
            return special_slot_from_osc5(idx)
                .and_then(|s| self.special_color_overrides[s])
                .unwrap_or_else(default_special_color);
        }
        if u8::try_from(idx).is_ok() {
            return self
                .palette_overrides
                .get(&(idx as u8))
                .copied()
                .unwrap_or_else(|| default_palette_color(idx as u8));
        }
        if let Some(slot) = special_slot_from_osc4_alias(idx) {
            return self.special_color_overrides[slot].unwrap_or_else(default_special_color);
        }
        // An unaddressable idx echoes black so the wire stays well-formed.
        (0, 0, 0)
    }

    /// A write always lands in the session-local mirror (which serves
    /// the `?` readback) and queues a `ClipboardWrite`; whether it also
    /// reaches the OS clipboard is the client's `clipboard.osc_52 =
    /// "system"` opt-in, never decided here.
    pub(crate) fn dispatch_osc_52(&mut self, payload: Option<&[u8]>) {
        let (selection_bytes, data_bytes) = split_osc_first(payload.unwrap_or_default());
        let Some(data_bytes) = data_bytes else {
            return;
        };
        let Some(selection) = parse_osc_52_selection(selection_bytes) else {
            return;
        };
        // A `?` with anything after its `;` is still the query form.
        let sigil = split_osc_first(data_bytes).0;
        if sigil == b"?" {
            let cached = self.clipboard_cache_for(selection);
            // The request's selector chars are echoed verbatim, not the
            // honored subset.
            self.enqueue_response(format_osc_52_response(selection_bytes, cached));
            return;
        }
        if sigil == b"!" {
            if selection.contains(ClipboardSelection::CLIPBOARD) {
                self.clipboard_cache_clipboard = None;
            }
            if selection.contains(ClipboardSelection::PRIMARY) {
                self.clipboard_cache_primary = None;
            }
            return;
        }
        let Some(body) = sanitize_osc_str(data_bytes) else {
            return;
        };
        let Some(data) = felis_protocol::base64::decode(body.as_bytes()) else {
            return;
        };
        if selection.contains(ClipboardSelection::CLIPBOARD) {
            self.clipboard_cache_clipboard = Some(data.clone());
        }
        if selection.contains(ClipboardSelection::PRIMARY) {
            self.clipboard_cache_primary = Some(data.clone());
        }
        self.pending_clipboard_set = Some(ClipboardWrite { selection, data });
    }

    /// Clipboard wins when both are present: xterm's "first selector
    /// with data" rule, with `c` first.
    fn clipboard_cache_for(&self, selection: ClipboardSelection) -> Option<&[u8]> {
        if selection.contains(ClipboardSelection::CLIPBOARD)
            && let Some(v) = self.clipboard_cache_clipboard.as_deref()
        {
            return Some(v);
        }
        if selection.contains(ClipboardSelection::PRIMARY)
            && let Some(v) = self.clipboard_cache_primary.as_deref()
        {
            return Some(v);
        }
        None
    }

    /// OSC 9, iTerm2 notification. A `ConEmu` `OSC 9 ; <digit> ; …`
    /// subcommand is filtered out: a bare-digit first field is never a
    /// human message (`docs/reference/protocols/notifications.md`).
    pub(crate) fn dispatch_osc9(&mut self, body: &[u8]) {
        let (_, message) = split_osc_first(body);
        if let Some(message) = message {
            let (first, rest) = split_osc_first(message);
            if rest.is_some() && first.len() == 1 && first[0].is_ascii_digit() {
                return;
            }
        }
        if let Some(n) = felis_vt::notification::parse_osc9(body) {
            self.relay_notification(n);
        }
    }

    /// OSC 777, rxvt-unicode `notify ; title ; body`.
    pub(crate) fn dispatch_osc777(&mut self, body: &[u8]) {
        if let Some(n) = felis_vt::notification::parse_osc777(body) {
            self.relay_notification(n);
        }
    }

    /// OSC 22, the kitty pointer-shape extension, including its stack
    /// forms (`>` pushes, `<` pops). Ships the first well-formed keyword:
    /// the client maps an unknown keyword to the default arrow.
    pub(crate) fn dispatch_osc_22(&mut self, name: Option<&[u8]>) {
        let raw = match name {
            None | Some(b"") => {
                self.set_pointer_shape(None);
                return;
            }
            Some(raw) => raw,
        };
        match raw.first() {
            Some(b'<') => self.pop_pointer_shape(),
            Some(b'>') => {
                // A push with no valid shape leaves the stack alone.
                if let Some(shape) = Self::first_valid_pointer_shape(&raw[1..]) {
                    self.push_pointer_shape(Some(shape));
                }
            }
            _ => {
                // An all-invalid spec keeps the current shape.
                if let Some(shape) = Self::first_valid_pointer_shape(raw) {
                    self.set_pointer_shape(Some(shape));
                }
            }
        }
    }

    fn first_valid_pointer_shape(spec: &[u8]) -> Option<String> {
        spec.split(|b| *b == b',').find_map(|entry| {
            let valid = (1..=POINTER_SHAPE_MAX_LEN).contains(&entry.len())
                && entry.iter().all(|b| b.is_ascii_lowercase() || *b == b'-');
            if valid {
                core::str::from_utf8(entry).ok().map(str::to_owned)
            } else {
                None
            }
        })
    }

    /// OSC 99, kitty desktop notifications
    /// (`docs/reference/protocols/notifications.md`).
    pub(crate) fn dispatch_osc99(&mut self, body: &[u8]) {
        use felis_vt::notification::{Notification, Osc99, PayloadKind};
        let Some(ev) = felis_vt::notification::parse_osc99(body) else {
            return;
        };
        let (id, done, kind, text, urgency) = match ev {
            Osc99::Query { id } => {
                self.respond_osc99_support(&id);
                return;
            }
            Osc99::Ignored => return,
            Osc99::Payload {
                id,
                done,
                kind,
                text,
                urgency,
            } => (id, done, kind, text, urgency),
        };

        if done && !self.osc99_pending.contains_key(&id) {
            let mut n = Notification {
                title: None,
                body: String::new(),
                urgency: urgency.unwrap_or_default(),
                id: (!id.is_empty()).then(|| id.clone()),
            };
            match kind {
                PayloadKind::Title => n.title = (!text.is_empty()).then_some(text),
                PayloadKind::Body => n.body = text,
            }
            self.relay_notification(n);
            return;
        }

        if !self.osc99_pending.contains_key(&id)
            && self.osc99_pending.len() >= super::NOTIFY_REASSEMBLY_IDS
        {
            return;
        }
        let partial = self.osc99_pending.entry(id.clone()).or_default();
        if let Some(u) = urgency {
            partial.urgency = u;
        }
        match kind {
            PayloadKind::Title => partial.title.push_str(&text),
            PayloadKind::Body => partial.body.push_str(&text),
        }
        if partial.title.len() + partial.body.len() > super::NOTIFY_REASSEMBLY_BYTES {
            self.osc99_pending.remove(&id);
            return;
        }
        if done {
            let partial = self.osc99_pending.remove(&id).unwrap_or_default();
            let n = Notification {
                title: (!partial.title.is_empty()).then_some(partial.title),
                body: partial.body,
                urgency: partial.urgency,
                id: (!id.is_empty()).then_some(id),
            };
            self.relay_notification(n);
        }
    }

    /// A saturated outbox drops the notification and flags the bell,
    /// so the producer sees one signal rather than a silent loss (the
    /// APC-outbox posture).
    fn relay_notification(&mut self, n: felis_vt::notification::Notification) {
        if n.title.is_none() && n.body.is_empty() {
            return;
        }
        if self.notifications.len() >= super::NOTIFY_OUTBOX_CAP {
            self.bell_pending = true;
            return;
        }
        self.notifications.push(n);
    }

    /// The `p=?` reply, in kitty's shape
    /// (`docs/reference/protocols/notifications.md`): title, body, and
    /// the three urgencies, no actions or close events. An absent id
    /// echoes `i=0`.
    fn respond_osc99_support(&mut self, id: &str) {
        let id = if id.is_empty() { "0" } else { id };
        let mut out = Vec::with_capacity(48);
        out.extend_from_slice(b"\x1b]99;i=");
        out.extend_from_slice(id.as_bytes());
        out.extend_from_slice(b":p=?;p=title,body:u=0,1,2\x1b\\");
        self.enqueue_response(out);
    }
}

#[cfg(test)]
mod tests;
