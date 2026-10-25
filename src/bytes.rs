//! Little-endian reads at offsets, checked once.
//!
//! Readers check that a structure is long enough ([`take`]), then read its fields with
//! [`field`] and the `le*` helpers, which cannot fail: a read past the checked bounds is a bug
//! here, never a property of the input, so it returns zeros (and asserts in debug builds)
//! instead of panicking.

use crate::FormatError;

/// `need` bytes of `data` at `at`, or why not.
#[inline]
pub(crate) fn take<'a>(
    data: &'a [u8],
    at: usize,
    need: usize,
    what: &'static str,
) -> Result<&'a [u8], FormatError> {
    at.checked_add(need)
        .and_then(|end| data.get(at..end))
        .ok_or(FormatError::Truncated {
            what,
            at,
            need,
            len: data.len(),
        })
}

/// The `N` bytes of `data` at `at`, if they are there.
#[inline]
pub(crate) fn array<const N: usize>(data: &[u8], at: usize) -> Option<[u8; N]> {
    data.get(at..)?.first_chunk::<N>().copied()
}

/// The `N` bytes of `data` at `at`, which the caller has checked are there (zeros, never a
/// panic, if they are not).
#[inline]
pub(crate) fn field<const N: usize>(data: &[u8], at: usize) -> [u8; N] {
    if let Some(bytes) = array(data, at) {
        bytes
    } else {
        debug_assert!(false, "field at {at} read past checked bounds");
        [0; N]
    }
}

#[inline]
pub(crate) fn le16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(field(data, at))
}

#[inline]
pub(crate) fn le32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(field(data, at))
}

#[inline]
pub(crate) fn le64(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(field(data, at))
}

/// The byte at `at`, which the caller has checked is there.
#[inline]
pub(crate) fn byte(data: &[u8], at: usize) -> u8 {
    field::<1>(data, at)[0]
}

/// `bytes` (at most 8) as an unsigned little-endian number.
#[inline]
pub(crate) fn unsigned(bytes: &[u8]) -> u64 {
    let mut raw = [0; 8];
    if let Some(slot) = raw.get_mut(..bytes.len()) {
        slot.copy_from_slice(bytes);
    }
    u64::from_le_bytes(raw)
}

/// `bytes` (1 to 8) as a signed little-endian number.
#[inline]
pub(crate) fn signed(bytes: &[u8]) -> i64 {
    let negative = bytes.last().is_some_and(|b| b & 0x80 != 0);
    let mut raw = [if negative { 0xff } else { 0 }; 8];
    if let Some(slot) = raw.get_mut(..bytes.len()) {
        slot.copy_from_slice(bytes);
    }
    i64::from_le_bytes(raw)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn reads_are_little_endian_and_checked() {
        let data = [1, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(le16(&data, 0), 0x0201);
        assert_eq!(le32(&data, 1), 0x0504_0302);
        assert_eq!(le64(&data, 1), 0x0908_0706_0504_0302);
        assert_eq!(byte(&data, 8), 9);
        assert!(take(&data, 8, 2, "x").is_err());
        assert!(take(&data, usize::MAX, 2, "x").is_err());
        assert_eq!(take(&data, 7, 2, "x").unwrap(), &[8, 9]);
        assert_eq!(array::<2>(&data, 8), None);
        assert_eq!(unsigned(&[0xff, 0x01]), 0x01ff);
        assert_eq!(signed(&[0xff]), -1);
        assert_eq!(signed(&[0x7f]), 127);
        assert_eq!(signed(&[0x00, 0x80]), -32768);
    }
}
