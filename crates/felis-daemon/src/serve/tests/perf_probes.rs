use super::*;

/// Measurement (ignored): attached end-to-end drain of large input files.
///
/// Subscriber pull-paces at ~60 Hz like the GUI client to measure throughput.
/// Run with `FELIS_BENCH_FILE=/path` and `--run-ignored all --no-capture`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; needs FELIS_BENCH_FILE, run with --run-ignored all"]
async fn measure_attached_cat_drain() {
    let file = std::env::var("FELIS_BENCH_FILE")
        .expect("set FELIS_BENCH_FILE to a large ASCII file (see this test's doc comment)");
    let bytes = std::fs::metadata(&file)
        .unwrap_or_else(|e| panic!("stat {file}: {e}"))
        .len();

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = {
        let file = file.clone();
        Arc::new(move |_| {
            // `sh -c 'exec cat …'` rather than `/bin/cat`: a store-only
            // host has no `/bin/cat`, and `exec` leaves the same single
            // `cat` on the PTY.
            sh(&format!("exec cat '{file}'"), fixture_path())
        })
    };
    spawn_daemon(&path, pool.clone(), factory);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, true).await;

    // The payload's capture geometry, not the daemon default: replayed at
    // the wrong grid the stream becomes a wrap+scroll storm. Absent asks
    // for the daemon default.
    let env_axis = |key| {
        std::env::var(key)
            .ok()
            .and_then(|v: String| v.parse::<u32>().ok())
    };
    let dims = env_axis("FELIS_BENCH_ROWS")
        .zip(env_axis("FELIS_BENCH_COLS"))
        .map(|(rows, cols)| felis_protocol::messages::RequestedDims {
            rows,
            cols,
            pixel_w: 0,
            pixel_h: 0,
        });
    let _id = create_and_attach_with_dims(&mut reader, &mut writer, dims).await;
    let start = Instant::now();

    let pull = tokio::spawn(async move {
        loop {
            send_input(&mut writer, &InputMsg::NextGridFrame).await;
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
    });

    let deadline = start + Duration::from_secs(120);
    let mut frames: u64 = 0;
    loop {
        let framed = tokio::time::timeout(Duration::from_secs(10), reader.next_frame())
            .await
            .expect("drain stalled: no frame for 10s");
        match framed {
            Ok(Some(_)) => frames += 1,
            Ok(None) | Err(_) => break,
        }
        assert!(Instant::now() < deadline, "drain never completed");
    }
    let secs = start.elapsed().as_secs_f64();
    pull.abort();
    let mib = bytes as f64 / (1024.0 * 1024.0);
    eprintln!(
        "attached cat drain (pull-paced): {mib:.0} MiB in {secs:.3}s = {:.1} MiB/s \
         ({frames} wire frames)",
        mib / secs,
    );
}

/// Measurement (ignored, timing-dependent): pull-paced cycles a window gets
/// while the child floods visible cells; a `ParseCore` lock that never hands
/// off collapses this to a handful (`docs/explanation/rendering/pipeline.md`).
/// Run with `cargo nextest run --cargo-profile release -p felis-daemon
/// --run-ignored all --no-capture measure_pull_cycles_under_flood`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "measurement; timing-dependent, run with --run-ignored all"]
async fn measure_pull_cycles_under_flood() {
    use std::fmt::Write as _;

    const WINDOW: Duration = Duration::from_secs(10);

    let tmp = private_dir();
    let frames_file = tmp.path().join("frames");
    let passes_file = tmp.path().join("passes");
    // DOOM-fire's shape: every frame repaints every cell with its own
    // truecolor pair, so the parse thread is never idle waiting on the
    // PTY and each pull finds real damage.
    let mut frames = String::new();
    for frame in 0..64_u32 {
        frames.push_str("\x1b[H");
        for row in 0..24_u32 {
            for col in 0..80_u32 {
                let heat = (frame * 7 + row * 5 + col * 3) % 256;
                write!(
                    frames,
                    "\x1b[38;2;{heat};{};0m\x1b[48;2;0;0;{heat}m\u{2580}",
                    heat / 2
                )
                .unwrap();
            }
            if row < 23 {
                frames.push_str("\r\n");
            }
        }
    }
    std::fs::write(&frames_file, frames).unwrap();

    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory: SessionFactory = {
        let script = format!(
            "i=0; while :; do cat '{f}'; i=$((i+1)); echo $i > '{p}.tmp'; mv '{p}.tmp' '{p}'; done",
            f = frames_file.display(),
            p = passes_file.display(),
        );
        Arc::new(move |_| sh(&script, fixture_path()))
    };
    spawn_daemon(&path, pool.clone(), factory);

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, true).await;
    let _id = create_and_attach(&mut reader, &mut writer).await;

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), reader.next_frame())
            .await
            .expect("rehydrate stalled")
            .unwrap()
            .expect("frame");
        if frame.kind == MessageKind::Grid.as_u16()
            && matches!(
                codec::decode::<GridMsg>(&frame.body).unwrap(),
                GridMsg::RehydrateEnd
            )
        {
            break;
        }
    }

    let passes = || {
        std::fs::read_to_string(&passes_file)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let passes_at_start = passes();
    send_input(&mut writer, &InputMsg::NextGridFrame).await;
    let deadline = tokio::time::Instant::now() + WINDOW;
    let mut cycles: u64 = 0;
    while let Ok(framed) = tokio::time::timeout_at(deadline, reader.next_frame()).await {
        let frame = framed.unwrap().expect("daemon closed the stream");
        if frame.kind == MessageKind::Grid.as_u16()
            && matches!(
                codec::decode::<GridMsg>(&frame.body).unwrap(),
                GridMsg::CycleEnd
            )
        {
            cycles += 1;
            send_input(&mut writer, &InputMsg::NextGridFrame).await;
        }
    }
    let passes_at_end = passes();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        passes() > passes_at_end,
        "the child stopped flooding, so the window measured no contention",
    );
    eprintln!(
        "pull cycles under flood: {cycles} in {}s ({} frame-file passes)",
        WINDOW.as_secs(),
        passes_at_end.saturating_sub(passes_at_start),
    );
}

