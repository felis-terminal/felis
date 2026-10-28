//! The published bundle read as a consumer reads it: the objects the
//! reference documents must validate, and the fixtures it must reject.
//! `cli_sessions.rs` covers what a live run produces; what is left
//! here is the malformed vocabulary it cannot.

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
