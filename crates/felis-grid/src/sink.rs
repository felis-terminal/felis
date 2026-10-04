//! The `felis_vt::Sink` implementation for `Grid`: the parser entry
//! points and the CSI/ESC/DCS/OSC/APC dispatch tables.

use super::{
    AttrFlags, Cell, DA1_REPLY, DCS_BUFFER_LIMIT, DcsKind, DcsState, ErasedRange, Grapheme, Grid,
    ModifyOtherKeys, MouseEncoding, MouseProtocol, PromptMark, PtyEffect, SPECIAL_COLOR_COUNT,
    Sink, ThemeChannel, parse_osc_133, sanitize_osc_str,
};
use crate::editing::{PendingBidi, blank_cells, is_emoji_modifier, is_regional_indicator};
use felis_vt::{osc_number, split_osc_first};
use std::sync::LazyLock;

fn set_if_changed(field: &mut Option<String>, value: &str) -> bool {
    if field.as_deref() == Some(value) {
        return false;
    }
    *field = Some(value.to_owned());
    true
}

/// Printable ASCII must land as `Grapheme::Ascii`, the representation
/// the byte paths produce, or a mixed run's ASCII spans diverge from
/// per-byte chunking (`bulk_utf8_run_is_chunking_invariant`).
#[inline]
const fn grapheme_for_char(c: char) -> Grapheme {
    if c.is_ascii() {
        Grapheme::Ascii(c as u8)
    } else {
        Grapheme::Char(c)
    }
}

/// `0` defers char to `put_grapheme`: width 0, or a char that can fold
/// into the previous grapheme. Nonzero answers must match
/// `Grid::grapheme_width`.
pub(crate) fn bulk_width_or_defer(c: char) -> usize {
    use unicode_width::UnicodeWidthChar;
    if is_emoji_modifier(c) || is_regional_indicator(c) {
        0
    } else {
        c.width().unwrap_or(0)
    }
}

/// `bulk_width_or_defer` for every BMP scalar, two bits each. The batch
/// loop needs its width from a load, not a call: an opaque call between
/// cell stores makes the compiler reload `cells` on every store.
static BMP_WIDTHS: LazyLock<Box<[u8; 0x4000]>> = LazyLock::new(|| {
    let mut table = Box::new([0u8; 0x4000]);
    for cp in 0..0x1_0000u32 {
        let w = char::from_u32(cp).map_or(0, bulk_width_or_defer);
        table[(cp >> 2) as usize] |= (w as u8) << ((cp & 3) * 2);
    }
    table
});

pub(crate) fn bmp_widths() -> &'static [u8; 0x4000] {
    &BMP_WIDTHS
}

/// Same answer as `bulk_width_or_defer`.
#[inline]
pub(crate) fn table_width(table: &[u8; 0x4000], c: char) -> usize {
    let cp = c as u32;
    if cp < 0x1_0000 {
        usize::from((table[(cp >> 2) as usize] >> ((cp & 3) * 2)) & 3)
    } else {
        bulk_width_or_defer(c)
    }
}

/// Only the run's two edges can split a wide pair; a pair inside it is
/// overwritten whole. Past the pre-write watermark `occ` the row holds a
/// recycled slot's stale tenant, so an edge there is not probed: a stale
/// `Spacer` at `start` would otherwise blank the live cell to its left.
#[inline]
fn store_ascii_run(row: &mut [Cell], start: usize, occ: usize, bytes: &[u8], pen: Cell) {
    let end = start + bytes.len();
    if start > 0 && start < occ && matches!(row[start].grapheme, Grapheme::Spacer) {
        row[start - 1] = Cell::default();
    }
    if end < occ && matches!(row[end].grapheme, Grapheme::Spacer) {
        row[end] = Cell::default();
    }
    for (dst, &b) in row[start..end].iter_mut().zip(bytes) {
        *dst = Cell {
            grapheme: Grapheme::Ascii(b),
            ..pen
        };
    }
}

impl Sink for Grid {
    fn print(&mut self, byte: u8) {
        if (0x20..=0x7E).contains(&byte) {
            self.put_grapheme(Grapheme::Ascii(byte));
            return;
        }
        let mut produced: Option<char> = None;
        self.utf8.push(byte, |c| {
            produced = Some(c);
        });
        if let Some(c) = produced {
            self.put_grapheme(Grapheme::Char(c));
        }
    }

