//! `FILE` records: the header of each `$MFT` entry, and the update sequence that protects its
//! sectors.
//!
//! NTFS writes a record sector by sector. Before writing, it replaces the last two bytes of each
//! 512-byte stride with the record's update sequence number and keeps the replaced bytes in the
//! update sequence array; a reader checks that every stride still ends with the number (one
//! that does not was written apart from the rest: the record is "torn"), then puts the replaced
//! bytes back. [`Record::fixup`] does that in place, after checking every stride first, so a
//! refused record is left exactly as it was read; [`Record::raw_into`] undoes it exactly.
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0x00 | 4 | signature `FILE` |
//! | 0x04 | 2 | update sequence array offset |
//! | 0x06 | 2 | update sequence array entries (the number, then one per stride) |
//! | 0x08 | 8 | `$LogFile` sequence number |
//! | 0x10 | 2 | sequence number, incremented each time the record is freed |
//! | 0x12 | 2 | hard link count |
//! | 0x14 | 2 | offset of the first attribute |
//! | 0x16 | 2 | flags ([`flags`]) |
//! | 0x18 | 4 | bytes in use |
//! | 0x1C | 4 | bytes allocated: the record size |
//! | 0x20 | 8 | base record reference (0 for a base record) |
//! | 0x28 | 2 | next attribute identifier |
//! | 0x2C | 4 | the record's own number (NTFS 3.1 and later) |

use alloc::vec::Vec;

use crate::{
    FormatError,
    attribute::{Attribute, Attributes},
    bytes::{field, le16},
};

/// The stride the update sequence protects, whatever the sector size.
pub const SECTOR: usize = 512;
/// The end of the header NTFS 1.2 writes; the update sequence array never starts before it.
pub const HEADER: usize = 42;
/// The end of the NTFS 3.1 header, which adds the record's own number.
const HEADER_WITH_NUMBER: usize = 48;
/// The largest record (and index block) size accepted.
pub const MAX_SIZE: usize = 64 << 10;

/// The record header's flags.
pub mod flags {
    /// The record is in use.
    pub const IN_USE: u16 = 0x0001;
    /// The record is a directory: it holds a `$I30` file name index.
    pub const DIRECTORY: u16 = 0x0002;
    /// The record is in `$Extend`.
    pub const IN_EXTEND: u16 = 0x0004;
    /// The record holds a view index (`$Secure`, `$ObjId`, `$Quota`, `$Reparse`).
    pub const VIEW_INDEX: u16 = 0x0008;
}

/// A file reference: a record number (48 bits) and the sequence number the record had (16
/// bits). A reference whose sequence no longer matches the record's names an earlier use of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileReference {
    /// The record number.
    pub record: u64,
    /// The sequence number.
    pub sequence: u16,
}

impl FileReference {
    /// A reference to `record` at `sequence`.
    #[must_use]
    pub const fn new(record: u64, sequence: u16) -> Self {
        Self { record, sequence }
    }

    /// The reference stored as `value`.
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self {
            record: value & 0x0000_ffff_ffff_ffff,
            // Lossless: the top 16 bits.
            #[allow(clippy::cast_possible_truncation)]
            sequence: (value >> 48) as u16,
        }
    }

    /// The reference as stored.
    #[must_use]
    pub const fn to_u64(self) -> u64 {
        (self.record & 0x0000_ffff_ffff_ffff) | ((self.sequence as u64) << 48)
    }
}

/// The sequence the last file of a freed record had, from the record's sequence now.
///
/// NTFS increments a record's sequence when it frees the record, so an unallocated record still
/// holding a `$FILE_NAME` last held a file at the sequence before. The increment skips 0 when it
/// wraps, and a record at 0 is not incremented, so a sequence of 1 follows 0xFFFF or a first use
/// at 0, and one of 0 follows 0: neither says which, and both give `None`.
#[must_use]
pub const fn previous_sequence(sequence: u16) -> Option<u16> {
    if sequence >= 2 {
        Some(sequence - 1)
    } else {
        None
    }
}

/// The sequence a record at `sequence` has once freed: one more, wrapping from 0xFFFF to 1.
#[must_use]
pub const fn next_sequence(sequence: u16) -> u16 {
    if sequence == u16::MAX {
        1
    } else {
        sequence + 1
    }
}

