use super::*;

fn feature_query_controls(controls: &[(u8, &[u8])], payload: &[u8]) -> Option<String> {
    let (mut grid, mut images, mut placements) = handler_state();
    let reply = dispatch_controls(&mut grid, &mut images, &mut placements, controls, payload);
    assert!(
        images.is_empty() && placements.is_empty(),
        "a feature query stores nothing"
    );
    reply.map(|r| String::from_utf8(r).unwrap())
}

fn feature_query(body: &str) -> Option<String> {
    let parsed = kitty_graphics::parse(body.as_bytes()).unwrap();
    feature_query_controls(&parsed.controls, parsed.payload)
}

fn ok_with(keys: &str) -> String {
    format!("\x1b_Gi=31{keys};OK\x1b\\")
}

/// kitty's own feature-query cases (`kitty_tests/graphics.py`), with the
/// keys felis does not implement left out of the expected replies.
#[test]
fn feature_query_echoes_the_supported_keys_per_action() {
    let cases = [
        ("Ga=q,i=31,e=T,s=1,v=1,f=24,U=1", ",e=T,f=24,s=1,v=1,U=1"),
        (
            "Ga=q,i=31,e=T,s=1,v=1,f=24,t=d,C=1,z=-3,N=1",
            ",e=T,t=d,f=24,s=1,v=1,C=1,z=-3",
        ),
        ("Ga=q,i=31,e=t,s=1,v=1,f=8,c=2;abcd", ",e=t,s=1,v=1"),
        ("Ga=q,i=31,e=p,c=2,r=3,U=1,C=1,s=1", ",e=p,c=2,r=3,C=1,U=1"),
        (
            "Ga=q,i=31,e=f,c=2,r=3,z=40,X=1,f=8,s=1",
            ",e=f,s=1,c=2,r=3,z=40,X=1",
        ),
        (
            "Ga=q,i=31,e=a,s=3,v=1,r=2,c=1,z=-1",
            ",e=a,s=3,v=1,r=2,c=1,z=-1",
        ),
        (
            "Ga=q,i=31,e=c,r=1,c=2,w=3,X=1,C=1,U=1",
            ",e=c,r=1,c=2,w=3,X=1,C=1",
        ),
        ("Ga=q,i=31,e=d,d=F,p=2,s=1", ",e=d,d=F,p=2"),
        (
            "Ga=q,i=31,e=T,s=1,v=1,f=24,C=7,N=6,U=2",
            ",e=T,f=24,s=1,v=1",
        ),
        ("Ga=q,i=31,e=t,f=42", ",e=t"),
        ("Ga=q,i=31,e=a,s=4", ",e=a"),
        ("Ga=q,i=31,e=x,s=1,v=1,f=24,U=1", ""),
    ];
    for (query, keys) in cases {
        assert_eq!(feature_query(query), Some(ok_with(keys)), "{query}");
    }
}

#[test]
fn feature_query_leaves_out_what_felis_ignores() {
    assert_eq!(
        feature_query("Ga=q,i=31,e=p,X=3,Y=4,P=7,Q=1,H=2,V=-2,U=1"),
        Some(ok_with(",e=p,U=1")),
        "cell offsets and relative placements parse but do nothing",
    );
    assert_eq!(
        feature_query("Ga=q,i=31,e=t,f=16,N=1,o=z"),
        Some(ok_with(",e=t,o=z")),
        "indexed formats and usage hints are not implemented",
    );
}

#[test]
fn feature_query_reports_values_the_handlers_read_and_leaves_byte_caps_out() {
    let cases = [
        ("Ga=q,i=31,e=p,c=65536,r=2", ",e=p,r=2"),
        (
            "Ga=q,i=31,e=t,f=32,s=5000,v=5000",
            ",e=t,f=32,s=5000,v=5000",
        ),
        ("Ga=q,i=31,e=t,f=100,s=10,v=10", ",e=t,f=100"),
        ("Ga=q,i=31,e=t,f=024,s=1", ",e=t,f=24,s=1"),
        ("Ga=q,i=31,e=t,t=d,S=8,O=4", ",e=t,t=d"),
        ("Ga=q,i=31,e=d,d=x,x=70000", ",e=d,d=x"),
        (
            "Ga=q,i=31,e=d,d=r,x=70000,y=80000",
            ",e=d,d=r,x=70000,y=80000",
        ),
    ];
    for (query, keys) in cases {
        assert_eq!(feature_query(query), Some(ok_with(keys)), "{query}");
    }
}

#[test]
fn a_format_written_with_leading_zeros_transmits_as_reported() {
    let (mut grid, mut images, mut placements) = handler_state();
    let controls: &[(u8, &[u8])] = &[
        (b'a', b"t"),
        (b'i', b"7"),
        (b'f', b"024"),
        (b's', b"1"),
        (b'v', b"1"),
    ];
    let payload = b64(&[0xAA, 0xBB, 0xCC]);
    let reply = dispatch_controls(&mut grid, &mut images, &mut placements, controls, &payload);
    assert_eq!(reply, Some(b"\x1b_Gi=7;OK\x1b\\".to_vec()));
    assert_eq!(images.get(ImageId(7)).unwrap().format, ImageFormat::Rgb24);
}