    /// `#[inline(never)]`: the parser inlines its `Sink` dispatch, and
    /// folding this body into the `advance` loop bloats codegen that is
    /// acutely sensitive to code size.
    #[inline(never)]
    fn print_str(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // An ASCII base never joins a ZWJ sequence, and the fast loops do
        // not go through `put_grapheme`, which would disarm it.
        self.zwj_pending = false;
        // The fast loops also skip the fold of a pending override.
        let bytes = if self.pending_bidi.is_empty() {
            bytes
        } else {
            self.print(bytes[0]);
            &bytes[1..]
        };
        if bytes.is_empty() {
            return;
        }
        // Each of these modes mutates state the tight loop does not
        // model; `has_sized_cells` is the conservative gate for an OSC 66
        // overprint, which must erase the whole run.
        if self.left_right_margin_mode
            || self.insert_mode
            || self.screen.has_sized_cells
            || self.current_sizing_handle.is_some()
            || !self.autowrap
        {
            for &b in bytes {
                self.print(b);
            }
            return;
        }
        // When `col > occ`, cursor sits over stale recycled row tail.
        // Fork to cold copy so fast loop avoids gap-fill stores that
        // prevent pointer hoisting. Only the run's start can gap since
        // wraps reset column to zero.
        let start = usize::from(self.screen.cursor.col);
        if start != 0 {
            let phys = self.screen.phys_row(self.screen.cursor.row);
            if start > usize::from(self.screen.occupancy[phys]) {
                self.print_ascii_bulk_gap(bytes, phys, start);
                return;
            }
        }
        let cols = usize::from(self.screen.cols);
        if cols == 0 {
            return;
        }
        let right_edge = cols - 1;
        let pen = Cell {
            grapheme: Grapheme::Empty,
            style: self.pen_style,
            link: self.current_link,
            sizing: None,
        };
        let mut i = 0;
        let mut last_byte: Option<u8> = None;
        while i < bytes.len() {
            if self.screen.cursor.pending_wrap {
                self.line_feed();
                self.screen.cursor.col = 0;
                self.screen.cursor.pending_wrap = false;
                self.screen.set_soft_wrap(self.screen.cursor.row, true);
            }
            let row = self.screen.cursor.row;
            let row_base = self.screen.idx(row, 0);
            let start_col = usize::from(self.screen.cursor.col);
            let room = cols - start_col;
            let take = (bytes.len() - i).min(room);
            let chunk_end = i + take;
            let occ = usize::from(self.screen.occupancy[row_base / cols]);
            store_ascii_run(
                &mut self.screen.cells[row_base..row_base + cols],
                start_col,
                occ,
                &bytes[i..chunk_end],
                pen,
            );
            let col = start_col + take;
            let j = chunk_end;
            self.screen
                .occ_bump_phys(row_base / cols, u16::try_from(col).unwrap_or(u16::MAX));
            self.screen.damage.mark(usize::from(row));
            last_byte = Some(bytes[j - 1]);
            i = j;
            if col == cols {
                self.screen.cursor.col = u16::try_from(right_edge).unwrap_or(u16::MAX);
                self.screen.cursor.pending_wrap = true;
            } else {
                self.screen.cursor.col = u16::try_from(col).unwrap_or(u16::MAX);
            }
        }
        if let Some(b) = last_byte {
            // REP anchors on the final graphic print.
            self.last_printed = Some(Grapheme::Ascii(b));
        }
    }

    /// A single validation pass over the run replaces the per-byte
    /// decoder state machine (~40% of the unicode print cost).
    fn print_utf8_run(&mut self, bytes: &[u8]) {
        // Continuation bytes of a sequence split across the previous
        // chunk lead this run; drain them per byte until the decoder is
        // empty. (`execute` resets the decoder on any control byte, so
        // this arises only at a raw read boundary.)
        let mut start = 0;
        if self.utf8.is_pending() {
            while start < bytes.len() {
                let b = bytes[start];
                start += 1;
                self.print(b);
                if !self.utf8.is_pending() {
                    break;
                }
            }
        }
        let rest = &bytes[start..];
        if rest.is_empty() {
            return;
        }
        if let Ok(s) = simdutf8::basic::from_utf8(rest) {
            self.print_wide_str(s);
            return;
        }
        // `basic` reports no error position; only an invalid run pays the
        // second, positional pass.
        match std::str::from_utf8(rest) {
            Ok(s) => self.print_wide_str(s),
            Err(e) => {
                let valid = e.valid_up_to();
                if let Ok(s) = std::str::from_utf8(&rest[..valid]) {
                    self.print_wide_str(s);
                }
                for &b in &rest[valid..] {
                    self.print(b);
                }
            }
        }
    }

    fn execute(&mut self, byte: u8) {
        self.utf8.reset();
        // xterm scopes REP to consecutive graphic prints.
        self.last_printed = None;
        // BEL leaves the cursor at the fold site, so a joiner stays armed
        // across it, as in kitty.
        if matches!(byte, 0x08..=0x0D) {
            self.zwj_pending = false;
        }
        if byte != 0x07 {
            self.pending_bidi = PendingBidi::default();
        }
        match byte {
            // Multiple BELs in a chunk coalesce into one
            // `GridMsg::Attention` so a chord of beeps does not strobe
            // the taskbar.
            0x07 => self.bell_pending = true,
            0x08 => self.backspace(),
            0x09 => self.tab(),
            0x0A..=0x0C => {
                // LNM (ANSI mode 20): LF / VT / FF perform a CR first;
                // esctest's `test_SM_LNM` exercises all three bytes.
                if self.linefeed_newline_mode {
                    self.carriage_return();
                }
                self.line_feed();
            }
            0x0D => self.carriage_return(),
            _ => {}
        }
    }

