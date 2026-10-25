//! Synthetic NTFS for tests and benchmarks, here and in callers.
//!
//! - [`record`]: byte-exact builders of `FILE` records and their attributes (resident and
//!   non-resident, every value type the crate reads), stored with their update sequence.
//! - [`corpus`]: a deterministic generator of realistic `$MFT`s of any size — directories,
//!   files, deleted records, hard links, alternate data streams, extension records with
//!   attribute lists, Unicode and long names, damaged and never-used slots — streamed to any
//!   writer, with the ground truth of every path.
//! - [`image`]: volume images with a boot sector, a fragmented `$MFT`, `$Bitmap`, and files
//!   with content (resident, non-resident, sparse, compressed, continued in extension records,
//!   deleted with reallocated clusters).
//! - [`lznt1_compress`]: an LZNT1 compressor for compressed streams.
//! - [`Rng`]: `SplitMix64`, the generators' only source of randomness.
//!
//! Builders panic on misuse (specs that do not fit their volume): they are test support.

#![allow(
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::missing_panics_doc
)]

pub mod corpus;
pub mod image;
pub mod record;

/// `SplitMix64`: a small, fast, deterministic generator.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number below `bound` (0 when `bound` is 0).
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        // Lemire's multiply-shift: unbiased enough for test data.
        #[allow(clippy::cast_possible_truncation)]
        let value = ((u128::from(self.next_u64()) * u128::from(bound)) >> 64) as u64;
        value
    }

    /// `true` with probability `per_million` / 1,000,000.
    pub fn chance(&mut self, per_million: u32) -> bool {
        self.below(1_000_000) < u64::from(per_million)
    }

    /// A random element of `items`.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        let at = usize::try_from(self.below(items.len() as u64)).ok()?;
        items.get(at)
    }
}

/// Compresses `input` with LZNT1 (greedy matches over each 4 KiB chunk; chunks that do not
/// shrink are stored uncompressed), ending with the zero header. Decodes with
/// [`crate::lznt1::decompress`].
#[must_use]
pub fn lznt1_compress(input: &[u8]) -> Vec<u8> {
    const CHUNK: usize = crate::lznt1::CHUNK;
    let mut out = Vec::new();
    for data in input.chunks(CHUNK) {
        let mut body = Vec::new();
        let mut position = 0;
        while position < data.len() {
            let flag_at = body.len();
            body.push(0u8);
            for bit in 0..8 {
                if position >= data.len() {
                    break;
                }
                let mut length_bits = 12u32;
                if position > 0 {
                    let mut grown = position - 1;
                    while grown >= 0x10 {
                        length_bits -= 1;
                        grown >>= 1;
                    }
                }
                let max_offset = (1usize << (16 - length_bits)).min(position);
                let max_length = ((1usize << length_bits) + 2).min(data.len() - position);
                let mut best = (0usize, 0usize);
                // Bounded search keeps generation fast on large inputs.
                for offset in 1..=max_offset.min(512) {
                    let mut length = 0;
                    while length < max_length
                        && data.get(position + length) == data.get(position + length - offset)
                    {
                        length += 1;
                    }
                    if length > best.1 {
                        best = (offset, length);
                    }
                }
                if best.1 >= 3 {
                    #[allow(clippy::cast_possible_truncation)]
                    let token = (((best.0 - 1) << length_bits) | (best.1 - 3)) as u16;
                    body.extend(token.to_le_bytes());
                    if let Some(flags) = body.get_mut(flag_at) {
                        *flags |= 1 << bit;
                    }
                    position += best.1;
                } else {
                    body.push(data.get(position).copied().unwrap_or(0));
                    position += 1;
                }
            }
        }
        if body.len() < data.len() && body.len() <= CHUNK {
            #[allow(clippy::cast_possible_truncation)]
            let header = 0xb000u16 | (body.len() + 2 - 3) as u16;
            out.extend(header.to_le_bytes());
            out.extend(body);
        } else {
            // Stored: the chunk as it is, with the compressed bit clear.
            #[allow(clippy::cast_possible_truncation)]
            let header = 0x3000u16 | (data.len() + 2 - 3) as u16;
            out.extend(header.to_le_bytes());
            out.extend_from_slice(data);
        }
    }
    out.extend([0, 0]);
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn rng_is_deterministic_and_bounded() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
            assert_eq!(a.below(10), b.below(10));
            assert!(a.below(10) < 10 && b.below(10) < 10);
        }
        assert_eq!(a.below(0), 0);
    }

    #[test]
    fn lznt1_round_trips() {
        let mut rng = Rng::new(3);
        for len in [0usize, 1, 100, 4096, 4097, 20_000, 65_536] {
            let data: Vec<u8> = (0..len)
                .map(|i| {
                    if rng.chance(100_000) {
                        rng.next_u64() as u8
                    } else {
                        (i / 7 % 13) as u8
                    }
                })
                .collect();
            let packed = lznt1_compress(&data);
            let mut out = vec![0xee; len.next_multiple_of(4096).max(4096)];
            crate::lznt1::decompress(&packed, &mut out).unwrap();
            assert_eq!(&out[..len], &data[..]);
        }
        // Incompressible data is stored.
        let noise: Vec<u8> = (0..8192).map(|_| rng.next_u64() as u8).collect();
        let packed = lznt1_compress(&noise);
        let mut out = vec![0; 8192];
        crate::lznt1::decompress(&packed, &mut out).unwrap();
        assert_eq!(out, noise);
    }
}
