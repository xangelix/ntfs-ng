//! `$ATTRIBUTE_LIST` (0x20).

use crate::{
    FormatError,
    bytes::{byte, le16, le32, le64, take},
    name::Name,
    record::FileReference,
};

/// One entry of an `$ATTRIBUTE_LIST`: where one attribute (or one extent of a non-resident
/// one) of a file spread over several records is.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0x00 | 4 | attribute type |
/// | 0x04 | 2 | entry length |
/// | 0x06 | 1 | name length, in UTF-16 code units |
/// | 0x07 | 1 | name offset |
/// | 0x08 | 8 | lowest VCN in the record named |
/// | 0x10 | 8 | the record holding the attribute |
/// | 0x18 | 2 | attribute identifier |
/// | 0x1A | … | name |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttributeListEntry<'a> {
    /// The attribute's type code.
    pub attribute_type: u32,
    /// The entry's length.
    pub length: u16,
    /// The first VCN of this extent (0 for a resident attribute or a first extent).
    pub lowest_vcn: u64,
    /// The record holding the attribute, with the sequence number it must have.
    pub segment: FileReference,
    /// The attribute's identifier (instance) in that record.
    pub attribute_id: u16,
    /// The attribute's name.
    pub name: Name<'a>,
    /// The entry's offset in the list.
    pub offset: usize,
}

impl AttributeListEntry<'_> {
    /// The fixed part of an entry.
    pub const HEADER: usize = 0x1a;
}

/// The entries of an `$ATTRIBUTE_LIST` value, in order. Each entry is checked to be at least
/// its header long and within the value, as is its name; the first that is not ends the
/// iteration with its error.
#[derive(Clone, Debug)]
pub struct AttributeList<'a> {
    value: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> AttributeList<'a> {
    /// The entries of `value`.
    #[must_use]
    pub const fn new(value: &'a [u8]) -> Self {
        Self {
            value,
            at: 0,
            done: false,
        }
    }

    fn read(&self) -> Result<AttributeListEntry<'a>, FormatError> {
        let at = self.at;
        let length = le16(
            take(self.value, at + 4, 2, "attribute-list entry length")?,
            0,
        );
        if usize::from(length) < AttributeListEntry::HEADER {
            return Err(FormatError::AttributeListEntryLength { at, length });
        }
        let entry = take(self.value, at, usize::from(length), "attribute-list entry")?;
        let name_length = byte(entry, 6);
        let name_offset = byte(entry, 7);
        let name = if name_length > 0 {
            if usize::from(name_offset) < AttributeListEntry::HEADER {
                return Err(FormatError::AttributeListNameOffset {
                    at,
                    offset: name_offset,
                });
            }
            Name::new(take(
                entry,
                usize::from(name_offset),
                usize::from(name_length) * 2,
                "attribute-list entry name",
            )?)
        } else {
            Name::default()
        };
        Ok(AttributeListEntry {
            attribute_type: le32(entry, 0),
            length,
            lowest_vcn: le64(entry, 0x08),
            segment: FileReference::from_u64(le64(entry, 0x10)),
            attribute_id: le16(entry, 0x18),
            name,
            offset: at,
        })
    }
}

impl<'a> Iterator for AttributeList<'a> {
    type Item = Result<AttributeListEntry<'a>, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.at >= self.value.len() {
            return None;
        }
        let entry = self.read();
        match &entry {
            Ok(entry) => self.at += usize::from(entry.length),
            Err(_) => self.done = true,
        }
        Some(entry)
    }
}

impl core::iter::FusedIterator for AttributeList<'_> {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{vec, vec::Vec};

    use super::*;

    #[test]
    fn attribute_list_entries_check_lengths_and_names() {
        let mut entry = vec![0; 32];
        entry[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        entry[4..6].copy_from_slice(&32u16.to_le_bytes());
        entry[6] = 2;
        entry[7] = 26;
        entry[16..24].copy_from_slice(&(42u64 | 3 << 48).to_le_bytes());
        entry[26..30].copy_from_slice(&[b'a', 0, b'b', 0]);
        let list = [entry.clone(), entry.clone()].concat();
        let entries: Vec<_> = AttributeList::new(&list).collect::<Result<_, _>>().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].name.to_string_lossy(), "ab");
        assert_eq!(entries[1].segment, FileReference::new(42, 3));
        assert_eq!(entries[1].offset, 32);

        let mut short = entry.clone();
        short[4..6].copy_from_slice(&20u16.to_le_bytes());
        assert!(matches!(
            AttributeList::new(&short).next(),
            Some(Err(FormatError::AttributeListEntryLength { .. }))
        ));
        let mut overlapping = entry.clone();
        overlapping[7] = 24;
        assert!(matches!(
            AttributeList::new(&overlapping).next(),
            Some(Err(FormatError::AttributeListNameOffset { .. }))
        ));
        let mut long_name = entry;
        long_name[6] = 4;
        assert!(matches!(
            AttributeList::new(&long_name).next(),
            Some(Err(FormatError::Truncated { .. }))
        ));
        assert!(AttributeList::new(&[0; 5]).next().unwrap().is_err());
        assert_eq!(AttributeList::new(&[]).count(), 0);
    }
}
