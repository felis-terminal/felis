//! Kitty graphics command parser, reassembler, and response framer.
//!
//! Enforces structural invariants of `G <controls> ; <payload>`. Spec source:
//! `docs/reference/protocols/kitty-graphics.md` "Wire format" and upstream
//! <https://sw.kovidgoyal.net/kitty/graphics-protocol/>.

pub mod inflate;
// base64 lives in `felis-protocol` because `felis-grid`'s OSC 52 path
// needs the same codec; re-exported so this path stays stable.
pub use felis_protocol::base64;

/// Borrows into the original APC buffer: the dispatcher copies what it
/// needs before the parser advances.
#[derive(Debug, PartialEq, Eq)]
pub struct Command<'a> {
    /// In transmission order; duplicates and unknown keys are preserved
    /// because the spec's last-write-wins rule isn't uniform.
    pub controls: Vec<(u8, &'a [u8])>,
    /// Bytes after the first `;`. Empty both when there was no `;` at all
    /// and when a trailing `;` had nothing after it.
    pub payload: &'a [u8],
}

/// Non-`G` bodies return `None` so other APC dialects cannot route through
/// the Kitty graphics path; so does any malformed control pair (empty
/// pair, missing `=`, non-ASCII-letter or multi-char key, empty value,
/// value with bytes outside `[A-Za-z0-9-]`).
#[must_use]
pub fn parse(body: &[u8]) -> Option<Command<'_>> {
    let after_g = body.strip_prefix(b"G")?;
    let (controls_bytes, payload) = after_g.iter().position(|&b| b == b';').map_or_else(
        || (after_g, &[][..]),
        |i| (&after_g[..i], &after_g[i + 1..]),
    );
    let controls = parse_controls(controls_bytes)?;
    Some(Command { controls, payload })
}

fn parse_controls(controls: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    if controls.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for pair in controls.split(|&b| b == b',') {
        // The spec gives a leading / trailing / consecutive comma no meaning.
        if pair.is_empty() {
            return None;
        }
        if pair.len() < 3 || pair[1] != b'=' {
            return None;
        }
        let key = pair[0];
        if !key.is_ascii_alphabetic() {
            return None;
        }
        let value = &pair[2..];
        // Spec values are ASCII-alphanumeric or `-` (`z=-1`); anything else
        // surfaces here, not at the dispatcher.
        for &b in value {
            if !(b.is_ascii_alphanumeric() || b == b'-') {
                return None;
            }
        }
        out.push((key, value));
    }
    Some(out)
}

impl<'a> Command<'a> {
    /// Latest value; iterate `self.controls` for every occurrence.
    #[must_use]
    pub fn get(&self, key: u8) -> Option<&'a [u8]> {
        self.controls
            .iter()
            .rev()
            .find_map(|(k, v)| (*k == key).then_some(*v))
    }
}

/// The last value `key` carries in a completed command's controls.
#[must_use]
pub fn control(controls: &[(u8, Vec<u8>)], key: u8) -> Option<&[u8]> {
    controls
        .iter()
        .rev()
        .find_map(|(k, v)| (*k == key).then_some(v.as_slice()))
}

/// `key` read as a decimal number, so `C=01` means what `C=1` does.
#[must_use]
pub fn control_u32(controls: &[(u8, Vec<u8>)], key: u8) -> Option<u32> {
    std::str::from_utf8(control(controls, key)?)
        .ok()?
        .parse()
        .ok()
}

/// Whether kitty moves the cursor past the placement a completed command
/// makes (`graphics.c` `handle_put_command`): `a=T` and `a=p` do unless
/// `C=1` asks not to or `U=1` makes the placement virtual. The grid stops its parse on exactly these commands
/// and the dispatcher moves the cursor on exactly these, so text after
/// the image lands beside it.
#[must_use]
pub fn moves_cursor(controls: &[(u8, Vec<u8>)]) -> bool {
    matches!(control(controls, b'a'), Some(b"T" | b"p"))
        && control_u32(controls, b'C') != Some(1)
        && control_u32(controls, b'U') != Some(1)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResponseRefs {
    /// `i=<u32>`.
    pub image_id: Option<u32>,
    /// `I=<u32>`.
    pub image_number: Option<u32>,
    /// `p=<u32>`.
    pub placement_id: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status<'a> {
    /// Wire form: `OK`.
    Ok,
    /// Command failed. Wire form: `<code>:<message>` (or just
    /// `<code>` when `message` is empty).
    Error {
        code: ErrorCode,
        /// `;`, C0 control bytes, and DEL are replaced with `_` so a message
        /// can neither terminate the APC early nor inject bytes into the
        /// PTY-bound reply.
        message: &'a str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorCode {
    /// `ENOENT`: image, placement, or file not found.
    NotFound,
    /// `EINVAL`: invalid value or combination of values.
    InvalidValue,
    /// `ENOTSUP`: operation not implemented yet.
    Unsupported,
    /// `ENODATA`: chunked data promised (`m=1`) but never terminated.
    NoData,
    /// `EBADF`: image present but the bytes are corrupt.
    BadImage,
    /// `EIO`: transmission error (file read, SHM open).
    IoError,
}

impl ErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotFound => "ENOENT",
            Self::InvalidValue => "EINVAL",
            Self::Unsupported => "ENOTSUP",
            Self::NoData => "ENODATA",
            Self::BadImage => "EBADF",
            Self::IoError => "EIO",
        }
    }
}