    #[expect(
        clippy::match_same_arms,
        reason = "distinct VT final bytes documented separately despite a shared handler"
    )]
    fn csi_dispatch(
        &mut self,
        params: &[u16],
        subparams: u32,
        intermediates: &[u8],
        ignore: bool,
        final_byte: u8,
    ) {
        let p1 = params.first().copied().unwrap_or(0);
        let p2 = params.get(1).copied().unwrap_or(0);
        // REP scopes its anchor to consecutive graphic prints; REP itself
        // re-anchors via `put_grapheme` so a chain keeps repeating.
        let is_rep = intermediates.is_empty() && final_byte == b'b';
        if !is_rep {
            self.last_printed = None;
        }
        // SGR alone keeps it: a highlighter colors the text right after
        // the override.
        if !(intermediates.is_empty() && final_byte == b'm') {
            self.pending_bidi = PendingBidi::default();
        }
        // REQ-903 exceed → discard: `ignore` marks a sequence that
        // overran `MAX_PARAMS` / `MAX_INTERMEDIATES`; acting on the
        // truncated prefix would misread the producer (an SGR clipped
        // mid-color), the posture xterm takes.
        if ignore {
            return;
        }
        match (intermediates, final_byte) {
            (&[], b'A') => self.cuu(p1),
            (&[], b'B') => self.cud(p1),
            (&[], b'C') => self.cuf(p1),
            (&[], b'D') => self.cub(p1),
            // CNL
            (&[], b'E') => {
                self.cud(p1);
                self.screen.cursor.col = self.left_edge_for_line_start();
            }
            // CPL
            (&[], b'F') => {
                self.cuu(p1);
                self.screen.cursor.col = self.left_edge_for_line_start();
            }
            (&[], b'G') => self.cha(p1),
            (&[], b'H' | b'f') => self.cup(p1, p2),
            (&[], b'J') => self.ed(p1),
            (&[], b'K') => self.el(p1),
            (&[], b'L') => self.il(p1),
            (&[], b'M') => self.dl(p1),
            (&[], b'P') => self.dch(p1),
            (&[], b'@') => self.ich(p1),
            (&[], b'S') => self.su(p1),
            (&[], b'T') => self.sd(p1),
            (&[], b'X') => self.ech(p1),
            (&[], b'd') => self.vpa(p1),
            (&[], b'm') => {
                self.pen.apply_sgr(params, subparams);
                self.resync_pen_style();
            }
            // REP: cleared by any control / CSI / OSC dispatch so a REP
            // after a CUP repeats nothing (xterm).
            (&[], b'b') => self.rep(p1),
            // HPA (the back-tick form vttest emits)
            (&[], b'`') => self.cha(p1),
            // HPR
            (&[], b'a') => self.cuf(p1),
            // VPR
            (&[], b'e') => self.cud(p1),
            // CHT / CBT
            (&[], b'I') => self.cht(p1),
            (&[], b'Z') => self.cbt(p1),
            // TBC; modes other than 0 / 3 are accepted per xterm.
            (&[], b'g') => self.tbc(p1),
            (&[], b'r') => self.decstbm(p1, p2),
            // Modes outside IRM (4) and LNM (20) are consumed and
            // dropped; DECRQM answers "unknown" for them.
            (&[], b'h' | b'l') => {
                let on = final_byte == b'h';
                for &mode in params {
                    match mode {
                        4 => self.insert_mode = on,
                        20 => self.linefeed_newline_mode = on,
                        _ => {}
                    }
                }
            }
            // DA2 reply `CSI > Pp ; Pv ; Pc c`: Pp=41 (VT420) matches DA1
            // (`docs/explanation/protocols/vt-compliance.md` "Reply identity").
            // Fixed Pv satisfies `314 <= Pv <= 999` independently of package version.
            (&[b'>'], b'c') if p1 == 0 => {
                self.enqueue_response(b"\x1b[>41;400;0c".to_vec());
            }
            // XTVERSION; tmux gates path selection on the reply.
            (&[b'>'], b'q') if p1 == 0 => {
                let mut response = Vec::with_capacity(32);
                response.extend_from_slice(b"\x1bP>|felis ");
                response.extend_from_slice(env!("CARGO_PKG_VERSION").as_bytes());
                response.extend_from_slice(b"\x1b\\");
                self.enqueue_response(response);
            }
            // xterm resource-modifier set. Only `modifyOtherKeys` (Pp=4)
            // is acted on (REQ-506: honor it as if Kitty keyboard were
            // on); the others are no-ops since felis already emits the
            // modern CSI forms. A bare `CSI > m` resets all.
            (&[b'>'], b'm') => {
                if params.is_empty() || p1 == 4 {
                    // xterm defines only 0..=2; a higher Pv clamps to
                    // Level2 rather than reading as Off and dropping the
                    // disambiguation the program asked for.
                    self.modify_other_keys = match p2 {
                        0 => ModifyOtherKeys::Off,
                        1 => ModifyOtherKeys::Level1,
                        _ => ModifyOtherKeys::Level2,
                    };
                }
            }
            // DA3: the hex of "felis-1" rather than xterm's all-zeroes
            // placeholder (`docs/reference/protocols/vt-compliance.md`
            // "Reporting and queries").
            (&[b'='], b'c') if p1 == 0 => {
                self.enqueue_response(b"\x1bP!|66656c69732d31\x1b\\".to_vec());
            }
            // DECRQM is unrecognized below DECSCL level 3 and no reply
            // is sent (esctest's `test_DECSCL_Level2DoesntSupportDECRQM`).
            (&[b'$'], b'p') if self.conformance_level >= 3 => self.decrqm(p1, false),
            (&[b'?', b'$'], b'p') if self.conformance_level >= 3 => self.decrqm(p1, true),
            // XTERM_SAVE / XTERM_RESTORE (esctest's `xterm_save.py`).
            (&[b'?'], b's') => self.xterm_save_modes(params),
            (&[b'?'], b'r') => self.xterm_restore_modes(params),
            (&[b'$'], b'z') => self.decera(params),
            (&[b'$'], b'x') => self.decfra(params),
            (&[b'$'], b'{') => self.decsera(params),
            (&[b'$'], b'v') => self.deccra(params),
            (&[b'$'], b'r') => self.deccara(params, subparams),
            (&[b'$'], b't') => self.decrara(params, subparams),
            // DECSACE selects the extent (stream vs rectangle) for
            // DECCARA / DECRARA only; the other four rectangle ops are
            // always rectangular (docs/reference/protocols/vt-compliance.md).
            (&[b'*'], b'x') => self.dec_sace = p1,
            // DECSASD / DECSSDT / DECSNLS: felis has no status line and
            // the daemon owns the row count; DECRQSS answers "invalid".
            (&[b'$'], b'}' | b'~') | (&[b'*'], b'|') => {}
            // DECSCL. The 7-bit flag is `c1_8bit`'s inverse: `level;1`
            // = 7-bit responses, `level;0` / `level;2` = 8-bit C1.
            (&[b'"'], b'p') => {
                // VT510: selecting a level fully resets the terminal,
                // OSC 10/11/12, OSC 4 and OSC 5 overrides included;
                // esctest's `test_ResetSpecialColor_Dynamic` queries
                // OSC 10 right after `reset()` (which issues DECSCL) and
                // expects the default reply.
                self.c1_8bit = matches!(p2, 0 | 2);
                self.theme_overrides = [None; 3];
                self.clear_all_palette_overrides();
                self.special_color_overrides = [None; SPECIAL_COLOR_COUNT];
                // Stored raw so DECRQM / DECSLRM gating reads it back;
                // DECRQSS still clamps to 64 (what DA1 advertises).
                if (61..=65).contains(&p1) {
                    self.conformance_level = (p1 - 60) as u8;
                }
            }
            (&[b'\''], b'}') => self.decic(p1),
            (&[b'\''], b'~') => self.decdc(p1),
            // DECRQCRA: the checksum matches xterm's `do_dec_check_sum`
            // so esctest's `AssertScreenCharsInRectEqual` lines up.
            (&[b'*'], b'y') => {
                self.decrqcra(params);
            }
            // Only the in-cells size queries are serviced; pixel reports
            // need the client's cell metrics over IPC and are
            // accepted-and-ignored so defensive probes do not hang.
            (&[], b't') => self.xterm_window_op(p1, p2),
            // DECREQTPARM: the VT100 defaults (no parity, 8 bits, 9600
            // baud both ways, multiplier 16, no flags); Ps=1 marks the
            // reply solicited.
            (&[], b'x') => {
                let sol = if p1 == 1 { b'3' } else { b'2' };
                let mut response = Vec::with_capacity(24);
                response.extend_from_slice(b"\x1b[");
                response.push(sol);
                response.extend_from_slice(b";1;1;120;120;1;0x");
                self.enqueue_response(response);
            }
            // xterm's SCOSC / DECSLRM disambiguation: with DECLRMM off
            // any `CSI s` saves the cursor; with it on, `CSI s` is
            // DECSLRM (no params = full-width defaults).
            (&[], b's') => {
                if self.left_right_margin_mode {
                    // DECSLRM is VT420+: below level 4 it is ignored even
                    // with DECLRMM on (esctest's
                    // `test_DSCSCL_Level3_SupportsDECRQMDoesntSupportDECSLRM`).
                    if self.conformance_level >= 4 {
                        self.decslrm(p1, p2);
                    }
                } else {
                    self.decsc();
                }
            }
            (&[], b'u') => self.decrc(),
            (&[b'!'], b'p') => self.decstr(),
            (&[b' '], b'q') => self.decscusr(p1),
            (&[b' '], b'@') => self.sl(p1),
            (&[b' '], b'A') => self.sr(p1),
            (&[b'"'], b'q') => self.decsca(p1),
            (&[b'?'], b'J') => self.decsed(p1),
            (&[b'?'], b'K') => self.decsel(p1),
            (&[b'>'], b'u') => self.kitty_kbd_push(p1),
            (&[b'<'], b'u') => self.kitty_kbd_pop(p1),
            (&[b'='], b'u') => self.kitty_kbd_apply(p1, p2),
            (&[b'?'], b'u') => {
                let mut response = Vec::with_capacity(8);
                response.extend_from_slice(b"\x1b[?");
                response.extend_from_slice(self.kitty_kbd_flags().bits().to_string().as_bytes());
                response.push(b'u');
                self.enqueue_response(response);
            }
            // DA1 sends xterm's VT420 default mask verbatim, unimplemented
            // bits included (docs/explanation/architecture/terminal-identity.md
            // "Verified capabilities only").
            (&[], b'c') if p1 == 0 => {
                self.enqueue_response(DA1_REPLY.to_vec());
            }
            // CPR respects DECOM (esctest's
            // `test_CUP_RespectsOriginMode`).
            (&[], b'n') => match p1 {
                5 => self.enqueue_response(b"\x1b[0n".to_vec()),
                6 => {
                    let (row, col) = self.cursor_position_report();
                    let mut response = Vec::with_capacity(16);
                    response.extend_from_slice(b"\x1b[");
                    response.extend_from_slice(row.to_string().as_bytes());
                    response.push(b';');
                    response.extend_from_slice(col.to_string().as_bytes());
                    response.push(b'R');
                    self.enqueue_response(response);
                }
                _ => {}
            },
            // Private DSR: felis is software-only, so the replies report
            // absent / locked / no support in xterm's shapes.
            (&[b'?'], b'n') => match p1 {
                // DECXCPR; the page is always 1.
                6 => {
                    let (row, col) = self.cursor_position_report();
                    let mut response = Vec::with_capacity(20);
                    response.extend_from_slice(b"\x1b[?");
                    response.extend_from_slice(row.to_string().as_bytes());
                    response.push(b';');
                    response.extend_from_slice(col.to_string().as_bytes());
                    response.extend_from_slice(b";1R");
                    self.enqueue_response(response);
                }
                // no printer
                15 => self.enqueue_response(b"\x1b[?13n".to_vec()),
                // UDK locked (no DECUDK)
                25 => self.enqueue_response(b"\x1b[?21n".to_vec()),
                // North American keyboard, no ready state, default keypad
                // mode
                26 => self.enqueue_response(b"\x1b[?27;1;0;0n".to_vec()),
                // Locator status: esctest accepts any of {50, 53, 55};
                // xterm without a locator reports 50.
                53 | 55 => self.enqueue_response(b"\x1b[?50n".to_vec()),
                // locator type 0 (none)
                56 => self.enqueue_response(b"\x1b[?57;0n".to_vec()),
                // DECMSR: zero macro space; `*{` is the exact shape
                // esctest's `ReadCSI('*{')` matches.
                62 => self.enqueue_response(b"\x1b[0*{".to_vec()),
                // DECCKSR: zero checksum for an empty macro store; Pid is
                // echoed so the test routes the reply.
                63 => {
                    let pid = params.get(1).copied().unwrap_or(0);
                    let mut response = Vec::with_capacity(16);
                    response.extend_from_slice(b"\x1bP");
                    response.extend_from_slice(pid.to_string().as_bytes());
                    response.extend_from_slice(b"!~0000\x1b\\");
                    self.enqueue_response(response);
                }
                // integrity: ready, no errors
                75 => self.enqueue_response(b"\x1b[?70n".to_vec()),
                // not configured for multiple sessions
                85 => self.enqueue_response(b"\x1b[?83n".to_vec()),
                // Color-scheme query (kitty / ghostty / contour).
                996 => {
                    let report = self.color_scheme_report_bytes();
                    self.enqueue_response(report);
                }
                _ => {}
            },
            (&[b'?'], b'h' | b'l') => {
                let on = final_byte == b'h';
                // One pass in stream order: xterm applies SM/RM
                // parameters sequentially, so `?40;3h` arms
                // Allow80To132 before DECCOLM consults it while `?3;40h`
                // does not.
                for &mode in params {
                    self.apply_dec_private_mode(mode, on);
                }
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], final_byte: u8) {
        self.last_printed = None;
        // ST closes the OSC that keeps an override pending.
        if !(intermediates.is_empty() && final_byte == b'\\') {
            self.pending_bidi = PendingBidi::default();
        }
        match (intermediates, final_byte) {
            (&[], b'D') => self.line_feed(), // IND
            (&[], b'E') => {
                // NEL: the LF's scroll arm is decided on the pre-CR
                // column; a NEL from outside the DECSLRM band must not
                // scroll just because CR snapped the column to
                // `left_margin`.
                let was_outside = self.cursor_outside_left_right();
                self.carriage_return();
                if was_outside && self.screen.cursor.row == self.margins.bottom {
                    self.screen.cursor.pending_wrap = false;
                } else {
                    self.line_feed();
                }
            }
            (&[], b'H') => self.hts(),
            (&[], b'M') => self.reverse_index(),
            (&[], b'7') => self.decsc(),
            (&[], b'8') => self.decrc(),
            (&[], b'9') => self.decfi(),
            (&[], b'6') => self.decbi(),
            // DECKPAM / DECKPNM: state only; the client's keymap consults
            // it via `ModeSnapshot`.
            (&[], b'=') => self.application_keypad = true,
            (&[], b'>') => self.application_keypad = false,
            // S7C1T / S8C1T. The 8-bit transform runs at
            // `take_pty_effects` time so handlers keep building responses
            // in 7-bit form.
            (&[b' '], b'F') => self.c1_8bit = false,
            (&[b' '], b'G') => self.c1_8bit = true,
            // SPA / EPA (ECMA-48) are not aliases for DECSCA: DECSERA
            // respects only DEC-protected cells, so a cell under SPA
            // must stay DECSERA-erasable. SPA toggles `ISO_PROTECTED`,
            // DECSCA `PROTECTED`.
            (&[], b'V') => {
                self.pen.flags.insert(AttrFlags::ISO_PROTECTED);
                self.resync_pen_style();
            }
            (&[], b'W') => {
                self.pen.flags.remove(AttrFlags::ISO_PROTECTED);
                self.resync_pen_style();
            }
            // DECID: defensive `ESC Z` probes need the same string as
            // `CSI c`.
            (&[], b'Z') => {
                self.enqueue_response(DA1_REPLY.to_vec());
            }
            (&[], b'c') => {
                // RIS. Kitty graphics spec: a hard reset wipes every
                // placement, `C=1` included. `Self::new` clobbers
                // `pty_effects`, so the forced range is pushed after it.
                let rows = self.screen.rows;
                let cols = self.screen.cols;
                let term_name = self.term_name.take();
                *self = Self::new(rows, cols);
                self.term_name = term_name;
                self.pty_effects.push(PtyEffect::Erased(ErasedRange {
                    top: 0,
                    bottom: rows.saturating_sub(1),
                    force: true,
                }));
            }
            (&[b'#'], b'8') => self.decaln(),
            _ => {}
        }
    }

    fn dcs_hook(&mut self, _params: &[u16], intermediates: &[u8], ignore: bool, final_byte: u8) {
        self.last_printed = None;
        self.pending_bidi = PendingBidi::default();
        // REQ-903 exceed → discard: an overflowed DCS opener leaves
        // `self.dcs` at `None`, so body and terminator are no-ops.
        if ignore {
            return;
        }
        let kind = if intermediates == *b"$" && final_byte == b'q' {
            DcsKind::Decrqss
        } else if intermediates == *b"+" && final_byte == b'q' {
            DcsKind::Xtgettcap
        } else {
            DcsKind::Other
        };
        self.dcs = Some(DcsState {
            kind,
            body: Vec::new(),
        });
    }

    fn dcs_put(&mut self, byte: u8) {
        if let Some(state) = self.dcs.as_mut()
            && state.kind != DcsKind::Other
            && state.body.len() < DCS_BUFFER_LIMIT
        {
            state.body.push(byte);
        }
    }

    fn dcs_unhook(&mut self) {
        let Some(state) = self.dcs.take() else {
            return;
        };
        match state.kind {
            DcsKind::Decrqss => self.decrqss_reply(&state.body),
            DcsKind::Xtgettcap => self.xtgettcap_reply(&state.body),
            DcsKind::Other => {}
        }
    }

    fn osc_dispatch(&mut self, body: &[u8], _bell_terminated: bool) {
        self.last_printed = None;
        // The payload arrives verbatim; each handler splits only the
        // fields its wire form defines, so a literal `;` past the last
        // framing separator stays in the payload.
        let Some((code, payload)) = osc_number(body) else {
            return;
        };
        match code {
            110 => self.set_theme_override(ThemeChannel::Foreground, None),
            111 => self.set_theme_override(ThemeChannel::Background, None),
            112 => self.set_theme_override(ThemeChannel::Cursor, None),
            8 => self.dispatch_osc_8(payload),
            52 => self.dispatch_osc_52(payload),
            // OSC 22 is handled ahead of the single-string group so a
            // bare `OSC 22 ST` (reset) is reachable rather than swallowed
            // by the missing-payload guard.
            22 => self.dispatch_osc_22(payload.map(|p| split_osc_first(p).0)),
            66 => self.dispatch_osc_66(body),
            // OSC 9 / 99 / 777 (docs/reference/protocols/notifications.md)
            // are handled ahead of the single-string group so a bare
            // `OSC 9 ST` still reaches the rejecting parser.
            9 => self.dispatch_osc9(body),
            99 => self.dispatch_osc99(body),
            777 => self.dispatch_osc777(body),
            // `idx;spec` pairs, never one string: every `;` is framing.
            4 | 5 | 104 | 105 => self.dispatch_osc_palette(code, payload),
            // xterm allows multiple specs per sequence (`OSC 10 ; fg ;
            // bg ; cursor`), so every `;` is framing.
            10..=12 => self.dispatch_osc_dynamic_color(code, payload),
            // OSC 7's hostname is display-only per security-model.md.
            0 | 1 | 2 | 7 | 133 => {
                let Some(payload) = payload else {
                    return;
                };
                let Some(text) = sanitize_osc_str(payload) else {
                    return;
                };
                match code {
                    0 => {
                        if set_if_changed(&mut self.title, text) {
                            self.mark_title_changed();
                        }
                        set_if_changed(&mut self.icon_name, text);
                    }
                    2 => {
                        if set_if_changed(&mut self.title, text) {
                            self.mark_title_changed();
                        }
                    }
                    1 => {
                        set_if_changed(&mut self.icon_name, text);
                    }
                    7 => {
                        if set_if_changed(&mut self.cwd, text) {
                            self.mark_cwd_changed();
                        }
                    }
                    133 => {
                        let Some((kind, exit_code)) = parse_osc_133(text) else {
                            return;
                        };
                        // The absolute line, not the screen row: the mark
                        // must stay resolvable after the content scrolls
                        // off (docs/explanation/data-model/scrollback.md).
                        self.prompt_marks.push(PromptMark {
                            line: self.scrollback_total_pushed + u64::from(self.screen.cursor.row),
                            kind,
                            exit_code,
                        });
                    }
                    _ => unreachable!("outer match already narrowed to this code set"),
                }
            }
            _ => {}
        }
    }

    fn apc_dispatch(&mut self, body: &[u8]) {
        // A Kitty graphics placement can move the cursor.
        self.pending_bidi = PendingBidi::default();
        // Grid acts as relay (`docs/reference/protocols/kitty-graphics.md`);
        // drop and BEL past cap. Cursor is captured immediately because
        // trailing escapes like DECRC/CUP can move it before daemon drains APC.
        if !self
            .pty_effects
            .push_apc(body, self.screen.cursor.row, self.screen.cursor.col)
        {
            self.bell_pending = true;
        }
    }

    fn apc_overflow(&mut self) {
        // The truncated body still reaches `apc_dispatch`; the bell lets
        // a later dispatcher pass surface the truncation as a Kitty
        // graphics error.
        self.bell_pending = true;
    }
}

