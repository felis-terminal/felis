//! Whole-block decoding with AVX2 (`x86_64`, detected at run time) or
//! NEON (`aarch64`) by Muła's nibble-table lookups; every other target,
//! and Kani, which cannot model the intrinsics, decodes no blocks here.

#[cfg(any(
    test,
    all(
        not(kani),
        any(
            target_arch = "x86_64",
            all(target_arch = "aarch64", target_feature = "neon")
        )
    )
))]
mod tables {
    /// Indexed by a byte's low nibble: the byte is in the alphabet iff
    /// `LO[c & 15] & HI[c >> 4] == 0`.
    pub(super) const LO: [u8; 16] = [
        0x15, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x13, 0x1A, 0x1B, 0x1B, 0x1B,
        0x1A,
    ];
    pub(super) const HI: [u8; 16] = [
        0x10, 0x10, 0x01, 0x02, 0x04, 0x08, 0x04, 0x08, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10,
        0x10,
    ];
    /// Added (wrapping) to an accepted byte to give its sextet, indexed by
    /// its high nibble, one less for `/` so it parts from `+`: 191 is -65,
    /// 185 is -71.
    pub(super) const ROLL: [u8; 16] = [0, 16, 19, 4, 191, 191, 185, 185, 0, 0, 0, 0, 0, 0, 0, 0];
}

/// Decodes the leading whole blocks of `body` onto `out`, returning how
/// many bytes they span: a multiple of four, at most `body.len()`.
/// `None` when one of those bytes is outside the alphabet.
#[cfg(all(target_arch = "x86_64", not(kani)))]
pub(super) fn decode_blocks(body: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    if !std::arch::is_x86_feature_detected!("avx2") {
        return Some(0);
    }
    // SAFETY: the CPU supports AVX2, checked just above; `avx2` is the
    // only feature `decode_blocks` enables.
    #[allow(unsafe_code)]
    unsafe {
        avx2::decode_blocks(body, out)
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon", not(kani)))]
pub(super) fn decode_blocks(body: &[u8], out: &mut Vec<u8>) -> Option<usize> {
    // SAFETY: this is compiled only with NEON in the build configuration,
    // and `neon` is the only feature `decode_blocks` enables.
    #[allow(unsafe_code)]
    unsafe {
        neon::decode_blocks(body, out)
    }
}

#[cfg(any(
    kani,
    not(any(
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_feature = "neon")
    ))
))]
#[allow(clippy::unnecessary_wraps)]
pub(super) const fn decode_blocks(_body: &[u8], _out: &mut Vec<u8>) -> Option<usize> {
    Some(0)
}

/// Input bytes one staging run spans: decoded runs are appended whole,
/// so the vector's spare capacity is never zeroed or written through.
#[cfg(all(
    any(
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_feature = "neon")
    ),
    not(kani)
))]
const RUN: usize = 512;

#[cfg(all(target_arch = "x86_64", not(kani)))]
mod avx2 {
    use std::arch::x86_64::{
        __m256i, _mm256_add_epi8, _mm256_and_si256, _mm256_cmpeq_epi8, _mm256_loadu_si256,
        _mm256_madd_epi16, _mm256_maddubs_epi16, _mm256_or_si256, _mm256_permutevar8x32_epi32,
        _mm256_set1_epi8, _mm256_set1_epi32, _mm256_setr_epi32, _mm256_setzero_si256,
        _mm256_shuffle_epi8, _mm256_srli_epi32, _mm256_storeu_si256, _mm256_testz_si256,
    };

    use super::RUN;
    use super::tables::{HI, LO, ROLL};

    const IN: usize = 32;
    const OUT: usize = 24;
    /// The last block's store writes a whole register, eight bytes past
    /// its output.
    const STAGING: usize = RUN / IN * OUT + 32 - OUT;

    #[target_feature(enable = "avx2")]
    fn load(src: &[u8; 32]) -> __m256i {
        // SAFETY: `src` is 32 readable bytes, and `loadu` takes any alignment.
        #[allow(unsafe_code)]
        unsafe {
            _mm256_loadu_si256(src.as_ptr().cast())
        }
    }