/// Cross-host latency probe (docs/reference/ipc.md
/// "Cross-host carrier: SSH stdio"):
/// byte positions of the first `RowDelta` and first `ImageMsg::Header`,
/// projected to wall-clock under a 100 ms RTT, 10 Mbps model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn time_to_first_frame_under_simulated_slow_link() {
    use std::num::NonZeroU16;

    use felis_grid::images::{
        CellPos, Extent, ImageEntry, ImageFormat, ImageId, Placement, PlacementId,
    };
    use felis_protocol::messages::ImageMsg;

    // The exact numbers are not load-bearing: the bound holds for any RTT ≥
    // 50 ms and bandwidth ≥ 1 Mbps.
    const RTT_MS: u64 = 100;
    const BANDWIDTH_BPS: u64 = 10_000_000;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read _x");

    spawn_daemon(&path, pool.clone(), factory);

    // 4 KiB of pixels: without them both landmarks land in one MTU and the
    // gap is framing overhead.
    let mut owned = owned_session("read _x");
    owned
        .images
        .insert(
            ImageId(7),
            ImageEntry::new(32, 32, ImageFormat::Rgba32, vec![0xAB; 32 * 32 * 4]),
        )
        .expect("insert under cap");
    owned.placements.upsert(Placement {
        image_id: ImageId(7),
        placement_id: Some(PlacementId(1)),
        anchor: CellPos { row: 1, col: 1 },
        cols: Extent::Requested(NonZeroU16::new(4).unwrap()),
        rows: Extent::Requested(NonZeroU16::new(4).unwrap()),
        source: None,
        z_index: 0,
        no_cursor_move: false,
        quiet: 0,
        anonymous: false,
        serial: 0,
    });
    let session_task::SessionLifecycle { id: session_id, .. } = session_task::spawn_owned(
        &pool,
        owned,
        IdlePolicy::default(),
        SessionId::new(),
        0,
        0,
        Vec::new(),
        None,
        Listing::Public,
    )
    .await;

    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    send_kind(
        &mut writer,
        &SessionToDaemonMsg::Attach {
            target: AttachTarget::Id(session_id.0),
            live_only: false,
            image_regions: false,
        },
    )
    .await;
    let _ack = reader.next_frame().await.unwrap().expect("ack");

    let mut bytes_on_wire: usize = 0;
    let mut bytes_to_first_row_delta: Option<usize> = None;
    let mut bytes_to_first_image_header: Option<usize> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "rehydrate burst did not end within deadline"
        );
        let frame = tokio::time::timeout(Duration::from_secs(2), reader.next_frame())
            .await
            .expect("read timed out")
            .unwrap()
            .expect("frame");
        bytes_on_wire += felis_protocol::frame::HEADER_LEN + frame.body.len();

        if frame.kind == MessageKind::Grid.as_u16() {
            let msg: GridMsg = codec::decode(&frame.body).unwrap();
            if matches!(msg, GridMsg::RowDelta { .. }) && bytes_to_first_row_delta.is_none() {
                bytes_to_first_row_delta = Some(bytes_on_wire);
            }
            if matches!(msg, GridMsg::RehydrateEnd) {
                break;
            }
        } else if frame.kind == MessageKind::Image.as_u16() {
            let msg: ImageMsg = codec::decode(&frame.body).unwrap();
            if matches!(msg, ImageMsg::Header { .. }) && bytes_to_first_image_header.is_none() {
                bytes_to_first_image_header = Some(bytes_on_wire);
            }
        }
    }

    let cells_bytes = bytes_to_first_row_delta.expect("at least one RowDelta");
    let image_bytes = bytes_to_first_image_header.expect("at least one ImageMsg::Header");

    assert!(
        cells_bytes < image_bytes,
        "cells must precede images on the wire (got cells_bytes={cells_bytes}, \
         image_bytes={image_bytes})",
    );

    // One RTT for the handshake + bytes / bandwidth; constant overhead
    // cancels between landmarks.
    let bytes_per_ms = (BANDWIDTH_BPS / 8 / 1000) as f64;
    let ms_to_first_cell = RTT_MS as f64 + cells_bytes as f64 / bytes_per_ms;
    let ms_to_first_image = RTT_MS as f64 + image_bytes as f64 / bytes_per_ms;

    eprintln!(
        "cross-host latency probe (RTT={RTT_MS} ms, BW={BANDWIDTH_BPS} bps):\n  \
         bytes_to_first_RowDelta:    {cells_bytes:>6} → {ms_to_first_cell:>6.2} ms\n  \
         bytes_to_first_ImageHeader: {image_bytes:>6} → {ms_to_first_image:>6.2} ms\n  \
         gap (cells-first benefit):   {:>6} bytes / {:>5.2} ms",
        image_bytes - cells_bytes,
        ms_to_first_image - ms_to_first_cell,
    );

    assert!(
        ms_to_first_cell <= 1.5 * RTT_MS as f64,
        "time-to-first-cell {ms_to_first_cell:.2} ms exceeds 1.5×RTT \
         ({:.2} ms) under the slow-link model",
        1.5 * RTT_MS as f64,
    );
    // The RTT bound admits ~62 KB before the first cell; bound the bytes
    // too.
    assert!(
        cells_bytes <= 16 * 1024,
        "{cells_bytes} bytes on the wire before the first RowDelta — \
         the cells-first preamble should be a few KB",
    );
}