impl Grid {
    /// The single writer for DECSET/DECRST state, shared with
    /// `xterm_restore_modes` so a restore runs the same side effects as
    /// a set; a second partial writer is how a restore silently misses
    /// a field.
    pub(crate) fn apply_dec_private_mode(&mut self, mode: u16, on: bool) {
        match mode {
            // DECCKM: mirrored via `ModeSnapshot` so the keyboard encoder
            // swaps to the SS3 form vim / less read from terminfo's
            // `kcuu1`.
            1 => self.application_cursor = on,
            // DECSCNM: mirrored via `ModeSnapshot`; the renderer XORs it
            // with each cell's SGR 7. `mark_all` forces the RowDelta
            // cycle that carries the flip.
            5 => {
                if self.reverse_video != on {
                    self.reverse_video = on;
                    self.screen.damage.mark_all();
                }
            }
            // DECOM: toggling homes the cursor to the new origin (VT220).
            6 => {
                self.origin_mode = on;
                let row = if on { self.margins.top } else { 0 };
                self.screen.cursor.row = row;
                self.screen.cursor.col = 0;
                self.screen.cursor.pending_wrap = false;
            }
            7 => {
                self.autowrap = on;
                self.screen.cursor.pending_wrap = false;
            }
            25 => self.screen.cursor.visible = on,
            41 => self.more_fix = on,
            1049 => {
                if on {
                    self.enter_alternate(true);
                } else {
                    self.leave_alternate(true, false);
                }
            }
            // ?1048: DECSC / DECRC without a buffer switch (esctest's
            // `test_DECSET_SaveRestoreCursor`).
            1048 => {
                if on {
                    self.decsc();
                } else {
                    self.decrc();
                }
            }
            // `?47l` keeps the alt contents for a later `?47h`; `?1047l`
            // clears them (esctest's `test_DECSET_ALTBUF`).
            47 => {
                if on {
                    self.enter_alternate(false);
                } else {
                    self.leave_alternate(false, true);
                }
            }
            1047 => {
                if on {
                    self.enter_alternate(false);
                } else {
                    self.leave_alternate(false, false);
                }
            }
            2004 => {
                if !on && self.bracketed_paste {
                    self.pty_effects.push(PtyEffect::BracketedPasteDisabled);
                }
                self.bracketed_paste = on;
            }
            1004 => self.focus_reporting = on,
            // win32-input-mode: ConPTY asks for it on startup and
            // PSReadLine re-requests it. Mirrored through `ModeSnapshot`
            // so a non-Windows client attached to a Windows daemon honors
            // it too.
            9001 => self.win32_input_mode = on,
            2026 => self.set_synchronized_output(on),
            // Records the opt-in; the daemon emits `CSI ? 997 ; Ps n` off
            // the client's `InputMsg::ColorScheme` while set.
            2031 => self.color_scheme_notify = on,
            // Answering the set from here would pre-bake the geometry
            // into the coalesced effect queue, which a resize written
            // straight to the PTY can overtake: the daemon reports
            // both the set's own answer and every later change, from
            // live geometry.
            2048 => {
                self.in_band_resize_notify = on;
                if !on {
                    self.resize_notify_epoch = self.resize_notify_epoch.wrapping_add(1);
                }
            }
            // Allow80To132: gate for the DECCOLM arm; off by default like
            // xterm.
            40 => self.allow_80_to_132 = on,
            // DECNCSM; see `Grid::dec_ncsm`.
            95 => self.dec_ncsm = on,
            // DECCOLM: the width change is not honored (a
            // daemon-coordinated resize), but the destructive side
            // effects fire when `allow_80_to_132` is set, the screen
            // clear excepted at Level 5+ with DECNCSM (esctest's
            // `test_DECSCL_Level4_SupportsDECSLRMDoesntSupportDECNCSM`).
            3 if self.allow_80_to_132 => {
                self.margins.top = 0;
                self.margins.bottom = self.screen.rows.saturating_sub(1);
                self.margins.left = 0;
                self.margins.right = self.screen.cols.saturating_sub(1);
                self.left_right_margin_mode = false;
                self.screen.cursor.row = 0;
                self.screen.cursor.col = 0;
                self.screen.cursor.pending_wrap = false;
                if !(self.conformance_level >= 5 && self.dec_ncsm) {
                    for cell in &mut self.screen.cells {
                        *cell = Cell::default();
                    }
                    self.screen.has_sized_cells = false;
                    self.screen.soft_wrap.fill(false);
                }
                self.screen.damage.mark_all();
            }
            // Distinct bits since xterm 2023 split the semantics; see
            // `Grid::reverse_wrap_inline`.
            45 => self.reverse_wrap_inline = on,
            1045 => self.reverse_wrap_extend = on,
            69 => {
                self.left_right_margin_mode = on;
                if !on {
                    self.margins.left = 0;
                    self.margins.right = self.screen.cols.saturating_sub(1);
                }
            }
            // xterm semantics: a `?l` clears only when it matches the
            // active level (`?1003l` after `?1000h` does not clear).
            1000 => self.set_mouse_protocol(on, MouseProtocol::ButtonEvents),
            1002 => self.set_mouse_protocol(on, MouseProtocol::ButtonAndDrag),
            1003 => self.set_mouse_protocol(on, MouseProtocol::AnyMotion),
            1006 => self.set_mouse_encoding(on, MouseEncoding::Sgr),
            1016 => self.set_mouse_encoding(on, MouseEncoding::SgrPixels),
            // ?1005 (UTF-8) and ?1015 (urxvt) are accepted-and-ignored
            // (`docs/reference/protocols/support-matrix.md` "Mouse and
            // focus") so encoded output stays in the shape the program
            // selected.
            _ => {}
        }
        // Soft-track so DECRQM reports set/reset for unimplemented modes;
        // its hard-coded reads take precedence.
        if let Some(state) = self.dec_mode_states.get_mut(&mode) {
            *state = on;
        } else {
            self.dec_mode_states.insert(mode, on);
        }
    }