    /// Writes all 32 bytes of `v` at the front of `dst`.
    #[target_feature(enable = "avx2")]
    fn store(dst: &mut [u8], v: __m256i) {
        assert!(dst.len() >= 32);
        // SAFETY: `dst` has at least 32 writable bytes, asserted just
        // above, and `storeu` takes any alignment.
        #[allow(unsafe_code)]
        unsafe {
            _mm256_storeu_si256(dst.as_mut_ptr().cast(), v);
        }
    }

    const fn both_lanes(t: [u8; 16]) -> [u8; 32] {
        let mut out = [0; 32];
        let mut i = 0;
        while i < 32 {
            out[i] = t[i % 16];
            i += 1;
        }
        out
    }

    /// Per 128-bit lane, the big-endian bytes of four packed 24-bit groups.
    const PACK: [u8; 16] = [
        2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, 0x80, 0x80, 0x80, 0x80,
    ];

    #[target_feature(enable = "avx2")]
    pub(super) fn decode_blocks(body: &[u8], out: &mut Vec<u8>) -> Option<usize> {
        let lo_table = load(&const { both_lanes(LO) });
        let hi_table = load(&const { both_lanes(HI) });
        let roll_table = load(&const { both_lanes(ROLL) });
        let pack = load(&const { both_lanes(PACK) });
        let nibble = _mm256_set1_epi8(0x0F);
        let slash = _mm256_set1_epi8(b'/'.cast_signed());
        let blocks = body.as_chunks::<IN>().0;
        let mut staging = [0u8; STAGING];
        for run in blocks.chunks(RUN / IN) {
            let mut bad = _mm256_setzero_si256();
            for (i, block) in run.iter().enumerate() {
                let v = load(block);
                let hi = _mm256_and_si256(_mm256_srli_epi32::<4>(v), nibble);
                let lo = _mm256_and_si256(v, nibble);
                bad = _mm256_or_si256(
                    bad,
                    _mm256_and_si256(
                        _mm256_shuffle_epi8(lo_table, lo),
                        _mm256_shuffle_epi8(hi_table, hi),
                    ),
                );
                let roll = _mm256_shuffle_epi8(
                    roll_table,
                    _mm256_add_epi8(hi, _mm256_cmpeq_epi8(v, slash)),
                );
                let sextets = _mm256_add_epi8(v, roll);
                let pairs = _mm256_maddubs_epi16(sextets, _mm256_set1_epi32(0x0140_0140));
                let groups = _mm256_madd_epi16(pairs, _mm256_set1_epi32(0x0001_1000));
                let packed = _mm256_permutevar8x32_epi32(
                    _mm256_shuffle_epi8(groups, pack),
                    _mm256_setr_epi32(0, 1, 2, 4, 5, 6, 3, 7),
                );
                store(&mut staging[i * OUT..], packed);
            }
            if _mm256_testz_si256(bad, bad) == 0 {
                return None;
            }
            out.extend_from_slice(&staging[..run.len() * OUT]);
        }
        Some(blocks.len() * IN)
    }
}

#[cfg(all(target_arch = "aarch64", target_feature = "neon", not(kani)))]
mod neon {
    use std::arch::aarch64::{
        uint8x16_t, uint8x16x3_t, uint8x16x4_t, vaddq_u8, vandq_u8, vceqq_u8, vdupq_n_u8, vld1q_u8,
        vld4q_u8, vmaxvq_u8, vorrq_u8, vqtbl1q_u8, vshlq_n_u8, vshrq_n_u8, vst3q_u8,
    };

    use super::RUN;
    use super::tables::{HI, LO, ROLL};

    const IN: usize = 64;
    const OUT: usize = 48;

    #[target_feature(enable = "neon")]
    fn table(t: &[u8; 16]) -> uint8x16_t {
        // SAFETY: `t` is 16 readable bytes, and `vld1q_u8` takes any alignment.
        #[allow(unsafe_code)]
        unsafe {
            vld1q_u8(t.as_ptr())
        }
    }

    /// The four bytes of each of 16 groups, deinterleaved by position.
    #[target_feature(enable = "neon")]
    fn load(src: &[u8; 64]) -> uint8x16x4_t {
        // SAFETY: `src` is 64 readable bytes, and `vld4q_u8` takes any alignment.
        #[allow(unsafe_code)]
        unsafe {
            vld4q_u8(src.as_ptr())
        }
    }

