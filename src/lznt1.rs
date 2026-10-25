//! LZNT1, the compression of NTFS's compressed attributes ([MS-XCA] §2.5).
//!
//! A compressed attribute is cut into compression units (16 clusters, usually 64 KiB). A unit
//! stored compressed is a sequence of chunks, each holding up to 4 KiB of output: a 16-bit
//! header (bit 15 set when the chunk is compressed; the low 12 bits its size, less 3), then
//! either the 4 KiB as they are, or groups of eight tokens behind a flag byte: a literal byte,
//! or a back-reference whose split between offset and length bits widens as the chunk's output
//! grows. A zero header ends the unit. A chunk that decodes to less than 4 KiB is followed by
//! zeros up to its 4 KiB, as NTFS reads it.

use crate::{
    FormatError,
    bytes::{byte, le16},
};

/// Bytes of output per chunk.
pub const CHUNK: usize = 4096;

/// Decodes the compressed unit `input` into `output`, at most `output.len()` bytes, and
/// returns how many bytes were written: a unit that ends early leaves the rest of `output`
/// untouched (NTFS reads it as zeros). Decoding stops once `output` is full.
///
/// # Errors
/// If a chunk runs past `input`, a back-reference reaches before its chunk, or a chunk decodes
/// to more than 4 KiB.
pub fn decompress(input: &[u8], output: &mut [u8]) -> Result<usize, FormatError> {
    let mut at = 0usize;
    let mut written = 0usize;
    while written < output.len() {
        let Some(header) = input.get(at..at + 2).map(|bytes| le16(bytes, 0)) else {
            break;
        };
        if header == 0 {
            break;
        }
        let size = usize::from(header & 0x0fff) + 3;
        let end = at + size;
        let Some(data) = input.get(at + 2..end) else {
            return Err(FormatError::Compressed { at });
        };
        let chunk_start = written;
        let chunk_end = (chunk_start + CHUNK).min(output.len());
        if header & 0x8000 == 0 {
            let room = output.get_mut(chunk_start..chunk_end).unwrap_or_default();
            let len = data.len().min(room.len());
            if let (Some(slot), Some(bytes)) = (room.get_mut(..len), data.get(..len)) {
                slot.copy_from_slice(bytes);
            }
            if data.len() > CHUNK {
                return Err(FormatError::Compressed { at });
            }
            written = chunk_start + len;
        } else {
            written = chunk(data, at + 2, output, chunk_start, chunk_end)?;
        }
        // A short chunk is followed by zeros up to its 4 KiB.
        if let Some(rest) = output.get_mut(written..chunk_end) {
            rest.fill(0);
        }
        written = chunk_end;
        at = end;
    }
    Ok(written)
}

/// Decodes one compressed chunk into `output[start..end]`; returns where its output ends.
fn chunk(
    data: &[u8],
    base: usize,
    output: &mut [u8],
    start: usize,
    end: usize,
) -> Result<usize, FormatError> {
    let mut at = 0usize;
    let mut out = start;
    while at < data.len() {
        let flags = byte(data, at);
        at += 1;
        for bit in 0..8 {
            if at >= data.len() {
                break;
            }
            if flags & (1 << bit) == 0 {
                if out >= end {
                    return finish(out, end, start, base + at);
                }
                if let Some(slot) = output.get_mut(out) {
                    *slot = byte(data, at);
                }
                out += 1;
                at += 1;
                continue;
            }
            let Some(token) = data.get(at..at + 2).map(|bytes| le16(bytes, 0)) else {
                return Err(FormatError::Compressed { at: base + at });
            };
            at += 2;
            let position = out - start;
            if position == 0 {
                return Err(FormatError::Compressed { at: base + at });
            }
            // The offset takes more bits the further into the chunk the output is: four, plus
            // one for each bit of the position past four.
            let grown = (usize::BITS - (position - 1).leading_zeros()).saturating_sub(4);
            let length_bits = 12 - grown.min(8);
            let length = usize::from(token & ((1u16 << length_bits) - 1)) + 3;
            let offset = usize::from(token >> length_bits) + 1;
            if offset > position {
                return Err(FormatError::Compressed { at: base + at });
            }
            if out + length > start + CHUNK {
                return Err(FormatError::Compressed { at: base + at });
            }
            // Copy in blocks of at most `offset` bytes: each block's source lies wholly before
            // its destination, so a repeating pattern (offset < length) unrolls correctly.
            let take = length.min(end - out);
            let mut done = 0;
            while done < take {
                let block = offset.min(take - done);
                let from = out + done - offset;
                if from + block <= output.len() && out + done + block <= output.len() {
                    output.copy_within(from..from + block, out + done);
                }
                done += block;
            }
            out += take;
            if take < length {
                return finish(out, end, start, base + at);
            }
        }
    }
    Ok(out)
}

