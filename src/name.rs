//! Names as NTFS stores them: UTF-16 little-endian code units, borrowed from the record they
//! were read from and decoded only when asked.
//!
//! NTFS does not require names to be valid UTF-16: a name may hold unpaired surrogates. Such a
//! name has no exact UTF-8 form; [`Name::push_lossy`] replaces each unpaired surrogate with
//! U+FFFD, [`Name::push_strict`] refuses it, and [`Name::push_wtf8`] keeps it (WTF-8, the
//! encoding of Rust's `OsString` on Windows). Most names are ASCII, which every method decodes
//! without per-character work.

use alloc::{string::String, vec::Vec};
use core::{char::DecodeUtf16Error, fmt};

/// UTF-16LE code units borrowed from a record.
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Name<'a>(&'a [u8]);

impl<'a> Name<'a> {
    /// The code units in `bytes`; an odd trailing byte is not one and is left out.
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self(bytes.split_at(bytes.len() & !1).0)
    }

    /// The code units, as stored.
    #[must_use]
    pub const fn bytes(self) -> &'a [u8] {
        self.0
    }

    /// The number of code units.
    #[must_use]
    pub const fn len(self) -> usize {
        self.0.len() / 2
    }

    /// Whether the name is empty.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0.is_empty()
    }

    /// The code units, in order.
    pub fn units(self) -> impl DoubleEndedIterator<Item = u16> + ExactSizeIterator + 'a {
        self.0
            .as_chunks::<2>()
            .0
            .iter()
            .map(|unit| u16::from_le_bytes(*unit))
    }

    /// The code units before the first NUL (all of them when there is none).
    #[must_use]
    pub fn until_nul(self) -> Self {
        let end = self
            .0
            .as_chunks::<2>()
            .0
            .iter()
            .position(|unit| *unit == [0, 0])
            .map_or(self.0.len(), |unit| unit * 2);
        Self(self.0.get(..end).unwrap_or(self.0))
    }

    /// The characters, or an error for each unpaired surrogate.
    pub fn chars(self) -> impl Iterator<Item = Result<char, DecodeUtf16Error>> + 'a {
        char::decode_utf16(self.units())
    }

    /// Whether every code unit is ASCII (below 0x80).
    #[must_use]
    #[inline]
    pub fn is_ascii(self) -> bool {
        // Branch-free over eight bytes (four units) at a time: a unit is ASCII when its high
        // byte is 0 and its low byte's top bit is clear.
        let (chunks, rest) = self.0.as_chunks::<8>();
        let mut acc = 0u64;
        for chunk in chunks {
            acc |= u64::from_le_bytes(*chunk);
        }
        let mut tail = [0u8; 8];
        if let Some(slot) = tail.get_mut(..rest.len()) {
            slot.copy_from_slice(rest);
        }
        acc |= u64::from_le_bytes(tail);
        acc & 0xFF80_FF80_FF80_FF80 == 0
    }

    /// Appends an all-ASCII name to `out` (the caller checked [`Self::is_ascii`]).
    #[inline]
    fn push_ascii(self, out: &mut String) {
        let mut narrow = [0u8; 128];
        for chunk in self.0.chunks(256) {
            let units = chunk.as_chunks::<2>().0;
            for (byte, unit) in narrow.iter_mut().zip(units) {
                *byte = unit[0];
            }
            if let Some(Ok(text)) = narrow.get(..units.len()).map(core::str::from_utf8) {
                out.push_str(text);
            }
        }
    }

    /// Appends the name to `out` as UTF-8, with U+FFFD for each unpaired surrogate.
    #[inline]
    pub fn push_lossy(self, out: &mut String) {
        if self.is_ascii() {
            self.push_ascii(out);
            return;
        }
        out.reserve(self.len());
        out.extend(
            self.chars()
                .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
        );
    }

    /// Appends the name to `out` as UTF-8.
    ///
    /// # Errors
    /// At the first unpaired surrogate; `out` is then as it was.
    pub fn push_strict(self, out: &mut String) -> Result<(), DecodeUtf16Error> {
        if self.is_ascii() {
            self.push_ascii(out);
            return Ok(());
        }
        let start = out.len();
        out.reserve(self.len());
        for c in self.chars() {
            match c {
                Ok(c) => out.push(c),
                Err(error) => {
                    out.truncate(start);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Appends the name to `out` as WTF-8: UTF-8, with each unpaired surrogate encoded as the
    /// three bytes UTF-8 would give its code point. Exact and reversible for every name.
    pub fn push_wtf8(self, out: &mut Vec<u8>) {
        let units = self.0.as_chunks::<2>().0;
        if self.is_ascii() {
            out.extend(units.iter().map(|unit| unit[0]));
        } else {
            push_wtf8_units(units, out);
        }
    }

    /// Whether the name is valid UTF-16.
    ///
    /// # Errors
    /// At the first unpaired surrogate.
    pub fn validate(self) -> Result<(), DecodeUtf16Error> {
        if self.is_ascii() {
            return Ok(());
        }
        self.chars().try_for_each(|c| c.map(drop))
    }

    /// The name as UTF-8, with U+FFFD for each unpaired surrogate.
    #[must_use]
    pub fn to_string_lossy(self) -> String {
        let mut out = String::with_capacity(self.len());
        self.push_lossy(&mut out);
        out
    }

    /// Whether the name is `text`, exactly.
    #[must_use]
    pub fn eq_str(self, text: &str) -> bool {
        match self.eq_ascii(text, |byte| byte) {
            Some(equal) => equal,
            None => self.eq_utf16(text, |unit| unit),
        }
    }

    /// Whether the name is `text`, ignoring ASCII case (as Windows compares most names; full
    /// case folding needs the volume's `$UpCase`).
    #[must_use]
    pub fn eq_ignore_ascii_case(self, text: &str) -> bool {
        let lower = |byte: u8| byte | (u8::from(byte.wrapping_sub(b'A') < 26) << 5);
        match self.eq_ascii(text, lower) {
            Some(equal) => equal,
            None => self.eq_utf16(text, |unit| {
                if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) {
                    unit | 0x20
                } else {
                    unit
                }
            }),
        }
    }

    /// Whether the name is `text` encoded as UTF-16, each unit taken through `fold`: for text
    /// that is not ASCII, kept out of line so that the ASCII comparison stays small.
    #[inline(never)]
    fn eq_utf16(self, text: &str, fold: impl Fn(u16) -> u16) -> bool {
        self.units().map(&fold).eq(text.encode_utf16().map(&fold))
    }

    /// Whether the name is `text`, each byte and unit taken through `fold` (which leaves bytes
    /// past ASCII as they are), told without encoding `text` when its length decides: `None`
    /// when `text` must be compared as UTF-16.
    ///
    /// UTF-8 never takes fewer bytes than UTF-16 takes units, and as many only for ASCII. So a
    /// shorter `text` differs; one as long matches only if it is ASCII, and is compared unit by
    /// byte, every unit with no early exit so that the loop vectorizes; a longer one is not
    /// ASCII and is left to the caller.
    #[inline]
    fn eq_ascii(self, text: &str, fold: impl Fn(u8) -> u8) -> Option<bool> {
        let bytes = text.as_bytes();
        match bytes.len().cmp(&self.len()) {
            core::cmp::Ordering::Less => Some(false),
            core::cmp::Ordering::Greater => None,
            core::cmp::Ordering::Equal => {
                let (differ, high) = self.0.as_chunks::<2>().0.iter().zip(bytes).fold(
                    (0u8, 0u8),
                    |(differ, high), (unit, &byte)| {
                        // A unit equals an ASCII byte only with a zero high byte.
                        (differ | unit[1] | (fold(unit[0]) ^ fold(byte)), high | byte)
                    },
                );
                Some(differ == 0 && high < 0x80)
            }
        }
    }

    /// Whether the name is `units`, exactly.
    #[must_use]
    pub fn eq_units(self, units: &[u16]) -> bool {
        self.len() == units.len() && self.units().eq(units.iter().copied())
    }
}

/// Appends `units` (UTF-16LE) as WTF-8, encoded straight from the units into room reserved once:
/// a unit takes at most three bytes, and a surrogate pair (two units) four.
fn push_wtf8_units(units: &[[u8; 2]], out: &mut Vec<u8>) {
    out.reserve(units.len() * 3);
    let mut at = 0;
    while let Some(&unit) = units.get(at) {
        let unit = u16::from_le_bytes(unit);
        at += 1;
        match unit {
            0..=0x7F => out.push(unit as u8),
            0x80..=0x7FF => {
                out.extend_from_slice(&[0xC0 | (unit >> 6) as u8, 0x80 | (unit & 0x3F) as u8]);
            }
            0xD800..=0xDBFF => {
                let low = units
                    .get(at)
                    .map(|low| u16::from_le_bytes(*low))
                    .filter(|low| (0xDC00..=0xDFFF).contains(low));
                if let Some(low) = low {
                    at += 1;
                    let c =
                        0x1_0000 + ((u32::from(unit) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
                    out.extend_from_slice(&[
                        0xF0 | (c >> 18) as u8,
                        0x80 | ((c >> 12) & 0x3F) as u8,
                        0x80 | ((c >> 6) & 0x3F) as u8,
                        0x80 | (c & 0x3F) as u8,
                    ]);
                } else {
                    push_three(out, unit);
                }
            }
            // The rest of the BMP, and unpaired low surrogates (kept, as WTF-8 keeps them).
            _ => push_three(out, unit),
        }
    }
}

/// Appends `unit`, a code point of the BMP or an unpaired surrogate, as three bytes.
#[inline]
fn push_three(out: &mut Vec<u8>, unit: u16) {
    out.extend_from_slice(&[
        0xE0 | (unit >> 12) as u8,
        0x80 | ((unit >> 6) & 0x3F) as u8,
        0x80 | (unit & 0x3F) as u8,
    ]);
}

impl fmt::Display for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.chars() {
            fmt::Write::write_char(f, c.unwrap_or(char::REPLACEMENT_CHARACTER))?;
        }
        Ok(())
    }
}

impl fmt::Debug for Name<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{
        format,
        string::{String, ToString},
        vec,
        vec::Vec,
    };

    use super::Name;

    fn encode(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn names_decode_strictly_lossily_and_exactly() {
        let long = "x".repeat(300);
        for text in [
            "",
            "plain.txt",
            "日本語.txt",
            "\u{1F600} emoji",
            "nul\0inside",
            "seven77",
            long.as_str(),
        ] {
            let bytes = encode(text);
            let name = Name::new(&bytes);
            assert_eq!(name.len(), text.encode_utf16().count());
            assert_eq!(name.is_ascii(), text.is_ascii());
            let mut out = String::from("prefix:");
            name.push_strict(&mut out).unwrap();
            assert_eq!(out, format!("prefix:{text}"));
            assert_eq!(name.to_string_lossy(), text);
            assert_eq!(name.to_string(), text);
            let mut wtf8 = Vec::new();
            name.push_wtf8(&mut wtf8);
            assert_eq!(wtf8, text.as_bytes());
            assert!(name.validate().is_ok());
            assert!(name.eq_str(text));
        }
        for units in [[0xd800_u16, 0x41], [0x41, 0xdc00]] {
            let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
            let name = Name::new(&bytes);
            let mut out = String::from("kept");
            assert!(name.push_strict(&mut out).is_err());
            assert_eq!(out, "kept");
            assert!(name.validate().is_err());
            assert!(name.to_string_lossy().contains('\u{FFFD}'));
            let mut wtf8 = Vec::new();
            name.push_wtf8(&mut wtf8);
            assert_eq!(wtf8.len(), 4);
        }
    }

    #[test]
    fn non_ascii_high_bytes_and_comparisons() {
        // A unit with a zero high byte but a high low byte (é) is not ASCII.
        let bytes = encode("café");
        assert!(!Name::new(&bytes).is_ascii());
        assert_eq!(Name::new(&bytes).to_string_lossy(), "café");
        assert!(Name::new(&encode("ReadMe.TXT")).eq_ignore_ascii_case("readme.txt"));
        assert!(!Name::new(&encode("ReadMe.TXT")).eq_str("readme.txt"));
        assert!(Name::new(&encode("ab")).eq_units(&[0x61, 0x62]));
        // Lengths: one unit against two bytes of UTF-8 (equal), two units against two bytes
        // that are one character (never equal), and a longer name against shorter text.
        for (name, text, equal) in [
            ("é", "é", true),
            ("É", "é", false),
            ("ab", "é", false),
            ("é", "e", false),
            ("abc", "ab", false),
            ("😀", "😀", true),
        ] {
            let bytes = encode(name);
            assert_eq!(Name::new(&bytes).eq_str(text), equal, "{name} {text}");
            assert_eq!(
                Name::new(&bytes).eq_ignore_ascii_case(text),
                equal,
                "{name} {text}"
            );
        }
        // Only ASCII letters fold: É and é stay apart.
        assert!(Name::new(&encode("RÉSUMÉ.TXT")).eq_ignore_ascii_case("rÉsumÉ.txt"));
        assert!(!Name::new(&encode("RÉSUMÉ.TXT")).eq_ignore_ascii_case("résumé.txt"));
    }

    /// WTF-8 as `char::decode_utf16` defines it: pairs as one code point, an unpaired
    /// surrogate as three bytes.
    fn reference_wtf8(units: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        for c in char::decode_utf16(units.iter().copied()) {
            match c {
                Ok(c) => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
                Err(error) => {
                    let unit = error.unpaired_surrogate();
                    out.extend_from_slice(&[
                        0xE0 | (unit >> 12) as u8,
                        0x80 | ((unit >> 6) & 0x3F) as u8,
                        0x80 | (unit & 0x3F) as u8,
                    ]);
                }
            }
        }
        out
    }

    /// The direct encoder and the comparisons agree with their definitions on every kind of
    /// unit: ASCII, two- and three-byte, pairs, unpaired surrogates of both halves.
    #[test]
    fn wtf8_and_comparisons_match_their_definitions() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let kinds: [fn(u64) -> u16; 7] = [
            |r| (r % 0x80) as u16,
            |r| (b'A' + (r % 26) as u8) as u16,
            |r| 0x80 + (r % 0x780) as u16,
            |r| 0x800 + (r % 0xD000) as u16,
            |r| 0xD800 + (r % 0x400) as u16,
            |r| 0xDC00 + (r % 0x400) as u16,
            |r| 0xE000 + (r % 0x2000) as u16,
        ];
        for _ in 0..20_000 {
            let len = (next() % 24) as usize;
            let units: Vec<u16> = (0..len)
                .map(|_| kinds[(next() % 7) as usize](next()))
                .collect();
            let bytes: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
            let name = Name::new(&bytes);
            let mut wtf8 = vec![b'>'];
            name.push_wtf8(&mut wtf8);
            assert_eq!(&wtf8[1..], &reference_wtf8(&units)[..], "{units:04x?}");
            // Against itself, against an ASCII case change, and against near misses.
            let text = name.to_string_lossy();
            let lower = text.to_ascii_lowercase();
            let shorter: String = text.chars().skip(1).collect();
            for candidate in [text.as_str(), lower.as_str(), shorter.as_str(), "", "a"] {
                let exact = units.iter().copied().eq(candidate.encode_utf16());
                assert_eq!(name.eq_str(candidate), exact, "{units:04x?} {candidate:?}");
                let fold = |u: u16| {
                    if (0x61..=0x7A).contains(&u) {
                        u - 32
                    } else {
                        u
                    }
                };
                let folded = units
                    .iter()
                    .map(|&u| fold(u))
                    .eq(candidate.encode_utf16().map(fold));
                assert_eq!(
                    name.eq_ignore_ascii_case(candidate),
                    folded,
                    "{units:04x?} {candidate:?}"
                );
            }
        }
    }

    #[test]
    fn odd_bytes_and_nuls_end_names() {
        assert_eq!(Name::new(&[b'a', 0, b'b']).len(), 1);
        let bytes = encode("ab\0cd");
        assert_eq!(Name::new(&bytes).until_nul().to_string_lossy(), "ab");
        assert_eq!(Name::new(&encode("abc")).until_nul().len(), 3);
        assert!(Name::new(&encode("\0x")).until_nul().is_empty());
    }
}