/// A `FILE` record's header, its geometry checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordHeader {
    /// The signature, `FILE`.
    pub signature: [u8; 4],
    /// Where the update sequence array starts.
    pub update_sequence_offset: u16,
    /// Entries in the update sequence array: the number itself, then one per 512-byte stride.
    pub update_sequence_count: u16,
    /// The `$LogFile` sequence number of the record's last change.
    pub log_sequence_number: u64,
    /// The sequence number.
    pub sequence: u16,
    /// Hard links (names in directory indexes).
    pub link_count: u16,
    /// Where the first attribute starts.
    pub first_attribute: u16,
    /// [`flags`].
    pub flags: u16,
    /// Bytes in use.
    pub used_size: u32,
    /// Bytes allocated: the record's size.
    pub allocated_size: u32,
    /// The base record, for an extension record; zero for a base record.
    pub base_record: FileReference,
    /// The next attribute identifier.
    pub next_attribute_id: u16,
    /// The record's own number, which NTFS 3.1 writes at 0x2C; `None` when the update sequence
    /// array starts before it (an older header).
    pub record_number: Option<u32>,
}

impl RecordHeader {
    /// Read the header of `raw`, a whole record, and check its geometry: the signature; the
    /// allocated size is the record's length; the used size covers the first attribute and the
    /// `$END` marker; the update sequence array lies after the header, within the first stride,
    /// with one entry per stride; and the first attribute is 8-byte aligned after the array.
    /// The strides themselves are checked by [`Record::fixup`].
    ///
    /// # Errors
    /// If any of those does not hold.
    pub fn read(raw: &[u8]) -> Result<Self, FormatError> {
        let len = raw.len();
        if len < SECTOR || !len.is_multiple_of(SECTOR) || len > MAX_SIZE {
            return Err(FormatError::RecordSize { len });
        }
        let Some(h) = raw.first_chunk::<HEADER_WITH_NUMBER>() else {
            return Err(FormatError::RecordSize { len });
        };
        let u16_at = |at: usize| u16::from_le_bytes(field(h, at));
        let u32_at = |at: usize| u32::from_le_bytes(field(h, at));
        let signature = [h[0], h[1], h[2], h[3]];
        if signature != *b"FILE" {
            return Err(FormatError::Signature { found: signature });
        }
        let update_sequence_offset = u16_at(0x04);
        let header = Self {
            signature,
            update_sequence_offset,
            update_sequence_count: u16_at(0x06),
            log_sequence_number: u64::from_le_bytes(field(h, 0x08)),
            sequence: u16_at(0x10),
            link_count: u16_at(0x12),
            first_attribute: u16_at(0x14),
            flags: u16_at(0x16),
            used_size: u32_at(0x18),
            allocated_size: u32_at(0x1c),
            base_record: FileReference::from_u64(u64::from_le_bytes(field(h, 0x20))),
            next_attribute_id: u16_at(0x28),
            record_number: (usize::from(update_sequence_offset) >= HEADER_WITH_NUMBER)
                .then(|| u32_at(0x2c)),
        };
        let used = header.used_size as usize;
        let first = usize::from(header.first_attribute);
        if header.allocated_size as usize != len || used > len || used < first + 4 {
            return Err(FormatError::Sizes {
                allocated: header.allocated_size,
                used: header.used_size,
                first_attribute: header.first_attribute,
                len,
            });
        }
        let array_end = check_update_sequence(
            update_sequence_offset,
            header.update_sequence_count,
            len,
            HEADER,
        )?;
        if !first.is_multiple_of(8) || first < array_end {
            return Err(FormatError::FirstAttribute {
                offset: header.first_attribute,
                header_end: array_end,
            });
        }
        Ok(header)
    }

    /// Checks the number an NTFS 3.1 record states against `expected`, its place in `$MFT`.
    /// Records of older headers state none and pass, and so does a stated 0: formatters leave
    /// the number unset in records they reserve (`mkntfs` in records 16 to 23).
    ///
    /// # Errors
    /// If the record states another number: its bytes belong to another record.
    pub fn check_number(&self, expected: u64) -> Result<(), FormatError> {
        match self.record_number {
            Some(found) if found != 0 && u64::from(found) != expected & 0xffff_ffff => {
                Err(FormatError::RecordNumber { expected, found })
            }
            _ => Ok(()),
        }
    }

