use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_pty_output_through_the_grid_to_the_wire() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("printf hello-felis; sleep 0.5");

    spawn_daemon(&path, pool, factory);

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    hello_welcome(&mut reader, &mut writer, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    expect_row_containing(&mut reader, "hello-felis").await;
}

/// Text written after a placement in the same write lands where kitty
/// moved the cursor: right of the image, on its last row.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_after_a_placement_in_one_write_lands_right_of_the_image() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory(
        "printf 'top\\n\\033_Ga=T,f=24,s=1,v=1,c=2,r=2;AAAA\\033\\\\AFTER'; sleep 0.5",
    );
    spawn_daemon(&path, pool, factory);
    let (mut reader, mut writer) = dial(&path, ConnectionMode::Window, false).await;
    create_and_attach(&mut reader, &mut writer).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let (row, col) = loop {
        assert!(tokio::time::Instant::now() < deadline, "AFTER never landed");
        let frame = tokio::time::timeout(Duration::from_secs(5), reader.next_frame())
            .await
            .expect("read timed out")
            .unwrap()
            .expect("frame");
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        if let GridMsg::RowDelta { rows } = codec::decode::<GridMsg>(&frame.body).unwrap()
            && let Some(hit) = rows
                .iter()
                .find_map(|(i, body)| row_text(&body.0).find("AFTER").map(|c| (*i, c)))
        {
            break hit;
        }
    };
    assert_eq!((row, col), (2, 2));
}

/// The daemon hands region bytes back rather than spawning the argv: the
/// pipe target belongs to the requester's host.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_region_request_ships_the_bytes_back_and_spawns_nothing() {
    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let probe = Arc::clone(&pool);
    let factory = shell_factory("read _x");

    spawn_daemon(&path, pool, factory);

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    hello_welcome(&mut reader, &mut writer, false).await;
    let _host = create_and_attach(&mut reader, &mut writer).await;

    send_request(
        &mut writer,
        &RegionToDaemonMsg::Request {
            source: felis_protocol::messages::RegionSource::Visible,
            ansi: false,
            session: None,
        },
        1,
    )
    .await;

    loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), reader.next_frame())
            .await
            .expect("the region reply should arrive")
            .unwrap()
            .expect("frame");
        if frame.kind != MessageKind::Region.as_u16() {
            continue;
        }
        if let RegionToClientMsg::Reply { .. } = codec::decode(&frame.body).unwrap() {
            break;
        }
    }
    assert_eq!(
        probe.lock().await.len(),
        1,
        "the daemon must not spawn a transient for a client-side sink",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resize_over_the_wire_makes_subsequent_row_deltas_carry_the_new_width() {
    use felis_grid::decode_row;

    const NEW_COLS: u16 = 120;

    let tmp = private_dir();
    let path = tmp.path().join("daemon.sock");
    let pool = Arc::new(Mutex::new(SessionPool::new()));
    let factory = shell_factory("read x; printf 'got=%s\\n' \"$x\"");

    spawn_daemon(&path, pool, factory);

    let (read_half, write_half) = connect(&path).await.unwrap();
    let (mut reader, mut writer) = framed(read_half, write_half).await;

    hello_welcome(&mut reader, &mut writer, false).await;

    create_and_attach(&mut reader, &mut writer).await;

    send_input(
        &mut writer,
        &InputMsg::Resize {
            dims: felis_protocol::messages::RequestedDims {
                rows: 30,
                cols: u32::from(NEW_COLS),
                pixel_w: 0,
                pixel_h: 0,
            },
        },
    )
    .await;

    send_input(&mut writer, &InputMsg::KeyBytes(b"HELLO\n".to_vec())).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut saw_post_resize_row = false;
    while tokio::time::Instant::now() < deadline {
        let frame = match tokio::time::timeout(Duration::from_secs(5), reader.next_frame()).await {
            Ok(Ok(Some(f))) => f,
            Ok(Err(e)) => panic!("read: {e:?}"),
            Ok(Ok(None)) | Err(_) => break,
        };
        if frame.kind != MessageKind::Grid.as_u16() {
            continue;
        }
        let msg: GridMsg = codec::decode(&frame.body).unwrap();
        if let GridMsg::RowDelta { rows } = msg {
            for (_, packed_cells) in &rows {
                let decoded =
                    decode_row(&packed_cells.0, &mut felis_grid::StyleTable::new()).unwrap();
                let text = row_text(&packed_cells.0);
                if text.contains("got=HELLO") {
                    assert_eq!(
                        decoded.cells.len(),
                        usize::from(NEW_COLS),
                        "post-resize row must be {NEW_COLS} cells wide; \
                         got {} (text: {text:?})",
                        decoded.cells.len(),
                    );
                    saw_post_resize_row = true;
                    break;
                }
            }
            if saw_post_resize_row {
                break;
            }
        }
    }
    assert!(
        saw_post_resize_row,
        "never saw the post-read printf land on a row"
    );
}
