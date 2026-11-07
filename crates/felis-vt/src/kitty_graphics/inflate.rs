//! `o=z` zlib inflate for Kitty graphics payloads.
//!
//! The caller-supplied output cap is per `docs/explanation/security-model.md`
//! "Kitty graphics": a small zlib stream can explode into hundreds of MiB
//! (zip-bomb). The dispatcher sizes the cap from its session memory budget.

use miniz_oxide::inflate::{
    TINFLStatus,
    core::{DecompressorOxide, decompress, inflate_flags},
    decompress_to_vec_zlib_with_limit,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum InflateError {
    Decompression,
    /// Distinct from [`InflateError::Decompression`] so the dispatcher emits
    /// a budget failure rather than a malformed-input status.
    SizeLimit,
}

pub fn inflate(input: &[u8], max_output: usize) -> Result<Vec<u8>, InflateError> {
    decompress_to_vec_zlib_with_limit(input, max_output).map_err(|err| match err.status {
        TINFLStatus::HasMoreOutput => InflateError::SizeLimit,
        _ => InflateError::Decompression,
    })
}

/// [`inflate`] for a stream whose output size is declared: the buffer
/// is allocated once at `len` and never grows, so a stream inflating
/// past it fails with [`InflateError::SizeLimit`]. A shorter stream
/// returns what it inflated to.
pub fn inflate_exact(input: &[u8], len: usize) -> Result<Vec<u8>, InflateError> {
    let mut out = vec![0; len];
    let mut state = Box::<DecompressorOxide>::default();
    let flags = inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER
        | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
    let (status, _, written) = decompress(&mut state, input, &mut out, 0, flags);
    match status {
        TINFLStatus::Done => {
            out.truncate(written);
            Ok(out)
        }
        TINFLStatus::HasMoreOutput => Err(InflateError::SizeLimit),
        _ => Err(InflateError::Decompression),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miniz_oxide::deflate::compress_to_vec_zlib;

    /// Compresses via a path independent from `inflate`.
    fn zlib_encode(input: &[u8]) -> Vec<u8> {
        compress_to_vec_zlib(input, 6)
    }

    proptest::proptest! {
        /// Within its declared size a stream inflates as the uncapped
        /// decoder inflates it, filling the size exactly included; past
        /// it, it fails as over the size limit.
        #[test]
        fn an_exact_inflate_agrees_with_the_uncapped_one(
            data in proptest::collection::vec(proptest::prop_oneof![
                proptest::strategy::Just(0u8),
                proptest::num::u8::ANY,
            ], 0..4096),
            slack in -64isize..64,
            corrupt in proptest::option::of(proptest::num::usize::ANY),
        ) {
            let mut compressed = zlib_encode(&data);
            if let Some(at) = corrupt {
                let at = at % compressed.len();
                compressed[at] ^= 0x5A;
            }
            let len = data.len().saturating_add_signed(slack);
            let exact = inflate_exact(&compressed, len);
            match inflate(&compressed, 1 << 20) {
                Ok(out) if out.len() <= len => proptest::prop_assert_eq!(exact, Ok(out)),
                Ok(_) => proptest::prop_assert_eq!(exact, Err(InflateError::SizeLimit)),
                Err(err) => proptest::prop_assert!(
                    exact == Err(err) || exact == Err(InflateError::SizeLimit),
                    "{exact:?} for a stream the uncapped decoder refuses with {err:?}"
                ),
            }
        }
    }

    #[test]
    fn rejects_invalid_zlib_header() {
        let bytes = b"\x00\x01\x02\x03not zlib at all";
        let outcome = inflate(bytes, 1024);
        assert_eq!(outcome, Err(InflateError::Decompression));
    }

    #[test]
    fn rejects_truncated_stream() {
        let mut compressed = zlib_encode(b"the quick brown fox jumps over the lazy dog");
        compressed.truncate(compressed.len() - 4);
        let outcome = inflate(&compressed, 1024);
        assert_eq!(outcome, Err(InflateError::Decompression));
    }

    #[test]
    fn rejects_corrupted_checksum() {
        let mut compressed = zlib_encode(b"hello");
        let last = compressed.len() - 1;
        compressed[last] ^= 0xFF;
        let outcome = inflate(&compressed, 1024);
        assert_eq!(outcome, Err(InflateError::Decompression));
    }
}