#[cfg(unix)]
#[test]
fn feature_query_reports_the_file_media_on_unix() {
    assert_eq!(
        feature_query("Ga=q,i=31,e=t,t=s,S=8,O=4;bmFtZQ"),
        Some(ok_with(",e=t,t=s,S=8,O=4")),
    );
}

#[cfg(windows)]
#[test]
fn feature_query_reports_direct_transmission_alone_on_windows() {
    assert_eq!(
        feature_query("Ga=q,i=31,e=t,t=s,S=8,O=4;bmFtZQ"),
        Some(ok_with(",e=t")),
    );
}

#[test]
fn feature_query_with_an_e_that_is_not_one_letter_is_dropped() {
    assert_eq!(feature_query("Ga=q,i=31,e=TT,U=1"), None);
    assert_eq!(feature_query("Ga=q,i=31,e=1,U=1"), None);
}

#[test]
fn e_outside_a_query_is_ignored() {
    let (ctrls, payload) = tiny_rgba_controls();
    let mut all = vec![
        (b'a', b"t".as_slice()),
        (b'i', b"7".as_slice()),
        (b'e', b"T".as_slice()),
    ];
    all.extend_from_slice(&ctrls);
    let (mut grid, mut images, mut placements) = handler_state();
    let reply = dispatch_controls(&mut grid, &mut images, &mut placements, &all, &payload);
    assert_eq!(reply, Some(b"\x1b_Gi=7;OK\x1b\\".to_vec()));
    assert!(!images.is_empty());
}

const QUERY_KEYS: &[u8] = b"tofsvSOpxywhcrCUzXYdPQHVNm";

const QUERY_VALUES: &[&str] = &[
    "0",
    "1",
    "2",
    "3",
    "-3",
    "024",
    "100",
    "65535",
    "65536",
    "4294967295",
    "d",
    "f",
    "s",
    "z",
    "F",
    "r",
    "x",
];

/// Splits a reply into its keys after `i=31`, or `None` when it is not
/// an `OK` addressed to image 31.
fn reply_keys(reply: &str) -> Option<Vec<(u8, String)>> {
    let body = reply
        .strip_prefix("\x1b_Gi=31")?
        .strip_suffix(";OK\x1b\\")?;
    body.split(',')
        .skip(1)
        .map(|kv| Some((kv.as_bytes()[0], kv.get(2..)?.to_owned())))
        .collect()
}

proptest! {
    /// Whatever the query holds, the reply names its action first and
    /// then only keys the query carried, each with the query's value
    /// read as a number where it is one; never an addressing or framing
    /// key. Sending the reply's keys back answers the same reply, and
    /// `q=` or a missing `i=` silences it.
    #[test]
    fn a_feature_query_echoes_a_normalized_subset_of_its_own_keys(
        action in prop::sample::select(b"tTqpdfacxZ".to_vec()),
        keys in proptest::collection::vec(
            (prop::sample::select(QUERY_KEYS.to_vec()), prop::sample::select(QUERY_VALUES.to_vec())),
            0..12,
        ),
    ) {
        let e = [action];
        let mut controls: Vec<(u8, &[u8])> = vec![(b'a', b"q"), (b'i', b"31"), (b'e', &e)];
        controls.extend(keys.iter().map(|(k, v)| (*k, v.as_bytes())));
        let reply = feature_query_controls(&controls, b"");
        let echoed = reply.as_deref().and_then(reply_keys);
        prop_assert!(echoed.is_some(), "unexpected reply {reply:?}");
        let echoed = echoed.unwrap();

        let known = b"tTqpdfac".contains(&action);
        prop_assert_eq!(echoed.first().map(|(k, v)| (*k, v.clone())),
            known.then(|| (b'e', char::from(action).to_string())));
        for (key, value) in echoed.iter().skip(1) {
            prop_assert!(!b"aiImqe".contains(key), "{} echoed", char::from(*key));
            let sent = keys.iter().rev().find(|(k, _)| k == key).map(|(_, v)| *v);
            prop_assert!(sent.is_some(), "{} was not in the query", char::from(*key));
            let sent = sent.unwrap();
            let normalized = sent.parse::<i64>().map_or_else(|_| sent.to_owned(), |n| n.to_string());
            prop_assert_eq!(value, &normalized);
            prop_assert_ne!(value.as_str(), "0");
        }

        let mut again: Vec<(u8, &[u8])> = vec![(b'a', b"q"), (b'i', b"31"), (b'e', &e)];
        again.extend(echoed.iter().skip(1).map(|(k, v)| (*k, v.as_bytes())));
        prop_assert_eq!(feature_query_controls(&again, b""), reply);

        let mut quiet = controls.clone();
        quiet.push((b'q', b"1"));
        prop_assert_eq!(feature_query_controls(&quiet, b""), None);
        controls.retain(|(k, _)| *k != b'i');
        prop_assert_eq!(feature_query_controls(&controls, b""), None);
    }
}
