use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_returns_hits_and_no_match_exits_one() {
    let tmp = private_dir();
    let factory = sh_factory(
        "i=0; while [ $i -lt 50 ]; do printf 'entry-%s-PROBE\\r\\n' $i; i=$((i+1)); done; read x",
    );
    let (server, socket, _pool) = spawn_daemon(&tmp, factory);
    let id = create_and_detach(&socket).await;
    let id_hex = format!("{id:032x}");

    poll_until(&socket, &["sessions", "capture", &id_hex], "entry-49-PROBE").await;

    let out = felis_out(&socket, &["sessions", "search", &id_hex, "PROBE"]);
    assert!(
        out.status.success(),
        "search must succeed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let hits = stdout.lines().count();
    assert!(
        hits > 0,
        "expected at least one PROBE hit; stdout:\n{stdout}",
    );
    assert!(
        stdout.contains("PROBE"),
        "stdout must echo the matched text; got: {stdout}",
    );

    let out_miss = felis_out(&socket, &["sessions", "search", &id_hex, "XYZ-NOPE-NEVER"]);
    assert_eq!(
        out_miss.status.code(),
        Some(1),
        "no-match must exit 1: status={:?} stdout={} stderr={}",
        out_miss.status,
        String::from_utf8_lossy(&out_miss.stdout),
        String::from_utf8_lossy(&out_miss.stderr),
    );

    let out_json = felis_out(
        &socket,
        &["sessions", "search", &id_hex, "PROBE", "--format", "jsonl"],
    );
    let json_out = String::from_utf8_lossy(&out_json.stdout);
    let objects = parse_jsonl(&json_out);
    assert_all_versioned(&objects, "sessions search");
    assert!(
        json_out.contains("\"line_index\":"),
        "jsonl output missing line_index: {json_out}",
    );
    let terminal = objects.last().expect("at least the terminal object");
    assert_eq!(
        terminal["event"], "end",
        "jsonl output missing the end terminal: {json_out}",
    );

    server.abort();
}
