//! The CLI opens streams through `felis-client-core`'s helpers
//! (`reference/ipc.md` "Correlation, requests, and streams"): a
//! hand-rolled sequence (allocate an id, then stamp
//! `Correlation::stream` on the opener) leaves an id issued for a frame
//! that never goes out, which the daemon answers by closing.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// The needles are the two halves of the raw sequence: the allocation
/// and the envelope the opener travels under.
const RAW_SEQUENCE: &[&str] = &["driver.open_stream()", "Correlation::stream("];

#[test]
fn the_cli_never_hand_rolls_a_stream_open() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();
    let mut scanned = 0usize;

    for entry in std::fs::read_dir(&src).expect("felis-cli has a src directory") {
        let path = entry.expect("readable directory entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        scanned += 1;
        let text = std::fs::read_to_string(&path).expect("readable source file");
        for (number, line) in text.lines().enumerate() {
            for needle in RAW_SEQUENCE {
                if line.contains(needle) {
                    offenders.push(format!(
                        "{}:{}: {}",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }

    assert!(scanned > 0, "no felis-cli sources were scanned");
    assert!(
        offenders.is_empty(),
        "open the stream through felis_client_core (`Connection::open_stream`, \
         `Connection::subscribe_notifications`, or `open_stream` + `begin_stream`) \
         instead of:\n{}",
        offenders.join("\n"),
    );
}
