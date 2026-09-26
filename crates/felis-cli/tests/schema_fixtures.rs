//! The published bundle read as a consumer reads it: the objects the
//! reference documents must validate, and the fixtures it must reject.
//! `cli_sessions.rs` and `cli_bridge.rs` cover what a live run
//! produces; what is left here is the malformed vocabulary it cannot.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;

#[path = "common/schema.rs"]
mod schema;

/// The bundle rejects what the reference forbids: a truncated or
/// uppercased id, a numeric attachment id, an out-of-range dimension,
/// a missing or foreign `v`.
#[test]
fn every_invalid_output_fixture_fails_the_class_it_claims() {
    for (name, fixture) in schema::invalid_fixtures("out-") {
        let against = fixture["against"]
            .as_str()
            .expect("`against` names a class");
        // The repaired twin is what makes the rejection attributable:
        // without it a fixture whose targeted constraint is gone still
        // fails, on an unrelated defect such as a field the class
        // requires.
        assert!(
            !schema::cli_def_rejects(against, &fixture["repaired"]),
            "{name}'s `repaired` twin must validate against `{against}`",
        );
        assert!(
            schema::cli_def_rejects(against, &fixture["instance"]),
            "{name} validated against `{against}` but must not",
        );
    }
}

/// Unknown top-level fields, unknown parameters, unknown ops, and
/// integers past their bound are all refusals of the request grammar.
#[test]
fn every_invalid_request_fixture_fails_the_request_schema() {
    for (name, line) in schema::invalid_fixtures("req-") {
        assert!(
            !schema::bridge_request_is_valid(&line),
            "{name} validated as a request but must not",
        );
    }
}

/// The published request grammar is not merely strict: the lines the
/// reference documents still pass it.
#[test]
fn the_documented_request_shapes_validate() {
    let hex = "0123456789abcdef0123456789abcdef";
    for request in [
        json!({"v": 1, "id": "a", "op": "sessions.list"}),
        json!({"v": 1, "id": 7, "op": "sessions.list", "params": {"tags": ["work"]}}),
        json!({"v": 1, "id": "a", "op": "sessions.info", "params": {"session": "0123abcd"}}),
        json!({"v": 1, "id": "a", "op": "sessions.spawn",
               "params": {"cwd": "/tmp", "env": [["TERM", "xterm-felis"]], "rows": 24,
                          "cols": 80, "tags": ["work"], "cmd": ["/bin/sh"]}}),
        // An omitted optional may also be spelled `null`, which the
        // bridge reads as absence.
        json!({"v": 1, "id": "a", "op": "sessions.spawn",
               "params": {"env": null, "rows": null, "cols": null}}),
        json!({"v": 1, "id": "a", "op": "sessions.send",
               "params": {"session": hex, "text": "ls\n", "raw": false}}),
        json!({"v": 1, "id": "a", "op": "sessions.switch",
               "params": {"to": hex, "from": "0x0123ABCD", "attachment": "7"}}),
        json!({"v": 1, "id": "a", "op": "sessions.tag",
               "params": {"session": hex, "add": ["x"], "remove": []}}),
        json!({"v": 1, "id": "a", "op": "sessions.capture",
               "params": {"session": hex, "source": "scrollback", "ansi": true, "lines": 40}}),
        // `integer` counts `1.0` as the integer 1, and so does the
        // bridge that answers these lines.
        json!({"v": 1.0, "id": 1.0, "op": "sessions.capture",
               "params": {"session": hex, "lines": 40.0}}),
        json!({"v": 1, "id": "a", "op": "sessions.search",
               "params": {"session": hex, "pattern": "boom", "regex": true,
                          "case_insensitive": false}}),
        json!({"v": 1, "id": "a", "op": "notifications.subscribe", "params": {}}),
        json!({"v": 1, "id": "a", "op": "cancel", "params": {"target": "b"}}),
    ] {
        assert!(
            schema::bridge_request_is_valid(&request),
            "{request} is documented but does not validate",
        );
    }
}

