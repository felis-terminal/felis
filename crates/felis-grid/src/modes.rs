//! ANSI / DEC private-mode classification tables: xterm's SM/RM and
//! DECRQM reply policy.

/// ANSI modes xterm reports as permanently reset (Ps=4), ignoring
/// SM/RM: xterm's `set_ansi_mode` (`charproc.c`), matching esctest's
/// `doPermanentlyResetAnsiTest` callers.
pub const PERMANENTLY_RESET_ANSI_MODES: &[u16] = &[
    1,  // GATM: guarded area transfer
    5,  // SRTM: status report transfer
    7,  // VEM: vertical editing
    10, // HEM: horizontal editing
    11, // PUM: positioning unit
    13, // FEAM: format effector action
    14, // FETM: format effector transfer
    15, // MATM: multiple area transfer
    16, // TTM: transfer termination
    17, // SATM: selected area transfer
    18, // TSM: tabulation stop
    19, // EBM: editing boundary
];

/// DEC modes modifiable in xterm but unimplemented in felis: queried without
/// prior DECSET/DECRESET they answer "reset" (Ps=2) rather than "unknown" (Ps=0).
/// Mirrors esctest's `doModifiableDecTest` callers minus functional/permanent modes.
pub const KNOWN_MODIFIABLE_DEC_MODES: &[u16] = &[
    3,  // DECCOLM: 132-column (felis stays 80-col)
    4,  // DECSCLM: smooth scroll
    18, // DECPFF: print form-feed
    19, // DECPEX: print extent
    34, // DECRLM: right-to-left
    35, // DECHEBM: Hebrew-encoding mode
    36, // DECHEM: Hebrew encoding
    42, // DECNRCM: national replacement charset
    57, // DECNAKB: Greek/N-American keyboard
    66, // DECNKM: numeric keypad
    67, // DECBKM: backarrow key
];

/// DEC modes xterm reports as Ps=4. esctest marks their
/// `doModifiableDecTest` calls `@knownBug(terminal="xterm")`, so the
/// cycle must fail: replying as modifiable would trip its "Should have
/// failed" check. SM/RM writes are still recorded in `dec_mode_states`,
/// but DECRQM ignores them.
pub const PERMANENTLY_RESET_DEC_MODES: &[u16] = &[
    8,  // DECARM: auto-repeat (xterm rejects)
    60, // DECHCCM: horizontal cursor coupling
    61, // DECVCCM: vertical cursor coupling
    64, // DECPCCM: page cursor coupling
    68, // DECKBUM: keyboard usage
    73, // DECXRLM: transmit rate limit
    81, // DECKPM: keypad mode
];

pub const PERMANENTLY_SET_DEC_MODES: &[u16] = &[
    2027, // grapheme cluster mode (REQ-602)
];

const fn table_has(table: &[u16], mode: u16) -> bool {
    let mut i = 0;
    while i < table.len() {
        if table[i] == mode {
            return true;
        }
        i += 1;
    }
    false
}

pub(crate) const fn permanently_reset_ansi(mode: u16) -> bool {
    table_has(PERMANENTLY_RESET_ANSI_MODES, mode)
}

pub(crate) const fn known_modifiable_dec_mode(mode: u16) -> bool {
    table_has(KNOWN_MODIFIABLE_DEC_MODES, mode)
}

pub(crate) const fn permanently_set_dec_mode(mode: u16) -> bool {
    table_has(PERMANENTLY_SET_DEC_MODES, mode)
}

pub(crate) const fn permanently_reset_dec_mode(mode: u16) -> bool {
    table_has(PERMANENTLY_RESET_DEC_MODES, mode)
}

#[cfg(test)]
mod tests;
