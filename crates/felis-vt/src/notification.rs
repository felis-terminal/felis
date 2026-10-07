//! OSC 9 / 99 / 777 desktop notification parsers (decode-only).
//!
//! Wire forms match `docs/reference/protocols/notifications.md`. Each parser
//! splits off only the fields its wire form defines, preserving any literal
//! `;` inside the payload.

use crate::split_osc_first;

// `Urgency` is the wire type (`docs/reference/protocols/notifications.md`).
pub use felis_protocol::messages::Urgency;

/// `title` is `None` for the bare OSC 9 form. `id` is the producer's
/// OSC 99 `i=` token, opaque and echoed back so a multiplexer can correlate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(
    feature = "state-dump",
    derive(serde::Serialize, serde::Deserialize),
    serde(default)
)]
pub struct Notification {
    pub title: Option<String>,
    /// May be empty (a title-only OSC 99).
    pub body: String,
    pub urgency: Urgency,
    pub id: Option<String>,
}

/// `OSC 9 ; <message> ST`. The caller must not route `ConEmu`'s
/// `OSC 9 ; <digit> ; …` progress subcommands here (window chrome, not a
/// notification, per `docs/reference/protocols/notifications.md`): this
/// parser treats everything after the code as the body.
#[must_use]
pub fn parse_osc9(body: &[u8]) -> Option<Notification> {
    let (code, message) = split_osc_first(body);
    if code != b"9" {
        return None;
    }
    let body = String::from_utf8_lossy(message.unwrap_or_default()).into_owned();
    if body.is_empty() {
        return None;
    }
    Some(Notification {
        title: None,
        body,
        urgency: Urgency::Normal,
        id: None,
    })
}

