use super::*;

/// REQ-114: a frame the receiver has no use for passes the arm table
/// before it is dropped, the correlation column included.
#[test]
fn a_drained_frame_whose_envelope_contradicts_its_arm_is_fatal() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let mislabeled = frame_correlated(
        &a_match(),
        Correlation::request(RequestId::new(1).expect("nonzero")),
    );
    let p = payload_of(&mut client, &mislabeled);
    assert!(matches!(
        client.admit_drained(&p),
        Err(DriverError::Correlation { .. })
    ));
}

/// Draining accounts: the item names a stream this client never
/// opened, and only the stream table can say so, so the drain is where
/// it is caught rather than a frame quietly thrown away.
#[test]
fn a_drained_item_for_a_stream_the_client_never_opened_is_fatal() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let item = frame_correlated(&a_match(), Correlation::stream(stream(7)));
    let p = payload_of(&mut client, &item);
    assert!(matches!(
        client.admit_drained(&p),
        Err(DriverError::Correlation { .. })
    ));
    assert_eq!(client.outstanding_streams(), 0);
}

/// A frame held back until a verb's reply arrives gets the same
/// judgment as one that is dropped, and nothing stateful with it: the
/// requeued frame is read again.
#[test]
fn a_parked_frame_is_judged_by_the_arm_table_without_being_accounted() {
    let ops = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Ops);
    assert!(matches!(
        ops.admit_parked(&frame(&PushMsg::Reattach { id: 1 })),
        Err(DriverError::ArmDenied {
            mode: ConnectionMode::Ops,
            ..
        })
    ));

    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let item = frame_correlated(&a_match(), Correlation::stream(stream(7)));
    assert!(client.admit_parked(&item).is_ok());
    assert_eq!(client.outstanding_streams(), 0);
    // The requeued frame is still the one the stream table sees.
    assert!(matches!(
        decode_frame::<SearchToClientMsg>(&mut client, &item),
        Err(DriverError::Correlation { .. })
    ));
}

/// A reader that discards a family still owes it the table. Every
/// client drains traffic it has no use for (a one-shot verb drops
/// another family's frame), and
/// matching on the kind to skip it would exempt exactly those frames
/// from the checks `decode` is the only place to run.
#[test]
fn a_drained_family_is_still_admitted_through_the_arm_table() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);

    let honest = frame(&GridMsg::RehydrateBegin);
    let p = payload_of(&mut client, &honest);
    client
        .admit_drained(&p)
        .expect("an uncorrelated push is what this family sends");

    let stamped = with_hand_written_ids(&GridMsg::RehydrateBegin, REQUEST_ID_1);
    let p = payload_of(&mut client, &stamped);
    assert!(
        matches!(
            client.admit_drained(&p).unwrap_err(),
            DriverError::Correlation { .. }
        ),
        "an envelope no Grid arm claims is unattributable, drained or not"
    );

    let malformed = with_field_100(&GridMsg::RehydrateBegin, MALFORMED_ENVELOPE);
    let p = payload_of(&mut client, &malformed);
    assert!(
        matches!(
            client.admit_drained(&p).unwrap_err(),
            DriverError::Correlation { .. }
        ),
        "the held peek fault is re-raised for a drained family too"
    );
}

/// The kind columns hold on the drain path too. `classify` refuses
/// this frame first on a live connection, so this is what keeps the
/// arm-table-free drain of a uniform family from being the one way
/// into the driver that skips the table.
#[test]
fn a_drained_family_the_mode_may_not_see_is_refused() {
    let mut observer = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Observer);
    let payload = Payload {
        kind: MessageKind::Grid,
        body: Bytes::from(codec::encode(&GridMsg::RehydrateBegin)),
        correlation: None,
        envelope_fault: None,
    };
    assert!(
        matches!(
            observer.admit_drained(&payload).unwrap_err(),
            DriverError::ModeDenied {
                kind: MessageKind::Grid,
                ..
            }
        ),
        "an observer never subscribes to a grid"
    );
}