/// Output reached `end`: the end of `output` (stop decoding), or past a full chunk (invalid).
fn finish(out: usize, end: usize, start: usize, at: usize) -> Result<usize, FormatError> {
    if end - start < CHUNK {
        Ok(out)
    } else {
        Err(FormatError::Compressed { at })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{format, vec, vec::Vec};

    use super::*;

    /// A minimal LZNT1 compressor (literals and greedy back-references) for round trips.
    pub(crate) fn compress(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk_data in input.chunks(CHUNK) {
            let mut body = Vec::new();
            let mut position = 0;
            while position < chunk_data.len() {
                let flag_at = body.len();
                body.push(0u8);
                for bit in 0..8 {
                    if position >= chunk_data.len() {
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
                    let max_length =
                        ((1usize << length_bits) - 1 + 3).min(chunk_data.len() - position);
                    let mut best = (0, 0);
                    for offset in 1..=max_offset {
                        let mut length = 0;
                        while length < max_length
                            && chunk_data[position + length]
                                == chunk_data[position + length - offset]
                        {
                            length += 1;
                        }
                        if length > best.1 {
                            best = (offset, length);
                        }
                    }
                    if best.1 >= 3 {
                        let token = (((best.0 - 1) << length_bits) | (best.1 - 3)) as u16;
                        body.extend(token.to_le_bytes());
                        body[flag_at] |= 1 << bit;
                        position += best.1;
                    } else {
                        body.push(chunk_data[position]);
                        position += 1;
                    }
                }
            }
            let header = 0xb000u16 | u16::try_from(body.len() + 2 - 3).unwrap();
            out.extend(header.to_le_bytes());
            out.extend(body);
        }
        out.extend([0, 0]);
        out
    }

    #[test]
    fn round_trips_through_back_references() {
        let mut data = Vec::new();
        for i in 0..10_000u32 {
            data.extend_from_slice(format!("line {} of text ", i % 37).as_bytes());
        }
        data.truncate(3 * CHUNK + 1000);
        let packed = compress(&data);
        assert!(packed.len() < data.len() / 2);
        let mut out = vec![0xee; 4 * CHUNK];
        let written = decompress(&packed, &mut out).unwrap();
        assert_eq!(written, 4 * CHUNK);
        assert_eq!(&out[..data.len()], &data[..]);
        // The short last chunk is followed by zeros up to its 4 KiB.
        assert!(out[data.len()..].iter().all(|b| *b == 0));
        // A smaller output stops decoding where it is full.
        let mut small = vec![0; 5000];
        assert_eq!(decompress(&packed, &mut small).unwrap(), 5000);
        assert_eq!(&small[..], &data[..5000]);
    }

    #[test]
    fn uncompressed_chunks_and_runs() {
        let mut raw = vec![0x3fffu16.to_le_bytes()[0], 0x3fffu16.to_le_bytes()[1]];
        raw.extend((0..CHUNK).map(|i| (i % 251) as u8));
        raw.extend([0, 0]);
        let mut out = vec![0; CHUNK];
        assert_eq!(decompress(&raw, &mut out).unwrap(), CHUNK);
        assert_eq!(out[250], 250);
        // A run: one literal, then a back-reference of offset 1 repeating it.
        let token: u16 = 100 - 3;
        let mut run = vec![0, 0, 0b10, b'z'];
        run.extend(token.to_le_bytes());
        let size = u16::try_from(run.len() - 2 - 1).unwrap();
        run[0..2].copy_from_slice(&(0xb000 | size).to_le_bytes());
        let mut out = vec![1; 200];
        decompress(&run, &mut out).unwrap();
        assert!(out[..101].iter().all(|b| *b == b'z'));
        assert!(out[101..].iter().all(|b| *b == 0));
    }

    #[test]
    fn hostile_input_is_refused() {
        // A back-reference at the start of a chunk.
        let mut bad = vec![0, 0, 0b1, 0x00, 0x10];
        bad[0..2].copy_from_slice(&(0xb000u16 | 2).to_le_bytes());
        assert!(decompress(&bad, &mut vec![0; CHUNK]).is_err());
        // A chunk that claims more input than there is.
        assert!(decompress(&[0xff, 0xbf, 1, 2], &mut vec![0; CHUNK]).is_err());
        // An offset past the output so far.
        let mut far = vec![0, 0, 0b10, b'a'];
        far.extend((5u16 << 12).to_le_bytes());
        let size = u16::try_from(far.len() - 3).unwrap();
        far[0..2].copy_from_slice(&(0xb000 | size).to_le_bytes());
        assert!(decompress(&far, &mut vec![0; CHUNK]).is_err());
        // Empty input decodes nothing.
        assert_eq!(decompress(&[], &mut vec![0; CHUNK]).unwrap(), 0);
        // Arbitrary bytes never panic.
        let mut state = 0x1234_5678u32;
        for _ in 0..2000 {
            let len = (state % 300) as usize;
            let input: Vec<u8> = (0..len)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect();
            let _ = decompress(&input, &mut vec![0; 2 * CHUNK]);
        }
    }
}
