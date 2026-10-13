//! Property tests for the OSC 7501 decoder: a valid report survives any
//! spec-permitted respelling, noise the spec says to ignore changes nothing,
//! and one over-limit occurrence anywhere drops the whole report.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use felis_vt::program_status::limits::{
    APP_MAX, ID_DEPTH_MAX, ID_MAX, ID_SEGMENT_MAX, KEY_MAX, MSG_DECODED_MAX, SEQUENCE_MAX,
    TITLE_DECODED_MAX, TITLE_ENCODED_MAX,
};
use felis_vt::program_status::{Kind, Osc7501, Report, State, Terminator, parse_osc7501};
use proptest::prelude::*;

/// Independent of `felis_protocol::base64`, which the decoder uses.
fn b64(bytes: &[u8], padded: bool) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(A[(n >> (18 - 6 * i) & 63) as usize]));
        }
        if padded {
            for _ in chunk.len()..3 {
                out.push('=');
            }
        }
    }
    out
}

const NAME: &str = "[A-Za-z0-9_.+-]";

fn re(pattern: &str) -> proptest::string::RegexGeneratorStrategy<String> {
    proptest::string::string_regex(pattern).unwrap()
}

fn id() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        proptest::collection::vec(
            re(&format!("{NAME}{{1,{ID_SEGMENT_MAX}}}")),
            1..=ID_DEPTH_MAX
        )
        .prop_map(|segs| segs.join("/"))
        .prop_filter("id total cap", |id| id.len() <= ID_MAX),
    ]
}

fn state() -> impl Strategy<Value = State> {
    proptest::sample::select(State::ALL.to_vec())
}

/// `\PC` excludes every control character the spec refuses (and more).
/// Four bytes per char at most keeps `max_chars` within `cap` bytes; the
/// second arm lands on the cap exactly, with a multi-byte char at the end.
fn text(max_chars: usize, cap: usize) -> impl Strategy<Value = String> {
    prop_oneof![
        re(&format!("\\PC{{0,{max_chars}}}")),
        Just(format!("{}\u{e9}", "a".repeat(cap - 2))),
    ]
}

fn valid_report() -> impl Strategy<Value = Report> {
    (
        id(),
        state(),
        proptest::option::of(proptest::sample::select(Kind::ALL.to_vec())),
        proptest::option::of(0u8..=100),
        proptest::option::of(re(&format!("{NAME}{{1,{APP_MAX}}}"))),
        proptest::option::of(text(TITLE_DECODED_MAX / 4, TITLE_DECODED_MAX)),
        proptest::option::of(text(MSG_DECODED_MAX / 4, MSG_DECODED_MAX)),
    )
        .prop_map(|(id, state, kind, progress, app, title, msg)| Report {
            id,
            state,
            kind: kind.filter(|_| state == State::Blocked),
            progress: progress.filter(|_| matches!(state, State::Working | State::Blocked)),
            app,
            title,
            msg,
        })
}

/// One `key=value` per present field; the root id is spelled by omission.
fn pairs(r: &Report, padded: bool) -> Vec<String> {
    let mut out = vec![format!("state={}", r.state.as_str())];
    if !r.id.is_empty() {
        out.push(format!("id={}", r.id));
    }
    if let Some(k) = r.kind {
        out.push(format!("kind={}", k.as_str()));
    }
    if let Some(p) = r.progress {
        out.push(format!("progress={p}"));
    }
    if let Some(a) = &r.app {
        out.push(format!("app={a}"));
    }
    if let Some(t) = &r.title {
        out.push(format!("title={}", b64(t.as_bytes(), padded)));
    }
    if let Some(m) = &r.msg {
        out.push(format!("msg={}", b64(m.as_bytes(), padded)));
    }
    out
}

fn decode(payload: &str, terminator: Terminator) -> Osc7501 {
    parse_osc7501(format!("7501;{payload}").as_bytes(), terminator).unwrap()
}

const fn wire_len(payload: &str) -> usize {
    "\x1b]7501;".len() + payload.len() + "\x1b\\".len()
}

fn whitespace() -> impl Strategy<Value = String> {
    re("[ \t]{0,2}")
}

fn unknown_key() -> impl Strategy<Value = String> {
    re(&format!("[a-z]{{1,{KEY_MAX}}}")).prop_filter("known key", |k| {
        !["state", "id", "kind", "progress", "app", "title", "msg"].contains(&k.as_str())
    })
}

