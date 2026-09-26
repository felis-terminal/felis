//! `Parser::advance` and the per-state byte handlers.

use super::{
    ApcBuffer, Intermediates, OscBuffer, Params, Parser, Sink, SosPmApcKind, State,
    scan_mixed_print_run, scan_printable_run, scan_string_body,
};

impl Parser {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: State::Ground,
            params: Params::new(),
            intermediates: Intermediates::new(),
            osc: OscBuffer::new(),
            apc: ApcBuffer::new(),
            sos_pm_apc_kind: None,
        }
    }

    #[must_use]
    pub const fn state(&self) -> State {
        self.state
    }

    /// `#[inline]` so callers (daemon `pump_reads`, the benches) inline the
    /// bulk scan into their hot loop; without the hint the body drops out
    /// of the inlining heuristic and per-byte calls pay a function call
    /// per LF.
    #[inline]
    pub fn advance<S: Sink>(&mut self, sink: &mut S, bytes: &[u8]) {
        let mut rest = bytes;
        while let Some((&first, tail)) = rest.split_first() {
            'bulk: {
                if matches!(self.state, State::Ground) {
                    if (0x20..=0x7E).contains(&first) {
                        let end = scan_printable_run(tail);
                        sink.print_str(&rest[..=end]);
                        rest = &tail[end..];
                        break 'bulk;
                    }
                    if first >= 0x80 {
                        let end = scan_mixed_print_run(tail);
                        sink.print_utf8_run(&rest[..=end]);
                        rest = &tail[end..];
                        break 'bulk;
                    }
                } else if matches!(self.state, State::CsiEntry | State::CsiParam)
                    && matches!(first, 0x30..=0x3B)
                    && (first != b':' || matches!(self.state, State::CsiParam))
                {
                    // A leading `:` stays per-byte: `start_empty` opens no
                    // subparam slot, where `push_run` would.
                    self.state = State::CsiParam;
                    let n = self.params.push_run(rest);
                    rest = &rest[n..];
                    break 'bulk;
                } else if matches!(self.state, State::OscString | State::SosPmApcString)
                    && !matches!(first, 0x07 | 0x18 | 0x1A | 0x1B)
                {
                    let end = scan_string_body(tail);
                    let run = &rest[..=end];
                    if matches!(self.state, State::OscString) {
                        if !self.osc.extend(run) {
                            sink.osc_overflow();
                        }
                    } else if matches!(self.sos_pm_apc_kind, Some(SosPmApcKind::Apc))
                        && !self.apc.extend(run)
                    {
                        sink.apc_overflow();
                    }
                    rest = &tail[end..];
                    break 'bulk;
                }
                self.advance_byte(sink, first);
                rest = tail;
            }
        }
    }

    pub fn advance_byte<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        // ESC restarts from Escape; CAN / SUB drop to Ground silently (Williams).
        if byte == 0x18 || byte == 0x1A {
            // Per `docs/explanation/security-model.md` "OSC 8 hyperlinks and
            // OSC 7 CWD", a payload truncated by an in-band control byte must
            // NOT install its partial value: clear first so the exit action
            // dispatches an empty body.
            self.osc.clear();
            sink.execute(byte);
            self.transition_to(sink, State::Ground);
            return;
        }
        if byte == 0x1B {
            self.transition_to(sink, State::Escape);
            return;
        }

        match self.state {
            State::Ground => self.ground(sink, byte),
            State::Escape => self.escape(sink, byte),
            State::EscapeIntermediate => self.escape_intermediate(sink, byte),
            State::CsiEntry => self.csi_entry(sink, byte),
            State::CsiParam => self.csi_param(sink, byte),
            State::CsiIntermediate => self.csi_intermediate(sink, byte),
            State::CsiIgnore => self.csi_ignore(sink, byte),
            State::DcsEntry => self.dcs_entry(sink, byte),
            State::DcsParam => self.dcs_param(sink, byte),
            State::DcsIntermediate => self.dcs_intermediate(sink, byte),
            State::DcsPassthrough => self.dcs_passthrough(sink, byte),
            State::DcsIgnore => self.dcs_ignore(sink, byte),
            State::OscString => self.osc_string(sink, byte),
            State::SosPmApcString => self.sos_pm_apc(sink, byte),
        }
    }

    fn transition_to<S: Sink>(&mut self, sink: &mut S, next: State) {
        // APC joins the Williams OSC / DCS exit actions so the accumulated
        // body (Kitty graphics) dispatches. No state self-transitions, so
        // each `!matches!(next, …)` guard is always true (equivalent mutant).
        match self.state {
            State::OscString if !matches!(next, State::OscString) => {
                self.dispatch_osc(sink, /*bell_terminated*/ false);
                self.osc.clear();
            }
            State::SosPmApcString if !matches!(next, State::SosPmApcString) => {
                if matches!(self.sos_pm_apc_kind, Some(SosPmApcKind::Apc)) {
                    sink.apc_dispatch(self.apc.bytes());
                }
                self.apc.clear();
                self.sos_pm_apc_kind = None;
            }
            State::DcsPassthrough if !matches!(next, State::DcsPassthrough) => {
                sink.dcs_unhook();
            }
            _ => {}
        }
        match next {
            State::Escape => {
                self.params.clear();
                self.intermediates.clear();
            }
            // Always already empty (only reached via `escape`); equivalent mutant.
            State::CsiEntry | State::DcsEntry => {
                self.params.clear();
                self.intermediates.clear();
            }
            // Always already empty (every OscString exit clears); equivalent mutant.
            State::OscString => {
                self.osc.clear();
                let _ = sink;
            }
            // No-op arm for Williams-table symmetry; equivalent mutant.
            State::DcsPassthrough => {
                let final_byte = 0;
                let _ = final_byte;
            }
            _ => {}
        }
        self.state = next;
    }

    fn ground<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        if byte < 0x20 || byte == 0x7F {
            sink.execute(byte);
        } else {
            sink.print(byte);
        }
    }

    fn escape<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(byte),
            0x20..=0x2F => {
                self.intermediates.push(byte);
                self.state = State::EscapeIntermediate;
            }
            0x30..=0x4F | 0x51..=0x57 | 0x59 | 0x5A | 0x5C | 0x60..=0x7E => {
                sink.esc_dispatch(self.intermediates.as_slice(), byte);
                self.transition_to(sink, State::Ground);
            }
            0x50 => self.transition_to(sink, State::DcsEntry),
            0x58 | 0x5E | 0x5F => {
                self.sos_pm_apc_kind = Some(match byte {
                    0x58 => SosPmApcKind::Sos,
                    0x5E => SosPmApcKind::Pm,
                    _ => SosPmApcKind::Apc,
                });
                self.transition_to(sink, State::SosPmApcString);
            }
            0x5B => self.transition_to(sink, State::CsiEntry),
            0x5D => self.transition_to(sink, State::OscString),
            0x7F => { /* ignore per Williams */ }
            _ => self.transition_to(sink, State::Ground),
        }
    }

    fn escape_intermediate<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(byte),
            0x20..=0x2F => self.intermediates.push(byte),
            0x30..=0x7E => {
                sink.esc_dispatch(self.intermediates.as_slice(), byte);
                self.transition_to(sink, State::Ground);
            }
            0x7F => { /* ignore */ }
            _ => self.transition_to(sink, State::Ground),
        }
    }

    fn csi_entry<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(byte),
            0x20..=0x2F => {
                self.intermediates.push(byte);
                self.state = State::CsiIntermediate;
            }
            0x30..=0x39 | 0x3B => {
                self.params.start_with(byte);
                self.state = State::CsiParam;
            }
            0x3A => {
                self.params.start_empty();
                self.state = State::CsiParam;
            }
            0x3C..=0x3F => {
                self.intermediates.push(byte);
                self.state = State::CsiParam;
            }
            0x40..=0x7E => {
                // Both overflow flags are false here (pushing anything leaves
                // CsiEntry), so `||` -> `&&` is an equivalent mutant.
                self.dispatch_csi(sink, byte);
            }
            0x7F => { /* ignore */ }
            _ => self.state = State::CsiIgnore,
        }
    }

    fn csi_param<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(byte),
            0x30..=0x39 => self.params.push_digit(byte - b'0'),
            0x3A | 0x3B => self.params.next_slot(byte == 0x3A),
            0x20..=0x2F => {
                self.intermediates.push(byte);
                self.state = State::CsiIntermediate;
            }
            0x3C..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.dispatch_csi(sink, byte),
            0x7F => { /* ignore */ }
            _ => self.state = State::CsiIgnore,
        }
    }

    fn csi_intermediate<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(byte),
            0x20..=0x2F => self.intermediates.push(byte),
            0x30..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.dispatch_csi(sink, byte),
            0x7F => { /* ignore */ }
            _ => self.state = State::CsiIgnore,
        }
    }

    fn dispatch_csi<S: Sink>(&mut self, sink: &mut S, final_byte: u8) {
        sink.csi_dispatch(
            self.params.as_slice(),
            self.params.subparams(),
            self.intermediates.as_slice(),
            self.params.overflowed || self.intermediates.overflowed,
            final_byte,
        );
        self.transition_to(sink, State::Ground);
    }

    fn csi_ignore<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => sink.execute(byte),
            0x40..=0x7E => self.transition_to(sink, State::Ground),
            _ => {}
        }
    }

    fn dcs_entry<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F | 0x7F => { /* ignore */ }
            0x20..=0x2F => {
                self.intermediates.push(byte);
                self.state = State::DcsIntermediate;
            }
            0x30..=0x39 | 0x3B => {
                self.params.start_with(byte);
                self.state = State::DcsParam;
            }
            0x3A => {
                self.params.start_empty();
                self.state = State::DcsParam;
            }
            0x3C..=0x3F => {
                self.intermediates.push(byte);
                self.state = State::DcsParam;
            }
            0x40..=0x7E => self.enter_dcs_passthrough(sink, byte),
            _ => self.state = State::DcsIgnore,
        }
    }

    fn dcs_param<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F | 0x7F => { /* ignore */ }
            0x30..=0x39 => self.params.push_digit(byte - b'0'),
            // DCS never forwards the sub-param mask, so `==` -> `!=` is an
            // equivalent mutant.
            0x3A | 0x3B => self.params.next_slot(byte == 0x3A),
            0x20..=0x2F => {
                self.intermediates.push(byte);
                self.state = State::DcsIntermediate;
            }
            0x3C..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.enter_dcs_passthrough(sink, byte),
            _ => self.state = State::DcsIgnore,
        }
    }

    fn dcs_intermediate<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F | 0x7F => { /* ignore */ }
            0x20..=0x2F => self.intermediates.push(byte),
            0x30..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.enter_dcs_passthrough(sink, byte),
            _ => self.state = State::DcsIgnore,
        }
    }

    fn enter_dcs_passthrough<S: Sink>(&mut self, sink: &mut S, final_byte: u8) {
        sink.dcs_hook(
            self.params.as_slice(),
            self.intermediates.as_slice(),
            self.params.overflowed || self.intermediates.overflowed,
            final_byte,
        );
        self.state = State::DcsPassthrough;
    }

    fn dcs_passthrough<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F | 0x20..=0x7E => sink.dcs_put(byte),
            0x7F => { /* ignore */ }
            _ => sink.dcs_put(byte),
        }
    }

    fn dcs_ignore<S: Sink>(&mut self, _sink: &mut S, _byte: u8) {}

    fn osc_string<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        match byte {
            // BEL acts as ST per xterm.
            0x07 => {
                self.dispatch_osc(sink, true);
                self.osc.clear();
                self.state = State::Ground;
            }
            // 0x9C (C1 ST) is deliberately NOT honored: in a UTF-8 stream it
            // is a continuation byte of common CJK ideographs (作 = E4 BD 9C),
            // so it would truncate CJK titles, OSC 7 cwds, and OSC 52 payloads.
            _ => {
                if !self.osc.push(byte) {
                    sink.osc_overflow();
                }
            }
        }
    }

    fn sos_pm_apc<S: Sink>(&mut self, sink: &mut S, byte: u8) {
        // Same C1-ST refusal as `osc_string`: Kitty graphics m=1 chunks
        // carry raw RGBA bytes that include 0x9C.
        if matches!(self.sos_pm_apc_kind, Some(SosPmApcKind::Apc)) && !self.apc.push(byte) {
            sink.apc_overflow();
        }
    }

    fn dispatch_osc<S: Sink>(&mut self, sink: &mut S, bell_terminated: bool) {
        sink.osc_dispatch(self.osc.bytes(), bell_terminated);
    }
}