    /// Whether the record is in use.
    #[must_use]
    pub const fn in_use(&self) -> bool {
        self.flags & flags::IN_USE != 0
    }

    /// Whether the record is a directory.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.flags & flags::DIRECTORY != 0
    }

    /// Whether this is a base record, not an extension of another: its base reference is
    /// zero. (An extension record of `$MFT` names record 0 with a nonzero sequence, so the
    /// record number alone does not tell.)
    #[must_use]
    pub const fn is_base(&self) -> bool {
        self.base_record.record == 0 && self.base_record.sequence == 0
    }
}

/// Checks an update sequence array's geometry for a structure of `len` bytes whose header ends
/// at `header_end`, and returns where the array ends.
pub(crate) fn check_update_sequence(
    offset: u16,
    count: u16,
    len: usize,
    header_end: usize,
) -> Result<usize, FormatError> {
    let at = usize::from(offset);
    let entries = usize::from(count);
    let array_end = at + entries * 2;
    if entries != len / SECTOR + 1
        || at < header_end
        || !at.is_multiple_of(2)
        || array_end > SECTOR - 2
    {
        return Err(FormatError::UpdateSequence { offset, count, len });
    }
    Ok(array_end)
}

/// Applies an update sequence whose geometry was checked, in place: every stride is checked
/// before any is changed, so a torn structure is left as it was.
pub(crate) fn apply_update_sequence(
    buf: &mut [u8],
    offset: usize,
    count: usize,
) -> Result<(), FormatError> {
    let number: [u8; 2] = field(buf, offset);
    for sector in 1..count {
        let end = sector * SECTOR - 2;
        if field::<2>(buf, end) != number {
            return Err(FormatError::TornSector { sector: sector - 1 });
        }
    }
    for sector in 1..count {
        let end = sector * SECTOR - 2;
        let saved: [u8; 2] = field(buf, offset + sector * 2);
        if let Some(tail) = buf.get_mut(end..end + 2) {
            tail.copy_from_slice(&saved);
        }
    }
    Ok(())
}

/// Undoes [`apply_update_sequence`]: every stride ends with the update sequence number again.
pub(crate) fn restore_update_sequence(buf: &mut [u8], offset: usize, count: usize) {
    let number: [u8; 2] = field(buf, offset);
    for sector in 1..count {
        let end = sector * SECTOR - 2;
        if let Some(tail) = buf.get_mut(end..end + 2) {
            tail.copy_from_slice(&number);
        }
    }
}

/// A `FILE` record with its update sequence applied, borrowed from the buffer it was fixed up
/// in.
#[derive(Clone, Copy, Debug)]
pub struct Record<'a> {
    header: RecordHeader,
    data: &'a [u8],
}

impl<'a> Record<'a> {
    /// Check `buf`, a whole record as stored ([`RecordHeader::read`]), and apply its update
    /// sequence in place. On an error `buf` is unchanged.
    ///
    /// # Errors
    /// If the header is invalid, or a stride is torn.
    pub fn fixup(buf: &'a mut [u8]) -> Result<Self, FormatError> {
        let header = RecordHeader::read(buf)?;
        apply_update_sequence(
            buf,
            usize::from(header.update_sequence_offset),
            usize::from(header.update_sequence_count),
        )?;
        Ok(Self { header, data: buf })
    }

    /// Check `raw`, a whole record as stored, and copy it into `buffer` with its update sequence
    /// applied, leaving `raw` as it is. `buffer` is reused: cleared first, it grows to the
    /// record's size at most.
    ///
    /// # Errors
    /// If the header is invalid, or a stride is torn.
    pub fn read(raw: &[u8], buffer: &'a mut Vec<u8>) -> Result<Self, FormatError> {
        let header = RecordHeader::read(raw)?;
        buffer.clear();
        buffer.extend_from_slice(raw);
        apply_update_sequence(
            buffer,
            usize::from(header.update_sequence_offset),
            usize::from(header.update_sequence_count),
        )?;
        Ok(Self {
            header,
            data: buffer,
        })
    }

    /// A record from a header already read and checked from `data`, whose update sequence was
    /// applied.
    #[cfg(feature = "std")]
    pub(crate) const fn from_parts(header: RecordHeader, data: &'a [u8]) -> Self {
        Self { header, data }
    }