    /// Row-chunk loop appending base glyphs at the watermark without
    /// preceding cells access. Folds, overwrites, leading gaps, and
    /// pending wraps fall back to `put_grapheme`
    /// (`docs/explanation/data-model/grid-and-cells.md` "Occupancy watermark").
    fn print_wide_str(&mut self, s: &str) {
        let cols = usize::from(self.screen.cols);
        // Same gate as `print_str`; a 1-col grid cannot host a wide glyph.
        if cols < 2
            || self.left_right_margin_mode
            || self.insert_mode
            || self.screen.has_sized_cells
            || self.current_sizing_handle.is_some()
            || !self.autowrap
        {
            for c in s.chars() {
                self.put_grapheme(grapheme_for_char(c));
            }
            return;
        }
        let style = self.pen_style;
        let link = self.current_link;
        let widths = bmp_widths();
        let mut it = s.chars().peekable();
        while let Some(&c0) = it.peek() {
            // Folds attach to previous grapheme and must not wrap, so check
            // before handling `pending_wrap`. The defer set is cursor-independent,
            // preventing written chars from becoming later fold targets.
            if table_width(widths, c0) == 0 || self.zwj_pending || !self.pending_bidi.is_empty() {
                self.put_grapheme(grapheme_for_char(c0));
                it.next();
                continue;
            }
            if self.screen.cursor.pending_wrap {
                self.line_feed();
                self.screen.cursor.col = 0;
                self.screen.cursor.pending_wrap = false;
                self.screen.set_soft_wrap(self.screen.cursor.row, true);
            }
            let phys = self.screen.phys_row(self.screen.cursor.row);
            let col0 = usize::from(self.screen.cursor.col);
            if col0 != usize::from(self.screen.occupancy[phys]) {
                self.put_grapheme(grapheme_for_char(c0));
                it.next();
                continue;
            }
            // Every write lands past the existing watermark, so no eviction
            // or gap; touching only `cells` preserves bounds-check elision.
            let row = self.screen.cursor.row;
            let row_base = phys * cols;
            let mut col = col0;
            let mut last = c0;
            let row_cells = &mut self.screen.cells[row_base..row_base + cols];
            while let Some(&c) = it.peek() {
                let w = table_width(widths, c);
                if w == 0 {
                    break;
                }
                if col + w > cols {
                    break;
                }
                row_cells[col] = Cell {
                    grapheme: grapheme_for_char(c),
                    style,
                    link,
                    sizing: None,
                };
                if w == 2 {
                    row_cells[col + 1] = Cell {
                        grapheme: Grapheme::Spacer,
                        style,
                        link,
                        sizing: None,
                    };
                }
                col += w;
                last = c;
                it.next();
            }
            self.screen
                .occ_bump_phys(phys, u16::try_from(col).unwrap_or(u16::MAX));
            self.screen.damage.mark(usize::from(row));
            self.last_printed = Some(grapheme_for_char(last));
            // Not `grapheme_width`: a deferred char may fold into the glyph
            // just written, and a wrap armed for its standalone width would
            // move its owner out from under it.
            let next_need = it.peek().map_or(1, |&c| table_width(widths, c).max(1));
            if col >= cols || col + next_need > cols {
                self.screen.cursor.col = u16::try_from(cols - 1).unwrap_or(0);
                self.screen.cursor.pending_wrap = true;
            } else {
                self.screen.cursor.col = u16::try_from(col).unwrap_or(0);
            }
        }
    }

