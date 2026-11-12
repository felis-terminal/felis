use super::*;

#[test]
fn scan_printable_run_matches_scalar() {
    // The byte-at-a-time reference the SWAR path must match.
    fn scalar(buf: &[u8]) -> usize {
        buf.iter()
            .position(|&b| !(0x20..=0x7E).contains(&b))
            .unwrap_or(buf.len())
    }
    // Every byte value at each offset 0..16 of a printable-filled window
    // covers the SWAR body, the chunk boundary, and the scalar tail. The
    // two-byte borrow interaction is left to the Kani proof.
    for value in 0u16..=255 {
        let byte = value as u8;
        for pos in 0..16 {
            let mut buf = [b'x'; 16];
            buf[pos] = byte;
            assert_eq!(
                scan_printable_run(&buf),
                scalar(&buf),
                "byte {byte:#04x} at offset {pos}"
            );
        }
    }
    // Tail-only path.
    for len in 0..8 {
        let buf = vec![b'A'; len];
        assert_eq!(
            scan_printable_run(&buf),
            scalar(&buf),
            "all-printable len {len}"
        );
    }
}

#[test]
fn scan_mixed_print_run_matches_scalar() {
    // The per-byte control-or-DEL reference the SWAR path must match.
    fn scalar(buf: &[u8]) -> usize {
        buf.iter()
            .position(|&b| b < 0x20 || b == 0x7F)
            .unwrap_or(buf.len())
    }
    // High-byte background (0xE4 is a UTF-8 continuation byte) with one
    // byte of every value at each offset into a second chunk.
    for value in 0u16..=255 {
        let byte = value as u8;
        for pos in 0..16 {
            let mut buf = [0xE4u8; 16];
            buf[pos] = byte;
            assert_eq!(
                scan_mixed_print_run(&buf),
                scalar(&buf),
                "byte {byte:#04x} at offset {pos}"
            );
        }
    }
    // The scalar-tail loop, where the 0x1F/0x20 and 0x7E/0x7F/0x80 edges
    // separate `<` from `<=`.
    for value in 0u16..=255 {
        let byte = value as u8;
        for pos in 0..7 {
            let mut buf = [0xE4u8; 7];
            buf[pos] = byte;
            assert_eq!(
                scan_mixed_print_run(&buf),
                scalar(&buf),
                "tail byte {byte:#04x} at offset {pos}"
            );
        }
    }
    // The run length must equal the buffer length, not collapse to 0.
    for len in 0..16 {
        let buf: Vec<u8> = (0..len)
            .map(|i| if i % 2 == 0 { 0xE4u8 } else { b' ' })
            .collect();
        assert_eq!(scan_mixed_print_run(&buf), len, "interleaved len {len}");
    }
}

#[test]
fn scan_string_body_matches_scalar() {
    // The per-byte special-byte reference the SWAR path must match.
    fn scalar(buf: &[u8]) -> usize {
        buf.iter()
            .position(|&b| matches!(b, 0x07 | 0x18 | 0x1A | 0x1B))
            .unwrap_or(buf.len())
    }
    // Same single-offending-byte sweep as `scan_printable_run_matches_scalar`.
    for value in 0u16..=255 {
        let byte = value as u8;
        for pos in 0..16 {
            let mut buf = [b'x'; 16];
            buf[pos] = byte;
            assert_eq!(
                scan_string_body(&buf),
                scalar(&buf),
                "byte {byte:#04x} at offset {pos}"
            );
        }
    }
    for len in 0..8 {
        let buf = vec![0xE4u8; len];
        assert_eq!(scan_string_body(&buf), scalar(&buf), "all-body len {len}");
    }
}

#[test]
fn csi_param_chunked_advance_matches_per_byte_dispatch() {
    // `advance` over a CSI-dense stream must produce the same dispatches
    // (params, subparam mask, overflow flag) as `advance_byte` at every
    // chunk boundary. The stream mixes multi-digit 256-color SGR,
    // `:`-subparams, an empty leading param, a >16-param overflow, a C0
    // executed mid-sequence, and a CAN cancellation.
    let mut seq: Vec<u8> = Vec::new();
    seq.extend_from_slice(b"\x1b[38;5;196m\x1b[48;5;21mx");
    seq.extend_from_slice(b"\x1b[4:3m\x1b[58:2::255:128:0m");
    seq.extend_from_slice(b"\x1b[;5H");
    seq.extend_from_slice(b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17m");
    seq.extend_from_slice(b"\x1b[3\x0812m"); // BS executes inside CsiParam
    seq.extend_from_slice(b"\x1b[99\x18z"); // CAN cancels; `z` prints

    let mut per_byte = LogSink::default();
    let mut parser = Parser::new();
    for &b in &seq {
        parser.advance_byte(&mut per_byte, b);
    }
    for split in 0..=seq.len() {
        let mut bulk = LogSink::default();
        let mut parser = Parser::new();
        parser.advance(&mut bulk, &seq[..split]);
        parser.advance(&mut bulk, &seq[split..]);
        assert_eq!(bulk.events, per_byte.events, "split at {split}");
    }
}

#[test]
fn osc_body_bulk_path_matches_per_byte_dispatch() {
    // The bulk `OscBuffer::extend` path must produce the same dispatch as
    // feeding the same OSC byte by byte, including a body holding C0
    // bytes outside the special set.
    let mut body = vec![b'6', b';'];
    body.extend(std::iter::repeat_n(b'a', 100));
    body.push(0x0A); // non-special C0 stays payload
    body.extend(std::iter::repeat_n(0xE4, 7)); // high bytes stay payload
    let mut seq = vec![0x1B, b']'];
    seq.extend_from_slice(&body);
    seq.push(0x07);

    let mut bulk = LogSink::default();
    Parser::new().advance(&mut bulk, &seq);
    let mut per_byte = LogSink::default();
    let mut parser = Parser::new();
    for &b in &seq {
        parser.advance_byte(&mut per_byte, b);
    }
    assert_eq!(bulk.events, per_byte.events);
}