/// Wire shape: `ESC _ G <id keys> ; <status> ESC \`, where status is `OK`
/// or `<code>[:<message>]`. `q=` quiet-mode handling lives at the
/// dispatcher; this emits unconditionally.
#[must_use]
pub fn format_response(refs: &ResponseRefs, status: Status<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(32);
    out.extend_from_slice(b"\x1b_G");
    let mut first = true;
    let mut push_kv = |out: &mut Vec<u8>, key: u8, value: u32| {
        if !first {
            out.push(b',');
        }
        first = false;
        out.push(key);
        out.push(b'=');
        out.extend_from_slice(value.to_string().as_bytes());
    };
    if let Some(v) = refs.image_id {
        push_kv(&mut out, b'i', v);
    }
    if let Some(v) = refs.image_number {
        push_kv(&mut out, b'I', v);
    }
    if let Some(v) = refs.placement_id {
        push_kv(&mut out, b'p', v);
    }
    out.push(b';');
    match status {
        Status::Ok => out.extend_from_slice(b"OK"),
        Status::Error { code, message } => {
            out.extend_from_slice(code.as_str().as_bytes());
            if !message.is_empty() {
                out.push(b':');
                out.extend(message.bytes().map(|b| {
                    if b == b';' || b < 0x20 || b == 0x7f {
                        b'_'
                    } else {
                        b
                    }
                }));
            }
        }
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// 64 MiB covers a 4K RGBA frame uncompressed (≈33 MiB) plus base64
/// expansion. Per `docs/explanation/security-model.md` "Kitty graphics", a
/// producer streaming `m=1` forever costs O(cap) memory, not O(stream).
pub const REASSEMBLY_BUFFER_LIMIT: usize = 64 * 1024 * 1024;

// One chunk arrives as one APC body.
const _: () = assert!(REASSEMBLY_BUFFER_LIMIT > crate::APC_BUFFER_LIMIT);
// A literal, not the symbol: the documented cap must not move with it.
const _: () = assert!(REASSEMBLY_BUFFER_LIMIT == 67_108_864);

#[derive(Debug, PartialEq, Eq)]
pub struct CompleteCommand {
    /// Head-chunk controls with `m` stripped.
    pub controls: Vec<(u8, Vec<u8>)>,
    /// Concatenated chunk payloads as sent: base64 decoding (`t=d`) and
    /// zlib inflate (`o=z`) happen at the consumer.
    pub payload: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Done(CompleteCommand),
    /// Total payload exceeded [`REASSEMBLY_BUFFER_LIMIT`]; the reassembler
    /// reset itself. Distinct from `Pending` so the dispatcher answers with
    /// an error instead of awaiting a terminator that will not come, and it
    /// carries the head controls (a continuation chunk's are ignored per
    /// spec) so that answer can echo the producer's ids and honor its `q=`.
    Overflow {
        controls: Vec<(u8, Vec<u8>)>,
    },
}

fn is_nonzero(value: &[u8]) -> bool {
    std::str::from_utf8(value)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .is_some_and(|v| v != 0)
}

fn owned_controls_without_m(cmd: &Command<'_>) -> Vec<(u8, Vec<u8>)> {
    cmd.controls
        .iter()
        .filter(|(k, _)| *k != b'm')
        .map(|(k, v)| (*k, v.to_vec()))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize, Default),
    serde(default)
)]
pub(crate) struct OpenStream {
    head_controls: Vec<(u8, Vec<u8>)>,
    pub(crate) buffered: usize,
}

/// What one body does to a chunked transmission.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// `continued` is `false` when this body starts a new command.
    Pending {
        continued: bool,
    },
    Done {
        controls: Vec<(u8, Vec<u8>)>,
        continued: bool,
    },
    Overflow {
        controls: Vec<(u8, Vec<u8>)>,
    },
}