    /// Cold fork of `print_str` for a run starting past the watermark:
    /// blanks `[occ..start)`, a write `print_str`'s own body must not hold
    /// (`docs/explanation/data-model/grid-and-cells.md` "Occupancy
    /// watermark"), then runs the same chunk loop.
    #[inline(never)]
    fn print_ascii_bulk_gap(&mut self, bytes: &[u8], phys: usize, start: usize) {
        let cols = usize::from(self.screen.cols);
        if cols == 0 {
            return;
        }
        let occ = usize::from(self.screen.occupancy[phys]);
        let base = phys * cols;
        blank_cells(&mut self.screen.cells[base + occ..base + start]);
        let right_edge = cols - 1;
        let pen = Cell {
            grapheme: Grapheme::Empty,
            style: self.pen_style,
            link: self.current_link,
            sizing: None,
        };
        let mut i = 0;
        let mut last_byte: Option<u8> = None;
        while i < bytes.len() {
            if self.screen.cursor.pending_wrap {
                self.line_feed();
                self.screen.cursor.col = 0;
                self.screen.cursor.pending_wrap = false;
                self.screen.set_soft_wrap(self.screen.cursor.row, true);
            }
            let row = self.screen.cursor.row;
            let row_base = self.screen.idx(row, 0);
            let start_col = usize::from(self.screen.cursor.col);
            let room = cols - start_col;
            let take = (bytes.len() - i).min(room);
            let chunk_end = i + take;
            let occ = usize::from(self.screen.occupancy[row_base / cols]);
            store_ascii_run(
                &mut self.screen.cells[row_base..row_base + cols],
                start_col,
                occ,
                &bytes[i..chunk_end],
                pen,
            );
            let col = start_col + take;
            let j = chunk_end;
            self.screen
                .occ_bump_phys(row_base / cols, u16::try_from(col).unwrap_or(u16::MAX));
            self.screen.damage.mark(usize::from(row));
            last_byte = Some(bytes[j - 1]);
            i = j;
            if col == cols {
                self.screen.cursor.col = u16::try_from(right_edge).unwrap_or(u16::MAX);
                self.screen.cursor.pending_wrap = true;
            } else {
                self.screen.cursor.col = u16::try_from(col).unwrap_or(u16::MAX);
            }
        }
        if let Some(b) = last_byte {
            self.last_printed = Some(Grapheme::Ascii(b));
        }
    }
}
