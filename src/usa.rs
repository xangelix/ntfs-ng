//! The update sequence of NTFS's other multi-sector structures: `INDX` index blocks and the
//! `RSTR` and `RCRD` pages of `$LogFile`.
//!
//! Each 512-byte stride of such a structure ends with the update sequence number while stored;
//! the bytes it replaced wait in the update sequence array (see [`crate::record`]).
//! [`unprotect`] checks every stride, then puts them back in place; [`protect`] undoes it.

use crate::{
    FormatError,
    bytes::{le16, take},
    record::{apply_update_sequence, check_update_sequence, restore_update_sequence},
};

/// Applies the update sequence of `buf`, a whole structure whose fixed header ends at
/// `header_end` (`RSTR`/`RCRD`: 32; `INDX`: 40), in place. On an error `buf` is unchanged.
///
/// # Errors
/// If the structure is not a whole number of strides, the array is misplaced or does not cover
/// it, or a stride is torn.
pub fn unprotect(buf: &mut [u8], header_end: usize) -> Result<(), FormatError> {
    let len = buf.len();
    if len < crate::record::SECTOR || !len.is_multiple_of(crate::record::SECTOR) {
        return Err(FormatError::RecordSize { len });
    }
    let head = take(buf, 0, 8, "update sequence header")?;
    let offset = le16(head, 4);
    let count = le16(head, 6);
    check_update_sequence(offset, count, len, header_end)?;
    apply_update_sequence(buf, usize::from(offset), usize::from(count))
}

/// Undoes [`unprotect`]: every stride of `buf` ends with the update sequence number again.
///
/// # Errors
/// If the array is misplaced or does not cover the structure.
pub fn protect(buf: &mut [u8], header_end: usize) -> Result<(), FormatError> {
    let len = buf.len();
    let head = take(buf, 0, 8, "update sequence header")?;
    let offset = le16(head, 4);
    let count = le16(head, 6);
    check_update_sequence(offset, count, len, header_end)?;
    restore_update_sequence(buf, usize::from(offset), usize::from(count));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::vec;

    use super::*;

    #[test]
    fn pages_unprotect_and_protect_exactly() {
        let mut page = vec![0u8; 4096];
        page[0..4].copy_from_slice(b"RCRD");
        page[4..6].copy_from_slice(&40u16.to_le_bytes());
        page[6..8].copy_from_slice(&9u16.to_le_bytes());
        page[40..42].copy_from_slice(&[7, 7]);
        for sector in 1..=8 {
            page[40 + sector * 2] = u8::try_from(sector).unwrap();
            page[sector * 512 - 2..sector * 512].copy_from_slice(&[7, 7]);
        }
        let stored = page.clone();
        unprotect(&mut page, 32).unwrap();
        assert_eq!(page[1022], 2);
        protect(&mut page, 32).unwrap();
        assert_eq!(page, stored);
        page[2047] = 0;
        let before = page.clone();
        assert!(matches!(
            unprotect(&mut page, 32),
            Err(FormatError::TornSector { sector: 3 })
        ));
        assert_eq!(page, before);
        assert!(unprotect(&mut page[..1000], 32).is_err());
        assert!(unprotect(&mut page, 48).is_err());
    }
}