/// The chunked-transmission state machine without the payload: which body
/// completes a command, with which head controls. [`Reassembler`] runs on
/// it, and so can a consumer that must agree with the reassembler about
/// where commands end without buffering their bytes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct ReassemblyTracker {
    pub(crate) open: Option<OpenStream>,
}

impl ReassemblyTracker {
    #[must_use]
    pub const fn new() -> Self {
        Self { open: None }
    }

    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn step(&mut self, cmd: &Command<'_>) -> Step {
        // kitty's `grman_handle_command` routes only add actions through
        // `currently_loading`; the rest run at once. A delete drops an open
        // transfer (`handle_delete_command`); the others leave it to its
        // next chunk.
        if let Some(action @ (b"p" | b"d" | b"c" | b"a")) = cmd.get(b'a') {
            if action == b"d" {
                self.reset();
            }
            return Step::Done {
                controls: owned_controls_without_m(cmd),
                continued: false,
            };
        }
        // kitty consults `g->more` only in the `t=d` arm of `load_image_data`
        // (graphics.c), so `m=` is meaningless for file-backed media. mpv's
        // `--vo-kitty-use-shm` sends `t=s,…,m=1` every frame with no
        // terminating chunk; honoring `m=` would park it forever. Any
        // in-flight `t=d` stream is dropped, matching `handle_add_command`.
        let file_like = matches!(cmd.get(b't'), Some(b"f" | b"t" | b"s"));
        if file_like {
            self.reset();
        }
        let more = !file_like && matches!(cmd.get(b'm'), Some(b"1"));
        // The spec says a continuation's controls are ignored except `m`,
        // but kitty lets a non-zero `q=` override the head's, and that
        // holds for the reply to an overflowing chunk too.
        if let (Some(state), Some(quiet)) =
            (self.open.as_mut(), cmd.get(b'q').filter(|q| is_nonzero(q)))
        {
            state.head_controls.retain(|(k, _)| *k != b'q');
            state.head_controls.push((b'q', quiet.to_vec()));
        }
        // Reject before mutating the buffer so an overflow leaves it
        // fresh. A first-chunk overflow has no stored head, so the offending
        // chunk's own controls stand in.
        let buffered = self.open.as_ref().map_or(0, |s| s.buffered);
        if buffered.saturating_add(cmd.payload.len()) > REASSEMBLY_BUFFER_LIMIT {
            let controls = self
                .open
                .take()
                .map_or_else(|| owned_controls_without_m(cmd), |s| s.head_controls);
            return Step::Overflow { controls };
        }
        let continued = self.open.is_some();
        let state = match self.open.take() {
            Some(mut state) => {
                state.buffered += cmd.payload.len();
                state
            }
            None => OpenStream {
                head_controls: owned_controls_without_m(cmd),
                buffered: cmd.payload.len(),
            },
        };
        if more {
            self.open = Some(state);
            return Step::Pending { continued };
        }
        Step::Done {
            controls: state.head_controls,
            continued,
        }
    }

    pub fn reset(&mut self) {
        self.open = None;
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct Reassembler {
    pub(crate) tracker: ReassemblyTracker,
    #[cfg_attr(feature = "state-dump", serde(with = "crate::state::b64"))]
    pub(crate) payload: Vec<u8>,
}

impl Reassembler {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tracker: ReassemblyTracker::new(),
            payload: Vec::new(),
        }
    }

    /// Parked payload bytes of the open transmission, or `None` between
    /// transmissions. The daemon's resource accounting reads this:
    /// `felis daemon status` is the only place a never-terminated `m=1`
    /// stream is visible.
    #[must_use]
    pub fn in_flight_bytes(&self) -> Option<usize> {
        self.tracker.is_open().then_some(self.payload.len())
    }

    pub fn feed(&mut self, cmd: &Command<'_>) -> Outcome {
        match self.tracker.step(cmd) {
            Step::Overflow { controls } => {
                self.payload = Vec::new();
                Outcome::Overflow { controls }
            }
            Step::Pending { continued } => {
                if !continued {
                    self.payload.clear();
                }
                self.payload.extend_from_slice(cmd.payload);
                Outcome::Pending
            }
            Step::Done {
                controls,
                continued,
            } => {
                let payload = if continued {
                    let mut payload = std::mem::take(&mut self.payload);
                    payload.extend_from_slice(cmd.payload);
                    payload
                } else {
                    if !self.tracker.is_open() {
                        self.payload = Vec::new();
                    }
                    cmd.payload.to_vec()
                };
                Outcome::Done(CompleteCommand { controls, payload })
            }
        }
    }