/// Pairs the spec says to skip or ignore. None of them is a well-formed
/// pair for a known key, and no key exceeds the key cap.
fn noise() -> impl Strategy<Value = String> {
    prop_oneof![
        // Unknown key, well-formed value.
        (unknown_key(), "[A-Za-z0-9_.,+/=-]{0,16}").prop_map(|(k, v)| format!("{k}={v}")),
        // Unknown key, malformed value.
        (unknown_key(), "[ -~]{0,16}").prop_map(|(k, v)| format!("{k}=x {v}")),
        // No `=` at all.
        "[a-z]{0,16}",
        // Empty key.
        "=[A-Za-z0-9]{0,8}".prop_map(String::from),
        // A key outside `[a-z]+`.
        "[A-Z0-9][a-z]{0,15}=[a-z]{0,8}".prop_map(String::from),
        // A known non-id key whose value has a byte outside the value set.
        (
            proptest::sample::select(vec!["state", "kind", "progress", "app", "title", "msg"]),
            "[a-z]{0,4}",
            "[!-)*;<>?@\\[\\]^`{|}~]",
        )
            .prop_map(|(k, a, bad)| format!("{k}={a}{bad}")),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Totality over arbitrary bytes, for both terminators.
    #[test]
    fn decoder_is_total(payload in proptest::collection::vec(any::<u8>(), 0..5000), bel in any::<bool>()) {
        let mut body = b"7501;".to_vec();
        body.extend_from_slice(&payload);
        let terminator = if bel { Terminator::Bel } else { Terminator::St };
        prop_assert!(parse_osc7501(&body, terminator).is_some());
    }

    /// Spelling a valid report in any order, padded or not, with whitespace
    /// around keys and values, decodes to exactly that report.
    #[test]
    fn valid_report_round_trips(
        report in valid_report(),
        padded in any::<bool>(),
        order in any::<proptest::sample::Index>(),
        pads in proptest::collection::vec((whitespace(), whitespace(), whitespace(), whitespace()), 8),
    ) {
        let mut ps = pairs(&report, padded);
        let n = ps.len();
        ps.rotate_left(order.index(n));
        let payload = ps
            .iter()
            .zip(&pads)
            .map(|(p, (a, b, c, d))| {
                let (k, v) = p.split_once('=').unwrap();
                format!("{a}{k}{b}={c}{v}{d}")
            })
            .collect::<Vec<_>>()
            .join(":");
        prop_assume!(wire_len(&payload) <= SEQUENCE_MAX);
        prop_assert_eq!(decode(&payload, Terminator::St), Osc7501::Report(report));
    }

    /// Pairs the spec says to skip or ignore change nothing, wherever they sit.
    #[test]
    fn ignorable_pairs_change_nothing(
        report in valid_report(),
        noise in proptest::collection::vec((noise(), any::<proptest::sample::Index>()), 1..6),
    ) {
        let mut ps = pairs(&report, true);
        for (pair, at) in noise {
            let at = at.index(ps.len() + 1);
            ps.insert(at, pair);
        }
        let payload = ps.join(":");
        prop_assume!(wire_len(&payload) <= SEQUENCE_MAX);
        prop_assert_eq!(decode(&payload, Terminator::St), Osc7501::Report(report));
    }

    /// One poisoned occurrence anywhere, even before a later valid duplicate
    /// of the same key, drops the whole report.
    #[test]
    fn one_poisoned_occurrence_drops_the_report(
        report in valid_report(),
        poison in prop_oneof![
            (APP_MAX + 1..APP_MAX + 8).prop_map(|n| (format!("app={}", "a".repeat(n)), "app")),
            Just(("msg=a".to_owned(), "msg")),
            Just((format!("title={}", "A".repeat(TITLE_ENCODED_MAX + 4)), "title")),
            Just((format!("msg={}", b64(b"a\x07b", true)), "msg")),
            Just((format!("title={}", b64(&[0xC0, 0x80], true)), "title")),
            Just(("id=a//b".to_owned(), "id")),
            Just(("id=a b".to_owned(), "id")),
            Just((format!("{}=x", "k".repeat(KEY_MAX + 1)), "key")),
        ],
        at in any::<proptest::sample::Index>(),
    ) {
        let mut ps = pairs(&report, true);
        let at = at.index(ps.len() + 1);
        ps.insert(at, poison.0.clone());
        let payload = ps.join(":");
        prop_assume!(wire_len(&payload) <= SEQUENCE_MAX);
        let out = decode(&payload, Terminator::St);
        prop_assert!(matches!(out, Osc7501::Dropped(_)), "{} survived: {out:?}", poison.1);
    }

    /// Whatever a near-valid payload decodes to, an accepted report keeps
    /// every invariant consumers rely on.
    #[test]
    fn accepted_reports_keep_their_invariants(
        ps in proptest::collection::vec(
            (
                proptest::sample::select(vec!["state", "id", "kind", "progress", "app", "title", "msg", "x"]),
                prop_oneof![
                    proptest::sample::select(vec![
                        "idle", "working", "done", "blocked", "error", "clear", "auth",
                        "permission", "question", "0", "100", "101", "a/b", "a//b", "",
                    ]).prop_map(String::from),
                    "[A-Za-z0-9_.,+/=-]{0,40}",
                    "[ -~]{0,8}",
                ],
            ),
            0..10,
        ),
    ) {
        let payload = ps.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(":");
        let Osc7501::Report(r) = decode(&payload, Terminator::St) else { return Ok(()) };
        if r.kind.is_some() {
            prop_assert_eq!(r.state, State::Blocked);
        }
        if let Some(p) = r.progress {
            prop_assert!(p <= 100);
            prop_assert!(matches!(r.state, State::Working | State::Blocked));
        }
        if let Some(app) = &r.app {
            prop_assert!((1..=APP_MAX).contains(&app.len()));
            prop_assert!(app.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b)));
        }
        if !r.id.is_empty() {
            prop_assert!(r.id.len() <= ID_MAX);
            let segs: Vec<&str> = r.id.split('/').collect();
            prop_assert!(segs.len() <= ID_DEPTH_MAX);
            for s in segs {
                prop_assert!((1..=ID_SEGMENT_MAX).contains(&s.len()));
                prop_assert!(s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b)));
            }
        }
        for (t, cap) in [(&r.title, TITLE_DECODED_MAX), (&r.msg, MSG_DECODED_MAX)] {
            let Some(t) = t else { continue };
            prop_assert!(t.len() <= cap);
            prop_assert!(!t.chars().any(char::is_control));
        }
    }
}