    /// A record whose update sequence was already applied; only its header is checked.
    ///
    /// # Errors
    /// If the header is invalid.
    pub fn from_fixed(data: &'a [u8]) -> Result<Self, FormatError> {
        Ok(Self {
            header: RecordHeader::read(data)?,
            data,
        })
    }

    /// The header.
    #[must_use]
    pub const fn header(&self) -> &RecordHeader {
        &self.header
    }

    /// The whole record, fixed up.
    #[must_use]
    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The bytes in use, fixed up: the header and the attributes.
    #[must_use]
    pub fn used(&self) -> &'a [u8] {
        self.data
            .get(..self.header.used_size as usize)
            .unwrap_or(self.data)
    }

    /// The bytes after the used ones: record slack, as stored (strides fixed up).
    #[must_use]
    pub fn slack(&self) -> &'a [u8] {
        self.data
            .get(self.header.used_size as usize..)
            .unwrap_or_default()
    }

    /// The attributes, in order, up to the `$END` marker.
    #[must_use]
    pub fn attributes(&self) -> Attributes<'a> {
        Attributes::new(self.used(), usize::from(self.header.first_attribute))
    }

    /// The first attribute of type `type_code` (stopping at the first invalid attribute).
    #[must_use]
    pub fn find(&self, type_code: u32) -> Option<Attribute<'a>> {
        self.attributes()
            .map_while(Result::ok)
            .find(|attribute| attribute.type_code() == type_code)
    }

    /// Whether the record is in use.
    #[must_use]
    pub const fn in_use(&self) -> bool {
        self.header.in_use()
    }

    /// Whether the record is a directory.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.header.is_directory()
    }

    /// Whether this is a base record.
    #[must_use]
    pub const fn is_base(&self) -> bool {
        self.header.is_base()
    }

    /// The record's sequence number.
    #[must_use]
    pub const fn sequence(&self) -> u16 {
        self.header.sequence
    }

    /// The reference to this record at `number` with its sequence.
    #[must_use]
    pub const fn reference(&self, number: u64) -> FileReference {
        FileReference::new(number, self.header.sequence)
    }

    /// Checks the number an NTFS 3.1 record states against `expected`, its place in `$MFT`
    /// (see [`RecordHeader::check_number`]).
    ///
    /// # Errors
    /// If the record states another number: its bytes belong to another record.
    pub fn check_number(&self, expected: u64) -> Result<(), FormatError> {
        self.header.check_number(expected)
    }

    /// The record as stored: its bytes with the update sequence number back at the end of every
    /// stride, written to `out` (cleared first).
    pub fn raw_into(&self, out: &mut Vec<u8>) {
        out.clear();
        out.extend_from_slice(self.data);
        restore_update_sequence(
            out,
            usize::from(self.header.update_sequence_offset),
            usize::from(self.header.update_sequence_count),
        );
    }

    /// The update sequence number every stride ended with.
    #[must_use]
    pub fn update_sequence_number(&self) -> u16 {
        le16(self.data, usize::from(self.header.update_sequence_offset))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::vec;

    use super::*;

    /// A blank 1 KiB record whose update sequence array is at 48.
    fn blank() -> Vec<u8> {
        let mut data = vec![0; 1024];
        data[..4].copy_from_slice(b"FILE");
        data[4..6].copy_from_slice(&48u16.to_le_bytes());
        data[6..8].copy_from_slice(&3u16.to_le_bytes());
        data[20..22].copy_from_slice(&56u16.to_le_bytes());
        data[24..28].copy_from_slice(&64u32.to_le_bytes());
        data[28..32].copy_from_slice(&1024u32.to_le_bytes());
        data[44..48].copy_from_slice(&7u32.to_le_bytes());
        data[56..60].copy_from_slice(&u32::MAX.to_le_bytes());
        // The update sequence number, and the saved stride tails.
        data[48..50].copy_from_slice(&[0x5a, 0xa5]);
        data[50..54].copy_from_slice(&[1, 2, 3, 4]);
        data[510..512].copy_from_slice(&[0x5a, 0xa5]);
        data[1022..1024].copy_from_slice(&[0x5a, 0xa5]);
        data
    }

    #[test]
    fn sequences_wrap_as_ntfs_does() {
        assert_eq!(previous_sequence(2), Some(1));
        assert_eq!(previous_sequence(0xffff), Some(0xfffe));
        assert_eq!(previous_sequence(1), None);
        assert_eq!(previous_sequence(0), None);
        assert_eq!(next_sequence(1), 2);
        assert_eq!(next_sequence(u16::MAX), 1);
    }

    #[test]
    fn fixups_apply_in_place_and_undo_exactly() {
        let raw = blank();
        let mut buf = raw.clone();
        let record = Record::fixup(&mut buf).unwrap();
        assert_eq!(record.data()[510..512], [1, 2]);
        assert_eq!(record.data()[1022..1024], [3, 4]);
        assert_eq!(record.used().len(), 64);
        assert_eq!(record.header().record_number, Some(7));
        assert_eq!(record.attributes().count(), 0);
        assert_eq!(record.update_sequence_number(), 0xa55a);
        assert!(record.check_number(7).is_ok());
        assert!(matches!(
            record.check_number(8),
            Err(FormatError::RecordNumber {
                expected: 8,
                found: 7
            })
        ));
        let mut stored = Vec::new();
        record.raw_into(&mut stored);
        assert_eq!(stored, raw);
        // A stated 0 is unset (records a formatter reserved), and passes at any place.
        let mut unset = raw.clone();
        unset[44..48].fill(0);
        let mut buf = unset.clone();
        let record = Record::fixup(&mut buf).unwrap();
        assert_eq!(record.header().record_number, Some(0));
        assert!(record.check_number(17).is_ok());

        let mut copy = Vec::new();
        let copied = Record::read(&raw, &mut copy).unwrap();
        assert_eq!(copied.data()[510..512], [1, 2]);
    }

    /// An update sequence array at the end of a 64 KiB record (offset 0xFFFE) is refused:
    /// never an offset that wraps.
    #[test]
    fn update_sequences_past_the_record() {
        let mut record = vec![0u8; MAX_SIZE];
        record[..4].copy_from_slice(b"FILE");
        record[4..6].copy_from_slice(&0xfffeu16.to_le_bytes());
        record[6..8].copy_from_slice(&2u16.to_le_bytes());
        record[20..22].copy_from_slice(&56u16.to_le_bytes());
        record[24..28].copy_from_slice(&64u32.to_le_bytes());
        record[28..32].copy_from_slice(&(MAX_SIZE as u32).to_le_bytes());
        assert!(matches!(
            Record::fixup(&mut record),
            Err(FormatError::UpdateSequence { .. })
        ));
    }

    #[test]
    fn torn_records_are_left_untouched() {
        let mut torn = blank();
        torn[1023] = 0;
        let before = torn.clone();
        assert_eq!(
            Record::fixup(&mut torn).unwrap_err(),
            FormatError::TornSector { sector: 1 }
        );
        assert_eq!(torn, before);
    }

    #[test]
    fn bad_geometry_is_refused() {
        let cases: [(usize, &[u8]); 7] = [
            (0, b"BAAD"),
            (28, &1000u32.to_le_bytes()),
            (24, &1025u32.to_le_bytes()),
            (24, &59u32.to_le_bytes()),
            (6, &2u16.to_le_bytes()),
            (4, &41u16.to_le_bytes()),
            (20, &52u16.to_le_bytes()),
        ];
        for (at, bytes) in cases {
            let mut raw = blank();
            raw[at..at + bytes.len()].copy_from_slice(bytes);
            assert!(RecordHeader::read(&raw).is_err(), "{at}");
        }
        let mut raw = blank();
        raw[4..6].copy_from_slice(&506u16.to_le_bytes());
        assert!(matches!(
            RecordHeader::read(&raw),
            Err(FormatError::UpdateSequence { .. })
        ));
        assert!(matches!(
            RecordHeader::read(&raw[..1000]),
            Err(FormatError::RecordSize { len: 1000 })
        ));
    }

    #[test]
    fn references_split_record_and_sequence() {
        let reference = FileReference::from_u64(0x0007_0000_0000_0005);
        assert_eq!((reference.record, reference.sequence), (5, 7));
        assert_eq!(reference.to_u64(), 0x0007_0000_0000_0005);
    }
}