    pub fn reset(&mut self) {
        self.tracker.reset();
        self.payload = Vec::new();
    }
}

// Kani proofs (`docs/reference/testing.md` "Kani proof inventory").
#[cfg(kani)]
mod kani_proofs {
    use super::parse_controls;

    /// Whatever `parse_controls` accepts is well-formed (single ASCII-alpha
    /// key, non-empty `[A-Za-z0-9-]` value): the dispatcher reads control
    /// values without re-validating them.
    #[kani::proof]
    #[kani::unwind(10)]
    fn parse_controls_never_panics_and_output_is_well_formed() {
        let bytes: [u8; 8] = kani::any();
        if let Some(pairs) = parse_controls(&bytes) {
            for (key, value) in &pairs {
                assert!(key.is_ascii_alphabetic());
                assert!(!value.is_empty());
                for &b in *value {
                    assert!(b.is_ascii_alphanumeric() || b == b'-');
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_cursor_reads_c_and_u_as_numbers() {
        let controls = |body: &[u8]| -> Vec<(u8, Vec<u8>)> {
            parse(body)
                .unwrap()
                .controls
                .into_iter()
                .map(|(k, v)| (k, v.to_vec()))
                .collect()
        };
        for (body, want) in [
            (&b"Ga=T"[..], true),
            (b"Ga=p,C=0", true),
            (b"Ga=p,C=2", true),
            (b"Ga=T,C=1", false),
            (b"Ga=T,C=01", false),
            (b"Ga=p,U=01", false),
            (b"Ga=T,C=1,C=0", true),
            (b"Ga=t", false),
            (b"G", false),
        ] {
            assert_eq!(
                moves_cursor(&controls(body)),
                want,
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[test]
    fn rejects_body_not_starting_with_g() {
        // `G` is the only APC dialect routed to the graphics dispatcher.
        assert!(parse(b"X").is_none());
        assert!(parse(b"").is_none());
        assert!(parse(b"a=T,f=32").is_none());
    }

    #[test]
    fn empty_command_after_g_is_valid() {
        // `ESC _ G ESC \`: no controls, no payload, must not panic.
        let cmd = parse(b"G").unwrap();
        assert_eq!(cmd.controls, []);
        assert_eq!(cmd.payload, b"");
    }

    #[test]
    fn empty_controls_with_payload_separator() {
        let cmd = parse(b"G;hello").unwrap();
        assert_eq!(cmd.controls, []);
        assert_eq!(cmd.payload, b"hello");
    }

    #[test]
    fn empty_controls_and_empty_payload() {
        let cmd = parse(b"G;").unwrap();
        assert_eq!(cmd.controls, []);
        assert_eq!(cmd.payload, b"");
    }

    #[test]
    fn single_control_no_payload() {
        let cmd = parse(b"Ga=T").unwrap();
        assert_eq!(cmd.controls, vec![(b'a', b"T".as_slice())]);
        assert_eq!(cmd.payload, b"");
    }

    #[test]
    fn single_control_with_payload() {
        let cmd = parse(b"Ga=T;abc").unwrap();
        assert_eq!(cmd.controls, vec![(b'a', b"T".as_slice())]);
        assert_eq!(cmd.payload, b"abc");
    }

    #[test]
    fn multiple_controls_preserve_order() {
        let cmd = parse(b"Ga=T,f=100,i=42").unwrap();
        assert_eq!(
            cmd.controls,
            vec![
                (b'a', b"T".as_slice()),
                (b'f', b"100".as_slice()),
                (b'i', b"42".as_slice()),
            ]
        );
    }

    #[test]
    fn signed_integer_value_accepted() {
        let cmd = parse(b"Gz=-1").unwrap();
        assert_eq!(cmd.controls, vec![(b'z', b"-1".as_slice())]);
    }

    #[test]
    fn missing_equals_is_rejected() {
        assert!(parse(b"Ga").is_none());
        assert!(parse(b"Ga,b=2").is_none());
        assert!(parse(b"Ga=T,f").is_none());
    }

    #[test]
    fn empty_key_is_rejected() {
        assert!(parse(b"G=1").is_none());
        assert!(parse(b"Ga=1,=2").is_none());
    }

    #[test]
    fn multi_char_key_is_rejected() {
        // Spec keys are single ASCII letters.
        assert!(parse(b"Gab=1").is_none());
        assert!(parse(b"G1=2").is_none());
    }

    #[test]
    fn empty_value_is_rejected() {
        assert!(parse(b"Ga=").is_none());
        assert!(parse(b"Ga=,b=1").is_none());
    }

    #[test]
    fn whitespace_in_controls_is_rejected() {
        assert!(parse(b"G a=1").is_none());
        assert!(parse(b"Ga =1").is_none());
        assert!(parse(b"Ga= 1").is_none());
        assert!(parse(b"Ga=1, b=2").is_none());
    }

    #[test]
    fn value_with_invalid_byte_is_rejected() {
        // `+` and `/` are base64 chars; base64 belongs in the payload only.
        assert!(parse(b"Ga=ab+cd").is_none());
        assert!(parse(b"Ga=T,f=3_2").is_none());
    }

    #[test]
    fn leading_or_trailing_comma_is_rejected() {
        assert!(parse(b"G,a=1").is_none());
        assert!(parse(b"Ga=1,").is_none());
        assert!(parse(b"Ga=1,,b=2").is_none());
    }

    #[test]
    fn payload_can_contain_anything_after_first_semicolon() {
        let cmd = parse(b"Ga=T;ab;cd").unwrap();
        assert_eq!(cmd.controls, vec![(b'a', b"T".as_slice())]);
        assert_eq!(cmd.payload, b"ab;cd");
    }

    #[test]
    fn duplicate_keys_are_preserved_in_order() {
        let cmd = parse(b"Ga=1,a=2,a=3").unwrap();
        assert_eq!(
            cmd.controls,
            vec![
                (b'a', b"1".as_slice()),
                (b'a', b"2".as_slice()),
                (b'a', b"3".as_slice()),
            ]
        );
    }

    #[test]
    fn get_returns_latest_duplicate() {
        let cmd = parse(b"Ga=1,a=2,a=3").unwrap();
        assert_eq!(cmd.get(b'a'), Some(b"3".as_slice()));
        assert_eq!(cmd.get(b'b'), None);
    }

    #[test]
    fn realistic_kitty_graphics_command() {
        // Sample from the Kitty graphics protocol page.
        let cmd = parse(b"Ga=T,f=24,s=100,v=50;iVBORw0KGgo=").unwrap();
        assert_eq!(cmd.get(b'a'), Some(b"T".as_slice()));
        assert_eq!(cmd.get(b'f'), Some(b"24".as_slice()));
        assert_eq!(cmd.get(b's'), Some(b"100".as_slice()));
        assert_eq!(cmd.get(b'v'), Some(b"50".as_slice()));
        assert_eq!(cmd.payload, b"iVBORw0KGgo=");
    }

    fn done_payload(outcome: &Outcome) -> &[u8] {
        match outcome {
            Outcome::Done(c) => &c.payload,
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn single_chunk_image_emits_done_immediately() {
        let mut r = Reassembler::new();
        let cmd = parse(b"Ga=T,f=100;abc").unwrap();
        let outcome = r.feed(&cmd);
        let Outcome::Done(complete) = outcome else {
            panic!("expected Done");
        };
        assert_eq!(complete.payload, b"abc");
        assert_eq!(
            complete.controls,
            vec![(b'a', b"T".to_vec()), (b'f', b"100".to_vec())]
        );
    }

    #[test]
    fn explicit_m0_first_chunk_is_treated_as_single_chunk() {
        // `m=0` on a head with no prior chunks is "single chunk done".
        let mut r = Reassembler::new();
        let cmd = parse(b"Gm=0;hello").unwrap();
        assert_eq!(done_payload(&r.feed(&cmd)), b"hello");
    }

    #[test]
    fn multi_chunk_image_concatenates_in_order() {
        let mut r = Reassembler::new();
        let head = parse(b"Ga=T,f=100,m=1;aaaa").unwrap();
        assert_eq!(r.feed(&head), Outcome::Pending);
        let mid = parse(b"Gm=1;bbbb").unwrap();
        assert_eq!(r.feed(&mid), Outcome::Pending);
        let tail = parse(b"Gm=0;cccc").unwrap();
        let Outcome::Done(complete) = r.feed(&tail) else {
            panic!("expected Done");
        };
        assert_eq!(complete.payload, b"aaaabbbbcccc");
        assert_eq!(
            complete.controls,
            vec![(b'a', b"T".to_vec()), (b'f', b"100".to_vec())]
        );
    }

    /// The gauge `felis daemon status` reads: nothing between transmissions,
    /// the running total while one is open, nothing again once it completes.
    #[test]
    fn in_flight_bytes_tracks_the_open_transmission_only() {
        let mut r = Reassembler::new();
        assert_eq!(r.in_flight_bytes(), None);

        let head = parse(b"Ga=T,f=100,m=1;aaaa").unwrap();
        assert_eq!(r.feed(&head), Outcome::Pending);
        assert_eq!(r.in_flight_bytes(), Some(4));

        let mid = parse(b"Gm=1;bbbbbb").unwrap();
        assert_eq!(r.feed(&mid), Outcome::Pending);
        assert_eq!(r.in_flight_bytes(), Some(10));

        let tail = parse(b"Gm=0;cc").unwrap();
        assert_eq!(done_payload(&r.feed(&tail)), b"aaaabbbbbbcc");
        assert_eq!(
            r.in_flight_bytes(),
            None,
            "a completed transmission holds nothing"
        );
    }

    #[test]
    fn delete_mid_transfer_runs_and_drops_the_transfer() {
        let mut r = Reassembler::new();
        assert_eq!(
            r.feed(&parse(b"Ga=T,i=6,m=1;aaaa").unwrap()),
            Outcome::Pending
        );
        let Outcome::Done(delete) = r.feed(&parse(b"Ga=d,d=A,m=1").unwrap()) else {
            panic!("expected the delete to complete");
        };
        assert_eq!(
            delete.controls,
            vec![(b'a', b"d".to_vec()), (b'd', b"A".to_vec())]
        );
        assert_eq!(r.in_flight_bytes(), None);
        let Outcome::Done(tail) = r.feed(&parse(b"Gm=0;bb").unwrap()) else {
            panic!("expected Done");
        };
        assert_eq!(tail.payload, b"bb");
        assert_eq!(tail.controls, Vec::new());
    }

    /// A reset releases the parked bytes and the gauge must say so.
    #[test]
    fn in_flight_bytes_clears_on_reset() {
        let mut r = Reassembler::new();
        let head = parse(b"Ga=T,m=1;aaaa").unwrap();
        assert_eq!(r.feed(&head), Outcome::Pending);
        assert_eq!(r.in_flight_bytes(), Some(4));
        r.reset();
        assert_eq!(r.in_flight_bytes(), None);
    }

    #[test]
    fn missing_m_on_terminal_chunk_finalises() {
        // The spec is "m=1 means more follow", so absence means done.
        let mut r = Reassembler::new();
        let head = parse(b"Ga=T,m=1;aa").unwrap();
        assert_eq!(r.feed(&head), Outcome::Pending);
        let tail = parse(b"G;bb").unwrap();
        assert_eq!(done_payload(&r.feed(&tail)), b"aabb");
    }

    #[test]
    fn continuation_chunk_controls_are_ignored() {
        // A producer that changes `f=` mid-stream must not reach the dispatcher.
        let mut r = Reassembler::new();
        let head = parse(b"Ga=T,f=100,m=1;aa").unwrap();
        drop(r.feed(&head));
        let cont = parse(b"Gf=24,m=0;bb").unwrap();
        let Outcome::Done(complete) = r.feed(&cont) else {
            panic!("expected Done");
        };
        assert_eq!(
            complete.controls,
            vec![(b'a', b"T".to_vec()), (b'f', b"100".to_vec())]
        );
    }

    #[test]
    fn continuation_chunk_nonzero_q_overrides_the_heads() {
        let mut r = Reassembler::new();
        drop(r.feed(&parse(b"Ga=T,i=8,q=0,m=1;aa").unwrap()));
        let Outcome::Done(complete) = r.feed(&parse(b"Gq=2,m=0;bb").unwrap()) else {
            panic!("expected Done");
        };
        assert_eq!(control_u32(&complete.controls, b'q'), Some(2));
    }

    #[test]
    fn repeated_continuation_q_keeps_one_override() {
        let mut r = Reassembler::new();
        drop(r.feed(&parse(b"Ga=T,i=8,q=0,m=1;aa").unwrap()));
        for _ in 0..100 {
            assert_eq!(r.feed(&parse(b"Gq=1,m=1;").unwrap()), Outcome::Pending);
        }
        let Outcome::Done(complete) = r.feed(&parse(b"Gq=2;bb").unwrap()) else {
            panic!("expected Done");
        };
        assert_eq!(
            complete.controls,
            vec![
                (b'a', b"T".to_vec()),
                (b'i', b"8".to_vec()),
                (b'q', b"2".to_vec())
            ]
        );
    }

    #[test]
    fn continuation_chunk_zero_q_keeps_the_heads() {
        let mut r = Reassembler::new();
        drop(r.feed(&parse(b"Ga=T,i=8,q=1,m=1;aa").unwrap()));
        let Outcome::Done(complete) = r.feed(&parse(b"Gq=0,m=0;bb").unwrap()) else {
            panic!("expected Done");
        };
        assert_eq!(control_u32(&complete.controls, b'q'), Some(1));
    }

    #[test]
    fn file_backed_media_complete_despite_m1() {
        // kitty ignores `m=` for `t=f/t/s`; mpv's --vo-kitty-use-shm sends
        // `t=s,…,m=1` every frame with no terminating chunk.
        for medium in [b"f", b"t", b"s"] {
            let mut r = Reassembler::new();
            let mut body = b"Ga=T,f=32,t=".to_vec();
            body.extend_from_slice(medium);
            body.extend_from_slice(b",m=1;bXB2LWtpdHR5");
            let cmd = parse(&body).unwrap();
            let Outcome::Done(complete) = r.feed(&cmd) else {
                panic!("t={} with m=1 must complete", medium[0] as char);
            };
            assert_eq!(complete.payload, b"bXB2LWtpdHR5");
            assert!(complete.controls.iter().all(|(k, _)| *k != b'm'));
        }
    }

    #[test]
    fn file_backed_chunk_drops_in_flight_direct_stream() {
        // kitty's handle_add_command re-initializes `currently_loading`
        // whenever the transmission type is not 'd'.
        let mut r = Reassembler::new();
        assert_eq!(
            r.feed(&parse(b"Ga=T,f=100,m=1;aaaa").unwrap()),
            Outcome::Pending
        );
        let shm = parse(b"Ga=T,t=s,f=32,m=1;bmFtZQ==").unwrap();
        let Outcome::Done(complete) = r.feed(&shm) else {
            panic!("expected Done");
        };
        assert_eq!(complete.payload, b"bmFtZQ==");
        assert!(complete.controls.contains(&(b't', b"s".to_vec())));
        assert!(!complete.controls.contains(&(b'f', b"100".to_vec())));
    }

    #[test]
    fn reset_drops_in_flight_state() {
        let mut r = Reassembler::new();
        drop(r.feed(&parse(b"Ga=T,m=1;aaa").unwrap()));
        r.reset();
        let next = parse(b"Gb=p;zzz").unwrap();
        let Outcome::Done(complete) = r.feed(&next) else {
            panic!("expected Done");
        };
        assert_eq!(complete.payload, b"zzz");
        assert_eq!(complete.controls, vec![(b'b', b"p".to_vec())]);
    }

    #[test]
    fn format_response_ok_with_no_ids() {
        // The spec requires none of i/I/p.
        let bytes = format_response(&ResponseRefs::default(), Status::Ok);
        assert_eq!(bytes, b"\x1b_G;OK\x1b\\");
    }

    #[test]
    fn format_response_ok_with_image_id() {
        let bytes = format_response(
            &ResponseRefs {
                image_id: Some(42),
                ..Default::default()
            },
            Status::Ok,
        );
        assert_eq!(bytes, b"\x1b_Gi=42;OK\x1b\\");
    }

    #[test]
    fn format_response_with_all_ids_in_canonical_order() {
        // The spec's canonical order: image-id, image-number, placement-id.
        let bytes = format_response(
            &ResponseRefs {
                image_id: Some(1),
                image_number: Some(2),
                placement_id: Some(3),
            },
            Status::Ok,
        );
        assert_eq!(bytes, b"\x1b_Gi=1,I=2,p=3;OK\x1b\\");
    }

    #[test]
    fn format_response_error_with_message() {
        let bytes = format_response(
            &ResponseRefs {
                image_id: Some(7),
                ..Default::default()
            },
            Status::Error {
                code: ErrorCode::NotFound,
                message: "image 7 not found",
            },
        );
        assert_eq!(bytes, b"\x1b_Gi=7;ENOENT:image 7 not found\x1b\\");
    }

    #[test]
    fn format_response_error_message_has_injection_bytes_neutralized() {
        // An unfiltered `ESC \` would terminate the APC early and hand the
        // remainder to the PTY; `;` would fake the controls/status split.
        let bytes = format_response(
            &ResponseRefs::default(),
            Status::Error {
                code: ErrorCode::InvalidValue,
                message: "boom\x1b\\;x\x07\x7f",
            },
        );
        assert_eq!(bytes, b"\x1b_G;EINVAL:boom_\\_x__\x1b\\");
    }

    #[test]
    fn format_response_error_with_empty_message_omits_colon() {
        // Producers expect `<code>:<message>` or a bare `<code>`.
        let bytes = format_response(
            &ResponseRefs::default(),
            Status::Error {
                code: ErrorCode::InvalidValue,
                message: "",
            },
        );
        assert_eq!(bytes, b"\x1b_G;EINVAL\x1b\\");
    }

    #[test]
    fn error_code_wire_strings_match_spec() {
        assert_eq!(ErrorCode::NotFound.as_str(), "ENOENT");
        assert_eq!(ErrorCode::InvalidValue.as_str(), "EINVAL");
        assert_eq!(ErrorCode::Unsupported.as_str(), "ENOTSUP");
        assert_eq!(ErrorCode::NoData.as_str(), "ENODATA");
        assert_eq!(ErrorCode::BadImage.as_str(), "EBADF");
        assert_eq!(ErrorCode::IoError.as_str(), "EIO");
    }

    #[test]
    fn formatted_response_round_trips_through_parse() {
        let bytes = format_response(
            &ResponseRefs {
                image_id: Some(99),
                placement_id: Some(5),
                ..Default::default()
            },
            Status::Error {
                code: ErrorCode::BadImage,
                message: "checksum mismatch",
            },
        );
        let inner = bytes
            .strip_prefix(b"\x1b_")
            .and_then(|b| b.strip_suffix(b"\x1b\\"))
            .unwrap();
        let cmd = parse(inner).unwrap();
        assert_eq!(cmd.get(b'i'), Some(b"99".as_slice()));
        assert_eq!(cmd.get(b'p'), Some(b"5".as_slice()));
        assert_eq!(cmd.payload, b"EBADF:checksum mismatch");
    }

    #[test]
    fn payload_exceeding_buffer_limit_signals_overflow() {
        // First chunk fills to (limit - 4); the second asks for 8 more.
        let mut r = Reassembler::new();
        // `parse` cannot build a chunk this big (bodies cap at
        // `APC_BUFFER_LIMIT`), so the `Command` structs are hand-built.
        let near_cap = vec![b'x'; REASSEMBLY_BUFFER_LIMIT - 4];
        let big_head = Command {
            controls: vec![(b'a', b"T".as_slice()), (b'm', b"1".as_slice())],
            payload: &near_cap,
        };
        assert_eq!(r.feed(&big_head), Outcome::Pending);
        let overrun = vec![b'y'; 8];
        let big_tail = Command {
            controls: vec![(b'm', b"0".as_slice())],
            payload: &overrun,
        };
        assert_eq!(
            r.feed(&big_tail),
            Outcome::Overflow {
                controls: vec![(b'a', b"T".to_vec())],
            }
        );
        let recover = parse(b"Ga=T;ok").unwrap();
        assert_eq!(done_payload(&r.feed(&recover)), b"ok");
    }

    #[test]
    fn overflowing_chunk_quiet_override_reaches_the_overflow_reply() {
        let mut r = Reassembler::new();
        let near_cap = vec![b'x'; REASSEMBLY_BUFFER_LIMIT - 4];
        let big_head = Command {
            controls: vec![(b'a', b"T".as_slice()), (b'm', b"1".as_slice())],
            payload: &near_cap,
        };
        assert_eq!(r.feed(&big_head), Outcome::Pending);
        let overrun = vec![b'y'; 8];
        let big_tail = Command {
            controls: vec![(b'q', b"2".as_slice())],
            payload: &overrun,
        };
        assert_eq!(
            r.feed(&big_tail),
            Outcome::Overflow {
                controls: vec![(b'a', b"T".to_vec()), (b'q', b"2".to_vec())],
            }
        );
    }

    #[test]
    fn payload_exactly_at_buffer_limit_is_not_overflow() {
        // The overflow guard is `>`, not `>=`.
        let mut r = Reassembler::new();
        let head = vec![b'x'; REASSEMBLY_BUFFER_LIMIT - 4];
        let big_head = Command {
            controls: vec![(b'a', b"T".as_slice()), (b'm', b"1".as_slice())],
            payload: &head,
        };
        assert_eq!(r.feed(&big_head), Outcome::Pending);
        let tail = vec![b'y'; 4];
        let big_tail = Command {
            controls: vec![(b'm', b"0".as_slice())],
            payload: &tail,
        };
        match r.feed(&big_tail) {
            Outcome::Done(complete) => {
                assert_eq!(
                    complete.payload.len(),
                    REASSEMBLY_BUFFER_LIMIT,
                    "a payload exactly at the cap must assemble in full",
                );
            }
            other => panic!("a payload exactly at the cap must assemble, got {other:?}"),
        }
    }
}
