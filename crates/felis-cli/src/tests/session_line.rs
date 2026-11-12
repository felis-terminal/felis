use super::*;

fn session(rows: u16, cols: u16) -> SessionInfo {
    SessionInfo {
        dims: felis_protocol::messages::GridDims {
            rows,
            cols,
            pixel_w: 0,
            pixel_h: 0,
        },
        ..test_support::session_info(1)
    }
}

#[test]
fn format_session_line_drops_empty_optional_labels() {
    let s = session(24, 80);
    assert_eq!(format_session_line(&s, "00000000"), "00000000  24x80",);
}

#[test]
fn format_session_line_marks_a_post_exit_grace_corpse() {
    let s = SessionInfo {
        exited: true,
        ..session(24, 80)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  24x80  (exited)",
    );
}

#[test]
fn format_session_line_includes_age_title_and_cwd_when_present() {
    let s = SessionInfo {
        title: Some("zsh: ~/work".into()),
        cwd: Some("file://localhost/home/me/work".into()),
        idle_seconds: Some(75),
        ..session(30, 100)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  30x100  1m  zsh: ~/work  file://localhost/home/me/work",
    );
}

#[test]
fn format_session_line_appends_each_tag_hash_prefixed() {
    let s = SessionInfo {
        tags: vec!["agent".into(), "work".into()],
        ..session(24, 80)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  24x80  #agent  #work",
    );
}

#[test]
fn format_session_line_shows_foreground_program_bracketed() {
    let s = SessionInfo {
        title: Some("a title".into()),
        idle_seconds: Some(3),
        foreground: Some("claude".into()),
        ..session(24, 80)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  24x80  3s  [claude]  a title",
    );
}

#[test]
fn format_session_line_appends_notification_status() {
    // Body wins over title.
    let s = SessionInfo {
        last_notification: Some(felis_protocol::messages::SessionNotification {
            notification: felis_protocol::messages::Notification {
                title: Some("claude".into()),
                body: "task complete".into(),
                urgency: felis_protocol::messages::Urgency::Normal,
            },
            age_seconds: 5,
        }),
        ..session(24, 80)
    };
    assert_eq!(
        format_session_line(&s, "00000000"),
        "00000000  24x80  !task complete (5s)",
    );
}

#[test]
fn notification_snippet_collapses_newlines_and_truncates() {
    assert_eq!(notification_snippet("line1\nline2"), "line1 line2");
    let long = "x".repeat(50);
    let snip = notification_snippet(&long);
    assert_eq!(snip.chars().count(), 41); // 40 chars + ellipsis
    assert!(snip.ends_with('…'));
}

#[test]
fn format_session_line_treats_empty_optional_strings_as_absent() {
    let s = SessionInfo {
        title: Some(String::new()),
        cwd: Some("   ".into()),
        ..session(1, 1)
    };
    assert_eq!(format_session_line(&s, "00000000"), "00000000  1x1",);
}

#[test]
fn format_idle_age_picks_one_unit_per_magnitude() {
    assert_eq!(format_idle_age(0), "0s");
    assert_eq!(format_idle_age(59), "59s");
    assert_eq!(format_idle_age(60), "1m");
    assert_eq!(format_idle_age(60 * 60 - 1), "59m");
    assert_eq!(format_idle_age(60 * 60), "1h");
    assert_eq!(format_idle_age(60 * 60 * 24 - 1), "23h");
    assert_eq!(format_idle_age(60 * 60 * 24), "1d");
    assert_eq!(format_idle_age(60 * 60 * 24 * 7), "7d");
}
