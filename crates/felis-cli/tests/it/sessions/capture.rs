use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_renders_visible_grid_rows() {
    let tmp = private_dir();
    let factory = sh_factory("printf 'CAPTURE-LINE-ONE\\r\\nCAPTURE-LINE-TWO\\r\\n'; read x");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(
        &socket,
        &["sessions", "capture", &id_hex],
        "CAPTURE-LINE-TWO",
    )
    .await;

    let out = felis_out(&socket, &["sessions", "capture", &id_hex]);
    assert!(
        out.status.success(),
        "capture must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CAPTURE-LINE-ONE"),
        "capture output missing line 1: {stdout}",
    );
    assert!(
        stdout.contains("CAPTURE-LINE-TWO"),
        "capture output missing line 2: {stdout}",
    );
    let line_count = stdout.lines().count();
    assert!(
        line_count >= 24,
        "capture must emit one line per grid row (≥24); got {line_count}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_scrollback_includes_offscreen_history() {
    let tmp = private_dir();
    // 50 lines overflow the 24-row grid into scrollback.
    let factory = sh_factory(
        "i=1; while [ $i -le 50 ]; do printf 'SB-LINE-%03d\\r\\n' $i; i=$((i+1)); done; read x",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(&socket, &["sessions", "capture", &id_hex], "SB-LINE-050").await;

    let plain = felis_out(&socket, &["sessions", "capture", &id_hex]);
    assert!(plain.status.success(), "plain capture must succeed");
    let plain_out = String::from_utf8_lossy(&plain.stdout);
    assert!(
        !plain_out.contains("SB-LINE-001"),
        "line 1 should have scrolled off the visible grid: {plain_out}",
    );

    let scrolled = felis_out(
        &socket,
        &["sessions", "capture", &id_hex, "--source", "scrollback"],
    );
    assert!(
        scrolled.status.success(),
        "scrollback capture must succeed: status={:?} stderr={}",
        scrolled.status,
        String::from_utf8_lossy(&scrolled.stderr),
    );
    let scrolled_out = String::from_utf8_lossy(&scrolled.stdout);
    assert!(
        scrolled_out.contains("SB-LINE-001"),
        "scrollback capture missing the off-screen first line: {scrolled_out}",
    );
    assert!(
        scrolled_out.contains("SB-LINE-050"),
        "scrollback capture missing the last live line: {scrolled_out}",
    );
    let pos1 = scrolled_out.find("SB-LINE-001").unwrap();
    let pos50 = scrolled_out.find("SB-LINE-050").unwrap();
    assert!(pos1 < pos50, "scrollback must print oldest-first");

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_lines_tails_the_region_and_keeps_row_indices() {
    let tmp = private_dir();
    let factory = sh_factory(
        "i=1; while [ $i -le 50 ]; do printf 'SB-LINE-%03d\\r\\n' $i; i=$((i+1)); done; read x",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(&socket, &["sessions", "capture", &id_hex], "SB-LINE-050").await;

    let capture_json = |extra: Vec<&str>| {
        let mut args = vec!["sessions", "capture", &id_hex, "--format", "jsonl"];
        args.extend(extra);
        let out = cli_on(&socket)
            .args(&args)
            .output()
            .expect("run felis sessions capture --lines");
        assert!(
            out.status.success(),
            "capture --lines must succeed: status={:?} stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // A tail inside the visible grid keeps live indices: a filter, not
    // a renumbering.
    let tail5 = capture_json(vec!["--source", "scrollback", "--lines", "5"]);
    let rows5 = tail5.lines().filter(|l| l.contains("\"text\"")).count();
    assert_eq!(rows5, 5, "tail 5 must emit 5 rows: {tail5}");
    for r in 19..=23 {
        assert!(
            tail5.contains(&format!("\"row\":{r},")),
            "tail 5 must keep live index {r}: {tail5}",
        );
    }
    assert!(
        tail5.contains("\"count\":5"),
        "terminator must count the tail, not the region: {tail5}",
    );

    // 30 rows = 24 live + 6 scrollback.
    let tail30 = capture_json(vec!["--source", "scrollback", "--lines", "30"]);
    let rows30 = tail30.lines().filter(|l| l.contains("\"text\"")).count();
    assert_eq!(rows30, 30, "tail 30 must emit 30 rows: {tail30}");
    assert!(
        tail30.contains("\"row\":-1,") && tail30.contains("\"row\":-6,"),
        "tail 30 must keep the negative scrollback indices: {tail30}",
    );
    assert!(
        !tail30.contains("\"row\":-7,"),
        "tail 30 must not ship scrollback beyond the cut: {tail30}",
    );

    let visible3 = capture_json(vec!["--lines", "3"]);
    let rows3 = visible3.lines().filter(|l| l.contains("\"text\"")).count();
    assert_eq!(rows3, 3, "visible tail must emit 3 rows: {visible3}");
    assert!(
        visible3.contains("\"row\":23,"),
        "visible tail must end at the bottom grid row: {visible3}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_ansi_reconstructs_sgr_color() {
    let tmp = private_dir();
    let factory = sh_factory("printf '\\033[31mANSI-RED-LINE\\033[0m\\r\\n'; read x");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(&socket, &["sessions", "capture", &id_hex], "ANSI-RED-LINE").await;

    let ansi = felis_out(&socket, &["sessions", "capture", &id_hex, "--ansi"]);
    assert!(
        ansi.status.success(),
        "ansi capture must succeed: status={:?} stderr={}",
        ansi.status,
        String::from_utf8_lossy(&ansi.stderr),
    );
    let ansi_out = String::from_utf8_lossy(&ansi.stdout);
    assert!(
        ansi_out.contains("ANSI-RED-LINE"),
        "ansi capture missing the banner text: {ansi_out:?}",
    );
    assert!(
        ansi_out.contains("\x1b[31m"),
        "ansi capture missing the red SGR run: {ansi_out:?}",
    );

    let plain = felis_out(&socket, &["sessions", "capture", &id_hex]);
    let plain_out = String::from_utf8_lossy(&plain.stdout);
    assert!(
        plain_out.contains("ANSI-RED-LINE"),
        "plain capture missing the banner text: {plain_out:?}",
    );
    assert!(
        !plain_out.contains('\x1b'),
        "plain capture must carry no escape bytes: {plain_out:?}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_ansi_json_carries_both_forms_per_row() {
    let tmp = private_dir();
    let factory = sh_factory("printf '\\033[31mANSI-RED-LINE\\033[0m\\r\\n'; read x");
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(&socket, &["sessions", "capture", &id_hex], "ANSI-RED-LINE").await;

    let out = felis_out(
        &socket,
        &[
            "sessions", "capture", &id_hex, "--ansi", "--format", "jsonl",
        ],
    );
    assert!(
        out.status.success(),
        "capture --ansi --format jsonl must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let banner = stdout
        .lines()
        .find(|l| l.contains("ANSI-RED-LINE"))
        .unwrap_or_else(|| panic!("no row carried the banner: {stdout}"));
    // ESC is JSON-escaped as `\u001b`.
    let (text_half, ansi_half) = banner
        .split_once("\"ansi\":")
        .unwrap_or_else(|| panic!("row carries no ansi field: {banner}"));
    assert!(
        !text_half.contains("\\u001b"),
        "the text field must stay plain: {banner}",
    );
    assert!(
        ansi_half.contains("\\u001b[31m"),
        "the ansi field must carry the red SGR run: {banner}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_ansi_json_composes_on_a_mark_range_source() {
    let tmp = private_dir();
    let factory = sh_factory(
        "printf '\\033]133;A\\007$ \\033]133;B\\007echo hi\\r\\n\
             \\033]133;C\\007\\033[31mRED-133\\033[0m\\r\\n\\033]133;D;0\\007'; read x",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(
        &socket,
        &["sessions", "capture", &id_hex, "--source", "command-output"],
        "RED-133",
    )
    .await;

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "capture",
            &id_hex,
            "--ansi",
            "--format",
            "jsonl",
            "--source",
            "command-output",
        ],
    );
    assert!(
        out.status.success(),
        "the pair must succeed on a mark-range source: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"text\":\"RED-133\""),
        "plain text field must stay escape-free: {stdout:?}",
    );
    assert!(
        stdout.contains("\"ansi\":\"\\u001b["),
        "the ansi field must carry the SGR reconstruction: {stdout:?}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_source_command_output_returns_the_c_to_d_range() {
    let tmp = private_dir();
    let factory = sh_factory(
        "printf '\\033]133;A\\007$ \\033]133;B\\007echo hi\\r\\n\
             \\033]133;C\\007CMDOUT-133\\r\\n\\033]133;D;0\\007'; read x",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(
        &socket,
        &["sessions", "capture", &id_hex, "--source", "command-output"],
        "CMDOUT-133",
    )
    .await;

    let out = felis_out(
        &socket,
        &["sessions", "capture", &id_hex, "--source", "command-output"],
    );
    assert!(
        out.status.success(),
        "capture --source command-output must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("CMDOUT-133"),
        "command-output must contain the C→D output: {stdout:?}",
    );
    assert!(
        !stdout.contains("echo hi"),
        "command-output must exclude the command line: {stdout:?}",
    );

    let info = felis_out(&socket, &["sessions", "info", &id_hex, "--format", "json"]);
    let info_out = String::from_utf8_lossy(&info.stdout);
    assert!(
        info_out.contains("\"last_exit_code\":0"),
        "info --format json must surface the D mark's exit code: {info_out}",
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capture_json_semantic_source_emits_region_relative_rows() {
    let tmp = private_dir();
    let factory = sh_factory(
        "printf '\\033]133;A\\007$ \\033]133;B\\007echo hi\\r\\n\
             \\033]133;C\\007CMDOUT-133\\r\\n\\033]133;D;0\\007'; read x",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(
        &socket,
        &["sessions", "capture", &id_hex, "--source", "last-command"],
        "CMDOUT-133",
    )
    .await;

    let out = felis_out(
        &socket,
        &[
            "sessions",
            "capture",
            &id_hex,
            "--source",
            "last-command",
            "--format",
            "jsonl",
        ],
    );
    assert!(
        out.status.success(),
        "capture --source last-command --format jsonl must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"row\":0"),
        "semantic jsonl must start at region-relative row 0: {stdout:?}",
    );
    assert!(
        stdout.contains("CMDOUT-133"),
        "semantic jsonl must carry the command output text: {stdout:?}",
    );
    assert!(
        stdout.contains("\"soft_wrap_continued\":false"),
        "stitched region rows are never continuations: {stdout:?}",
    );
    let done = stdout.lines().last().expect("at least the terminal line");
    assert!(
        done.starts_with("{\"v\":1,\"event\":\"end\",\"count\":") && !done.contains("\"count\":0"),
        "semantic jsonl must close with a non-empty end terminal: {done:?}",
    );
    assert!(
        done.contains("\"exit_code\":0"),
        "the terminal must carry the D;0 mark's exit code: {done:?}",
    );

    server.abort();
}