/// A drained frame as the reader receives it: through `classify`, so
/// the envelope peek has run over the body the test wrote.
fn drained<R: Role>(driver: &mut ConnectionDriver<R>, kind: MessageKind, body: Vec<u8>) -> Payload {
    let frame = OwnedFrame {
        kind: kind.as_u16(),
        body: Bytes::from(body),
    };
    payload_of(driver, &frame)
}

/// The uniform families skip the arm-table walk on the drain path, not
/// the decode: a body that is not protobuf is corruption for the role
/// that drops grid traffic exactly as it is for the window client that
/// draws it. The peek already failed on these bytes, so this also
/// pins which of the two faults is reported.
#[test]
fn a_drained_uniform_body_that_is_not_protobuf_is_corruption() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);

    let honest = drained(
        &mut client,
        MessageKind::Grid,
        codec::encode(&GridMsg::RehydrateBegin),
    );
    client
        .admit_drained(&honest)
        .expect("a well-formed grid push is admitted");

    // A tag varint that never terminates.
    let garbage = drained(&mut client, MessageKind::Grid, vec![0xFF, 0xFF, 0xFF]);
    assert!(
        matches!(
            client.admit_drained(&garbage).unwrap_err(),
            DriverError::UndecodableBody {
                expected: MessageKind::Grid,
                ..
            }
        ),
        "a drained body that is not protobuf is corruption, not a correlation fault"
    );
}

/// An arm outside the schema is skipped by prost as an unknown field
/// and leaves the family's oneof absent, which the conversion refuses.
#[test]
fn a_drained_uniform_body_naming_no_known_arm_is_corruption() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    // Field 4095, wire type 2, zero-length: no `GridMsg` arm claims it.
    let unknown_arm = drained(&mut client, MessageKind::Grid, vec![0xFA, 0xFF, 0x01, 0x00]);
    let err = client.admit_drained(&unknown_arm).unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::UndecodableBody { expected: MessageKind::Grid, found }
                if found.contains("GridMsg.msg")
        ),
        "an unknown arm left the oneof absent, which is corruption: {err}"
    );
}

/// The stateless limits `felis-protocol` can check run on the drain
/// path too: an image header claiming more pixels than the per-image
/// cap is an allocation a consumer would have to refuse anyway.
#[test]
fn a_drained_uniform_body_over_a_stateless_limit_is_corruption() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let oversized = ImageMsg::Header {
        id: ImageId(1),
        target: ImageTarget::New {
            width: u32::MAX,
            height: u32::MAX,
            format: ImageFormat::Rgba32,
            serial: None,
        },
    };
    let payload = drained(&mut client, MessageKind::Image, codec::encode(&oversized));
    let err = client.admit_drained(&payload).unwrap_err();
    assert!(
        matches!(
            &err,
            DriverError::UndecodableBody { expected: MessageKind::Image, found }
                if found.contains("ImageNew.pixels")
        ),
        "an over-cap image claim is corruption on the drain path: {err}"
    );
}

/// The boundary the drain path cannot cross. `packed_cells` is opaque
/// to `felis-protocol`, so a row holding bytes no row codec produced
/// is a well-formed frame at the wire tier; only the shadow that
/// unpacks it, against the row codec in force, can call it malformed
/// (`docs/reference/ipc.md` "Corruption").
#[test]
fn a_drained_uniform_body_with_an_opaque_garbage_row_is_admitted() {
    let mut client = in_phase::<ClientSide>(Phase::Attached, ConnectionMode::Window);
    let garbage_row = GridMsg::RowDelta {
        rows: vec![(0, RowPayload(vec![0xFF; 16]))],
    };
    let payload = drained(&mut client, MessageKind::Grid, codec::encode(&garbage_row));
    client
        .admit_drained(&payload)
        .expect("row-codec contents are a consumer-tier invariant, not a wire-tier one");
}