    #[target_feature(enable = "neon")]
    fn store(dst: &mut [u8; 48], v: uint8x16x3_t) {
        // SAFETY: `dst` is 48 writable bytes, and `vst3q_u8` takes any alignment.
        #[allow(unsafe_code)]
        unsafe {
            vst3q_u8(dst.as_mut_ptr(), v);
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) fn decode_blocks(body: &[u8], out: &mut Vec<u8>) -> Option<usize> {
        let (lo_table, hi_table, roll_table) = (table(&LO), table(&HI), table(&ROLL));
        let sextets = |v: uint8x16_t, bad: &mut uint8x16_t| {
            let hi = vshrq_n_u8::<4>(v);
            let lo = vandq_u8(v, vdupq_n_u8(0x0F));
            *bad = vorrq_u8(
                *bad,
                vandq_u8(vqtbl1q_u8(lo_table, lo), vqtbl1q_u8(hi_table, hi)),
            );
            let index = vaddq_u8(hi, vceqq_u8(v, vdupq_n_u8(b'/')));
            vaddq_u8(v, vqtbl1q_u8(roll_table, index))
        };
        let blocks = body.as_chunks::<IN>().0;
        let mut staging = [0u8; RUN / IN * OUT];
        for run in blocks.chunks(RUN / IN) {
            let mut bad = vdupq_n_u8(0);
            for (dst, block) in staging.as_chunks_mut::<OUT>().0.iter_mut().zip(run) {
                let slots = load(block);
                let a = sextets(slots.0, &mut bad);
                let b = sextets(slots.1, &mut bad);
                let c = sextets(slots.2, &mut bad);
                let d = sextets(slots.3, &mut bad);
                store(
                    dst,
                    uint8x16x3_t(
                        vorrq_u8(vshlq_n_u8::<2>(a), vshrq_n_u8::<4>(b)),
                        vorrq_u8(vshlq_n_u8::<4>(b), vshrq_n_u8::<2>(c)),
                        vorrq_u8(vshlq_n_u8::<6>(c), d),
                    ),
                );
            }
            if vmaxvq_u8(bad) != 0 {
                return None;
            }
            out.extend_from_slice(&staging[..run.len() * OUT]);
        }
        Some(blocks.len() * IN)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{decode_char, decode_groups, decode_groups_scalar};
    use super::decode_blocks;
    use super::tables::{HI, LO, ROLL};

    /// One lane of either kernel, in scalar form.
    fn lane(c: u8) -> Option<u8> {
        let (hi, lo) = (usize::from(c >> 4), usize::from(c & 0x0F));
        if LO[lo] & HI[hi] != 0 {
            return None;
        }
        let index = if c == b'/' { hi - 1 } else { hi };
        Some(c.wrapping_add(ROLL[index]))
    }

    #[test]
    fn the_nibble_tables_classify_and_translate_every_byte_like_the_alphabet() {
        for c in 0..=u8::MAX {
            assert_eq!(lane(c), decode_char(c), "byte {c:#04x}");
        }
    }

    /// Every byte value in every lane of a 64-byte block (one NEON block,
    /// two AVX2 blocks), over a filler that spells the whole alphabet.
    #[test]
    fn every_byte_in_every_lane_decodes_like_the_scalar_groups() {
        let alphabet = *b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        for lane in 0..alphabet.len() {
            for c in 0..=u8::MAX {
                let mut body = alphabet;
                body[lane] = c;
                let (mut simd, mut scalar) = (Vec::new(), Vec::new());
                let ok = decode_groups(&body, &mut simd);
                assert_eq!(ok, decode_groups_scalar(&body, &mut scalar));
                if ok {
                    assert_eq!(simd, scalar, "byte {c:#04x} in lane {lane}");
                }
            }
        }
    }

    /// Guards against the differential tests passing because the
    /// dispatch quietly fell back to the scalar path.
    #[test]
    fn the_block_kernel_runs_where_the_cpu_has_one() {
        #[cfg(target_arch = "x86_64")]
        let live = std::arch::is_x86_feature_detected!("avx2");
        #[cfg(not(target_arch = "x86_64"))]
        let live = cfg!(all(target_arch = "aarch64", target_feature = "neon"));
        let body = [b'A'; 1100];
        let mut out = Vec::new();
        let done = decode_blocks(&body, &mut out).expect("all in the alphabet");
        if live {
            assert_eq!(done, 1088);
            assert_eq!(out, vec![0; 1088 / 4 * 3]);
        } else {
            assert_eq!(done, 0);
        }
    }
}