/// Every region named by the CLI's `--source` and the wire validates.
#[test]
fn every_documented_capture_source_validates() {
    let hex = "0123456789abcdef0123456789abcdef";
    for source in ["visible", "scrollback", "command-output", "last-command"] {
        let request = json!({"v": 1, "id": "a", "op": "sessions.capture",
                             "params": {"session": hex, "source": source}});
        assert!(
            schema::bridge_request_is_valid(&request),
            "`{source}` is a documented region but does not validate",
        );
    }
}

/// `rows` and `cols` travel together, and the grammar counts an
/// explicit `null` as absence exactly as the bridge does: the two
/// whole geometries pass, the two half ones do not.
#[test]
fn a_spawn_names_its_whole_grid_or_none_of_it() {
    let spawn = |params| json!({"v": 1, "id": "a", "op": "sessions.spawn", "params": params});
    for params in [json!({}), json!({"rows": null, "cols": null})] {
        assert!(
            schema::bridge_request_is_valid(&spawn(params.clone())),
            "{params} names no grid and must validate",
        );
    }
    for params in [json!({"rows": 24}), json!({"rows": null, "cols": 80})] {
        assert!(
            !schema::bridge_request_is_valid(&spawn(params.clone())),
            "{params} is half a geometry and must not validate",
        );
    }
}

/// The envelope shapes the reference's table lists, each validated as
/// the class it is: a consumer that keys on `event` gets the same
/// partition the schema draws.
#[test]
fn the_documented_envelope_shapes_validate() {
    for object in [
        json!({"v": 1, "sessions": []}),
        json!({"v": 1, "error": {"kind": "no_match", "message": "nope"}}),
        json!({"v": 1, "error": {"kind": "refused", "message": "held", "sessions": 2}}),
        json!({"v": 1, "event": "end", "count": 3}),
        json!({"v": 1, "event": "end", "count": 3, "exit_code": 1}),
        json!({"v": 1, "event": "error", "error": {"kind": "timeout", "message": "late"}}),
        json!({"v": 1, "event": "lag", "dropped": 7}),
        json!({"v": 1, "row": -1, "text": "hi", "soft_wrap_continued": false}),
    ] {
        assert!(
            schema::cli_object_is_valid(&object),
            "{object} is documented but does not validate",
        );
    }
}

/// A kind minted after a consumer's validator was written must still
/// validate: the vocabulary grows additively within an epoch.
#[test]
fn an_unknown_error_kind_still_validates() {
    assert!(schema::cli_object_is_valid(
        &json!({"v": 1, "error": {"kind": "a_kind_from_a_later_release", "message": "?"}})
    ));
}

/// A 64-bit bound is enforced only to `f64` precision: one past the
/// maximum is admitted, and rejection resumes at the next
/// representable `f64`, which is what the `above-u64` and `above-i64`
/// fixtures carry (cli.md "JSON Schema").
#[test]
fn a_64_bit_bound_is_enforced_only_to_f64_precision() {
    let unsigned = |errors: &str| {
        parse(&format!(
            r#"{{"path": "/p", "exists": true, "client": "felis 0.1.0",
                 "errors": {errors}, "warnings": 0, "diagnostics": []}}"#
        ))
    };
    let signed = |row: &str| {
        parse(&format!(
            r#"{{"row": {row}, "text": "hi", "soft_wrap_continued": false}}"#
        ))
    };

    for (def, exact, past, rejected) in [
        (
            "CheckResult",
            unsigned("18446744073709551615"),
            unsigned("18446744073709551616"),
            unsigned("18446744073709555712"),
        ),
        (
            "CaptureRow",
            signed("9223372036854775807"),
            signed("9223372036854775808"),
            signed("9223372036854777856"),
        ),
    ] {
        assert!(
            !schema::cli_def_rejects(def, &exact),
            "{def}'s exact maximum must validate",
        );
        assert!(
            !schema::cli_def_rejects(def, &past),
            "{def} one past its maximum is indistinguishable from the maximum \
             through f64; a rejection here means the limitation is gone and the \
             `above-` fixtures should move down to the true boundary",
        );
        assert!(
            schema::cli_def_rejects(def, &rejected),
            "{def} at the next representable f64 above its maximum must be rejected",
        );
    }
}

/// Integers past 2^53 are written here as text, because a Rust literal
/// for them would not fit the type `json!` picks.
fn parse(text: &str) -> serde_json::Value {
    serde_json::from_str(text).expect("the probe is JSON")
}
