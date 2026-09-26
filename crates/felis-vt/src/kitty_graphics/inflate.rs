//! `o=z` zlib inflate for Kitty graphics payloads.
//!
//! The caller-supplied output cap is per `docs/explanation/security-model.md`
//! "Kitty graphics": a small zlib stream can explode into hundreds of MiB
//! (zip-bomb). The dispatcher sizes the cap from its session memory budget.

use miniz_oxide::inflate::{TINFLStatus, decompress_to_vec_zlib_with_limit};

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

#[cfg(test)]
mod tests {
    use super::*;
    use miniz_oxide::deflate::compress_to_vec_zlib;

    /// Compresses via a path independent from `inflate`.
    fn zlib_encode(input: &[u8]) -> Vec<u8> {
        compress_to_vec_zlib(input, 6)
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