/// `OSC 777 ; notify ; <title> ; <body> ST`. Any other subcommand returns
/// `None`.
#[must_use]
pub fn parse_osc777(body: &[u8]) -> Option<Notification> {
    let (code, rest) = split_osc_first(body);
    if code != b"777" {
        return None;
    }
    let (subcommand, rest) = split_osc_first(rest?);
    if subcommand != b"notify" {
        return None;
    }
    let (title, body) = split_osc_first(rest?);
    let title = (!title.is_empty()).then(|| String::from_utf8_lossy(title).into_owned());
    // The wire form spends its last separator on the title, so a `;` past
    // it is message text.
    let body = String::from_utf8_lossy(body.unwrap_or_default()).into_owned();
    if title.is_none() && body.is_empty() {
        return None;
    }
    Some(Notification {
        title,
        body,
        urgency: Urgency::Normal,
        id: None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// `p=title` (the default when `p` is absent).
    Title,
    /// `p=body`.
    Body,
}

/// Reassembly across chunks (sharing one `id`) is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc99 {
    /// `done` is `d=1` (the default); `d=0` means more chunks with this
    /// `id` follow. `urgency` is `Some` only when this chunk carried `u=`,
    /// so an earlier `u=2` is not clobbered by a later chunk's absence.
    Payload {
        /// Producer `i=`, empty string when absent.
        id: String,
        done: bool,
        kind: PayloadKind,
        /// Decoded fragment text (Base64 already decoded when `e=1`).
        text: String,
        urgency: Option<Urgency>,
    },
    /// `p=?` capability query. The caller emits the truthful reply.
    Query {
        /// Echoed `i=` (empty string → the caller uses `i=0`).
        id: String,
    },
    /// Recognized but not acted on (`p=close`,
    /// `p=alive`, `p=icon`, `p=buttons`, action requests).
    Ignored,
}

/// `None` only if the code is not `99`; malformed-but-`99` input degrades
/// to [`Osc99::Ignored`] or a best-effort payload.
#[must_use]
pub fn parse_osc99(body: &[u8]) -> Option<Osc99> {
    let (code, rest) = split_osc_first(body);
    if code != b"99" {
        return None;
    }
    let (metadata, payload_bytes) = split_osc_first(rest.unwrap_or_default());
    let payload_bytes = payload_bytes.unwrap_or_default();

    let mut id = String::new();
    let mut done = true;
    let mut kind = PayloadKind::Title;
    let mut urgency: Option<Urgency> = None;
    let mut base64 = false;
    let mut is_query = false;
    let mut ignore = false;

    for pair in metadata.split(|b| *b == b':') {
        if pair.is_empty() {
            continue;
        }
        let Some(eq) = pair.iter().position(|&b| b == b'=') else {
            continue;
        };
        let key = &pair[..eq];
        let val = &pair[eq + 1..];
        match key {
            b"i" => id = String::from_utf8_lossy(val).into_owned(),
            b"d" => done = val != b"0",
            b"e" => base64 = val == b"1",
            b"u" => {
                urgency = match val {
                    b"0" => Some(Urgency::Low),
                    b"1" => Some(Urgency::Normal),
                    b"2" => Some(Urgency::Critical),
                    _ => urgency,
                };
            }
            b"p" => match val {
                b"title" => kind = PayloadKind::Title,
                b"body" => kind = PayloadKind::Body,
                b"?" => is_query = true,
                _ => ignore = true,
            },
            // a=report / a=focus and everything else (f/t/n/g/s/w/o/c) are
            // accepted and ignored: the title/body still relays, the action
            // never fires.
            _ => {}
        }
    }

    if is_query {
        return Some(Osc99::Query { id });
    }
    if ignore {
        return Some(Osc99::Ignored);
    }

    let text = if base64 {
        // Invalid Base64 drops the chunk rather than relaying garbage.
        let decoded = crate::kitty_graphics::base64::decode(payload_bytes)?;
        String::from_utf8_lossy(&decoded).into_owned()
    } else {
        String::from_utf8_lossy(payload_bytes).into_owned()
    };

    Some(Osc99::Payload {
        id,
        done,
        kind,
        text,
        urgency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(raw: &[&[u8]]) -> Vec<u8> {
        raw.join(&b';')
    }

    #[test]
    fn osc9_takes_the_whole_message_as_body() {
        let n = parse_osc9(&parts(&[b"9", b"Build done"])).unwrap();
        assert_eq!(n.title, None);
        assert_eq!(n.body, "Build done");
        assert_eq!(n.urgency, Urgency::Normal);
    }

    #[test]
    fn osc9_keeps_semicolons_inside_the_body() {
        // OSC 9 spends its only separator on the code.
        let n = parse_osc9(&parts(&[b"9", b"a", b"b", b"c"])).unwrap();
        assert_eq!(n.body, "a;b;c");
    }

    #[test]
    fn osc9_empty_message_is_rejected() {
        assert!(parse_osc9(&parts(&[b"9"])).is_none());
        assert!(parse_osc9(&parts(&[b"9", b""])).is_none());
    }

    #[test]
    fn osc777_notify_splits_title_and_body() {
        let n = parse_osc777(&parts(&[b"777", b"notify", b"Title", b"Body"])).unwrap();
        assert_eq!(n.title.as_deref(), Some("Title"));
        assert_eq!(n.body, "Body");
    }

    #[test]
    fn osc777_title_only_keeps_empty_body() {
        let n = parse_osc777(&parts(&[b"777", b"notify", b"Just a title"])).unwrap();
        assert_eq!(n.title.as_deref(), Some("Just a title"));
        assert_eq!(n.body, "");
    }

    #[test]
    fn osc777_non_notify_subcommand_is_ignored() {
        assert!(parse_osc777(&parts(&[b"777", b"clipboard", b"x"])).is_none());
    }

    #[test]
    fn osc777_keeps_semicolons_inside_the_body() {
        let n = parse_osc777(&parts(&[b"777", b"notify", b"Title", b"a", b"b", b"c"])).unwrap();
        assert_eq!(n.title.as_deref(), Some("Title"));
        assert_eq!(n.body, "a;b;c");
    }

    #[test]
    fn osc99_keeps_semicolons_inside_the_payload() {
        // The metadata boundary is the last `;` the wire form claims.
        let ev = parse_osc99(&parts(&[b"99", b"", b"a", b"b", b"c"])).unwrap();
        match ev {
            Osc99::Payload { text, .. } => assert_eq!(text, "a;b;c"),
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn osc99_plain_payload_defaults_to_title() {
        let ev = parse_osc99(&parts(&[b"99", b"", b"Hello"])).unwrap();
        match ev {
            Osc99::Payload {
                kind, text, done, ..
            } => {
                assert_eq!(kind, PayloadKind::Title);
                assert_eq!(text, "Hello");
                assert!(done, "absent d= means complete");
            }
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn osc99_decodes_base64_body_with_urgency() {
        let ev = parse_osc99(&parts(&[b"99", b"p=body:u=2:e=1", b"ZG9uZQ=="])).unwrap();
        match ev {
            Osc99::Payload {
                kind,
                text,
                urgency,
                ..
            } => {
                assert_eq!(kind, PayloadKind::Body);
                assert_eq!(text, "done");
                assert_eq!(urgency, Some(Urgency::Critical));
            }
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn osc99_urgency_low_and_normal_map_explicitly() {
        // Deleting the u=0 / u=1 arms would silently fall through to "unset".
        let low = parse_osc99(&parts(&[b"99", b"u=0", b"x"])).unwrap();
        match low {
            Osc99::Payload { urgency, .. } => assert_eq!(urgency, Some(Urgency::Low)),
            other => panic!("expected payload, got {other:?}"),
        }
        let normal = parse_osc99(&parts(&[b"99", b"u=1", b"x"])).unwrap();
        match normal {
            Osc99::Payload { urgency, .. } => assert_eq!(urgency, Some(Urgency::Normal)),
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn osc99_explicit_p_title_is_not_ignored() {
        // The default-title test omits `p`; only this exercises `p=title`.
        let ev = parse_osc99(&parts(&[b"99", b"p=title", b"Hi"])).unwrap();
        match ev {
            Osc99::Payload { kind, text, .. } => {
                assert_eq!(kind, PayloadKind::Title);
                assert_eq!(text, "Hi");
            }
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn osc99_d0_marks_more_chunks_coming() {
        let ev = parse_osc99(&parts(&[b"99", b"i=ab:d=0", b"par"])).unwrap();
        match ev {
            Osc99::Payload { id, done, .. } => {
                assert_eq!(id, "ab");
                assert!(!done, "d=0 means another chunk follows");
            }
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn osc99_query_is_recognized() {
        let ev = parse_osc99(&parts(&[b"99", b"i=7:p=?", b""])).unwrap();
        assert_eq!(ev, Osc99::Query { id: "7".into() });
    }

    #[test]
    fn osc99_close_and_buttons_are_accepted_but_ignored() {
        assert_eq!(
            parse_osc99(&parts(&[b"99", b"i=1:p=close", b""])).unwrap(),
            Osc99::Ignored
        );
        assert_eq!(
            parse_osc99(&parts(&[b"99", b"p=buttons", b"Yes"])).unwrap(),
            Osc99::Ignored
        );
    }

    #[test]
    fn osc99_action_request_still_relays_the_payload() {
        // a=report is rejected-by-design, but the title must still relay.
        let ev = parse_osc99(&parts(&[b"99", b"a=report:i=9", b"Click me"])).unwrap();
        match ev {
            Osc99::Payload { text, id, .. } => {
                assert_eq!(text, "Click me");
                assert_eq!(id, "9");
            }
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn osc99_invalid_base64_drops_the_chunk() {
        assert!(parse_osc99(&parts(&[b"99", b"e=1", b"!!!not base64!!!"])).is_none());
    }

    #[test]
    fn wrong_code_returns_none() {
        assert!(parse_osc9(&parts(&[b"8", b"x"])).is_none());
        assert!(parse_osc777(&parts(&[b"99", b"notify"])).is_none());
        assert!(parse_osc99(&parts(&[b"9", b""])).is_none());
    }
}
