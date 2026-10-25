//! Indexes: `$INDEX_ROOT` (0x90), the `INDX` blocks of `$INDEX_ALLOCATION` (0xA0), and their
//! entries.

use crate::{
    FormatError,
    bytes::{le16, le32, le64, take},
    record::{FileReference, apply_update_sequence, check_update_sequence},
};

/// Collation rules: how an index orders its keys.
pub mod collation {
    /// Byte order.
    pub const BINARY: u32 = 0x00;
    /// File names (`$I30`), upcased.
    pub const FILE_NAME: u32 = 0x01;
    /// Unicode strings.
    pub const UNICODE_STRING: u32 = 0x02;
    /// One 32-bit number.
    pub const NTOFS_ULONG: u32 = 0x10;
    /// A security identifier.
    pub const NTOFS_SID: u32 = 0x11;
    /// A security descriptor hash and identifier.
    pub const NTOFS_SECURITY_HASH: u32 = 0x12;
    /// Several 32-bit numbers.
    pub const NTOFS_ULONGS: u32 = 0x13;

    /// Whether `rule` is one NTFS defines.
    #[must_use]
    pub const fn is_known(rule: u32) -> bool {
        matches!(rule, BINARY | FILE_NAME | UNICODE_STRING | 0x10..=0x13)
    }
}

/// An index node's header, at 0x10 of `$INDEX_ROOT` and 0x18 of an `INDX` block; its offsets
/// count from the header's own start.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0x00 | 4 | offset of the first entry |
/// | 0x04 | 4 | bytes the entries use, header included |
/// | 0x08 | 4 | bytes allocated to them |
/// | 0x0C | 4 | flags: 1 when the node has children (a large index) |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexNodeHeader {
    /// Offset of the first entry.
    pub entries_offset: u32,
    /// Bytes the entries use, header included.
    pub index_length: u32,
    /// Bytes allocated to them.
    pub allocated_length: u32,
    /// 1 when the node has children.
    pub flags: u32,
}

impl IndexNodeHeader {
    /// Its size.
    pub const SIZE: usize = 0x10;

    fn read(data: &[u8], at: usize) -> Self {
        Self {
            entries_offset: le32(data, at),
            index_length: le32(data, at + 4),
            allocated_length: le32(data, at + 8),
            flags: le32(data, at + 12),
        }
    }

    /// The entries of the node whose header starts at `at` of `data`: from the first entry to
    /// the used length, clamped to `data`.
    fn entries<'a>(&self, data: &'a [u8], at: usize) -> Result<IndexEntries<'a>, FormatError> {
        let start = at.saturating_add(self.entries_offset as usize);
        let end = at
            .saturating_add(self.index_length as usize)
            .min(data.len());
        if (self.entries_offset as usize) < Self::SIZE || start > end {
            return Err(FormatError::IndexNode {
                entries_offset: self.entries_offset,
                index_length: self.index_length,
            });
        }
        Ok(IndexEntries {
            node: data.get(..end).unwrap_or(data),
            at: start,
            done: false,
        })
    }
}

/// `$INDEX_ROOT` (0x90): an index's parameters and its root node, small enough to stay in the
/// record.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0x00 | 4 | type of the attribute indexed (0x30 for `$I30`; 0 for view indexes) |
/// | 0x04 | 4 | collation rule ([`collation`]) |
/// | 0x08 | 4 | bytes per index block (`INDX` record) |
/// | 0x0C | 4 | clusters per index block (1 byte, then padding) |
/// | 0x10 | 16 | the node header ([`IndexNodeHeader`]) |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexRoot {
    /// The type of the attribute indexed.
    pub attribute_type: u32,
    /// The collation rule.
    pub collation_rule: u32,
    /// Bytes per index block.
    pub index_block_size: u32,
    /// Clusters per index block (the low byte; the rest is padding).
    pub clusters_per_index_block: u32,
    /// The root node's header.
    pub node: IndexNodeHeader,
}

impl IndexRoot {
    /// The index parameters and the node header.
    pub const HEADER: usize = 0x20;
    /// Where the node header starts.
    pub const NODE: usize = 0x10;

    /// Read `value`'s parameters and node header. The entries are read by [`Self::entries`].
    ///
    /// # Errors
    /// If it is shorter than both.
    pub fn parse(value: &[u8]) -> Result<Self, FormatError> {
        let header = take(value, 0, Self::HEADER, "$INDEX_ROOT")?;
        Ok(Self {
            attribute_type: le32(header, 0x00),
            collation_rule: le32(header, 0x04),
            index_block_size: le32(header, 0x08),
            clusters_per_index_block: le32(header, 0x0c),
            node: IndexNodeHeader::read(header, Self::NODE),
        })
    }

    /// Where the node's entries end in `value`: the node header's start plus the length it
    /// declares.
    #[must_use]
    pub fn node_end(&self) -> usize {
        Self::NODE.saturating_add(self.node.index_length as usize)
    }

    /// The root node's entries in `value`, through the entry flagged as the last.
    ///
    /// # Errors
    /// If the first entry lies outside the node.
    pub fn entries<'a>(&self, value: &'a [u8]) -> Result<IndexEntries<'a>, FormatError> {
        self.node.entries(value, Self::NODE)
    }
}

/// An `INDX` block of `$INDEX_ALLOCATION`, its update sequence applied in place.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0x00 | 4 | signature `INDX` |
/// | 0x04 | 2 | update sequence array offset |
/// | 0x06 | 2 | update sequence array entries |
/// | 0x08 | 8 | `$LogFile` sequence number |
/// | 0x10 | 8 | the block's VCN in the index |
/// | 0x18 | 16 | the node header ([`IndexNodeHeader`]) |
#[derive(Clone, Copy, Debug)]
pub struct IndexBlock<'a> {
    data: &'a [u8],
    /// The `$LogFile` sequence number of the block's last change.
    pub log_sequence_number: u64,
    /// The block's VCN.
    pub vcn: u64,
    /// The node header.
    pub node: IndexNodeHeader,
}

impl<'a> IndexBlock<'a> {
    /// Where the node header starts.
    pub const NODE: usize = 0x18;
    /// The end of the fixed header, where the update sequence array may start.
    pub const HEADER: usize = 0x28;

    /// Check `buf`, a whole block as stored, and apply its update sequence in place. On an
    /// error `buf` is unchanged.
    ///
    /// # Errors
    /// If the signature, the update sequence geometry or a stride is invalid.
    pub fn fixup(buf: &'a mut [u8]) -> Result<Self, FormatError> {
        let len = buf.len();
        if len < crate::record::SECTOR || !len.is_multiple_of(crate::record::SECTOR) {
            return Err(FormatError::RecordSize { len });
        }
        let head = take(buf, 0, Self::HEADER, "INDX header")?;
        let signature: [u8; 4] = crate::bytes::field(head, 0);
        if signature != *b"INDX" {
            return Err(FormatError::Signature { found: signature });
        }
        let offset = le16(head, 4);
        let count = le16(head, 6);
        check_update_sequence(offset, count, len, Self::HEADER)?;
        let log_sequence_number = le64(head, 8);
        let vcn = le64(head, 0x10);
        let node = IndexNodeHeader::read(head, Self::NODE);
        apply_update_sequence(buf, usize::from(offset), usize::from(count))?;
        Ok(Self {
            data: buf,
            log_sequence_number,
            vcn,
            node,
        })
    }

    /// The whole block, fixed up.
    #[must_use]
    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The block's entries, through the entry flagged as the last.
    ///
    /// # Errors
    /// If the first entry lies outside the node.
    pub fn entries(&self) -> Result<IndexEntries<'a>, FormatError> {
        self.node.entries(self.data, Self::NODE)
    }

    /// The bytes after the used entries, up to the end of the block: slack, where entries of
    /// deleted files linger.
    #[must_use]
    pub fn slack(&self) -> &'a [u8] {
        let used = Self::NODE.saturating_add(self.node.index_length as usize);
        self.data.get(used..).unwrap_or_default()
    }
}

/// An index entry's flags.
pub mod index_entry_flags {
    /// The entry points to a child node: its last 8 bytes are the child's VCN.
    pub const NODE: u16 = 0x01;
    /// The last entry of its node, which holds no key.
    pub const END: u16 = 0x02;
}

/// One index entry.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0x00 | 8 | the file indexed (`$I30`), or a view index's data offset and size |
/// | 0x08 | 2 | entry length |
/// | 0x0A | 2 | key length |
/// | 0x0C | 2 | flags ([`index_entry_flags`]) |
/// | 0x0E | 2 | padding |
/// | 0x10 | … | the key (a `$FILE_NAME` in `$I30`), then a child's VCN |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntry<'a> {
    /// The first 8 bytes, as stored.
    pub reference: u64,
    /// The entry's length.
    pub length: u16,
    /// The key's length.
    pub key_length: u16,
    /// The flags ([`index_entry_flags`]).
    pub flags: u16,
    /// Everything after the header, up to the entry's length.
    pub body: &'a [u8],
    /// The entry's offset in its node's data.
    pub offset: usize,
}

impl<'a> IndexEntry<'a> {
    /// The fixed header.
    pub const HEADER: usize = 0x10;

    /// The entry at `at` of `node`.
    ///
    /// # Errors
    /// If its header or the length it declares runs past `node`, or it is shorter than its
    /// header.
    pub fn read(node: &'a [u8], at: usize) -> Result<Self, FormatError> {
        let header = take(node, at, Self::HEADER, "index entry")?;
        let length = le16(header, 0x08);
        if usize::from(length) < Self::HEADER {
            return Err(FormatError::IndexEntryLength { at, length });
        }
        let entry = take(node, at, usize::from(length), "index entry")?;
        Ok(Self {
            reference: le64(header, 0),
            length,
            key_length: le16(header, 0x0a),
            flags: le16(header, 0x0c),
            body: entry.get(Self::HEADER..).unwrap_or_default(),
            offset: at,
        })
    }

    /// The file an `$I30` entry indexes.
    #[must_use]
    pub const fn file_reference(&self) -> FileReference {
        FileReference::from_u64(self.reference)
    }

    /// Whether this is its node's last entry.
    #[must_use]
    pub const fn is_last(&self) -> bool {
        self.flags & index_entry_flags::END != 0
    }

    /// The key, as long as the entry declares it: `None` when the key runs past the entry.
    #[must_use]
    pub fn key(&self) -> Option<&'a [u8]> {
        self.body.get(..usize::from(self.key_length))
    }

    /// The child node's VCN, for an entry that points to one.
    #[must_use]
    pub fn child_vcn(&self) -> Option<u64> {
        if self.flags & index_entry_flags::NODE == 0 {
            return None;
        }
        let at = self.body.len().checked_sub(8)?;
        crate::bytes::array::<8>(self.body, at).map(u64::from_le_bytes)
    }
}

/// The entries of an index node, in order, through the one flagged as the last. The first that
/// is invalid ends the iteration with its error.
#[derive(Clone, Debug)]
pub struct IndexEntries<'a> {
    node: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> Iterator for IndexEntries<'a> {
    type Item = Result<IndexEntry<'a>, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let entry = IndexEntry::read(self.node, self.at);
        match &entry {
            Ok(entry) if !entry.is_last() => self.at += usize::from(entry.length),
            _ => self.done = true,
        }
        Some(entry)
    }
}

impl core::iter::FusedIterator for IndexEntries<'_> {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{vec, vec::Vec};

    use super::*;
    use crate::value::FileName;

    fn file_name(name: &str) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut value = vec![0; FileName::HEADER];
        value[0..8].copy_from_slice(&(5u64 | 5 << 48).to_le_bytes());
        value[0x40] = u8::try_from(name.encode_utf16().count()).unwrap();
        value[0x41] = 1;
        value.extend(units);
        value
    }

    /// One entry holding `key`, then the last entry.
    fn node_entries(key: &[u8]) -> Vec<u8> {
        let entry_len = (IndexEntry::HEADER + key.len()).next_multiple_of(8);
        let mut entry = vec![0; entry_len];
        entry[0..8].copy_from_slice(&(64u64 | 1 << 48).to_le_bytes());
        entry[8..10].copy_from_slice(&u16::try_from(entry_len).unwrap().to_le_bytes());
        entry[10..12].copy_from_slice(&u16::try_from(key.len()).unwrap().to_le_bytes());
        entry[16..16 + key.len()].copy_from_slice(key);
        let mut last = vec![0; 24];
        last[8..10].copy_from_slice(&24u16.to_le_bytes());
        last[12..14]
            .copy_from_slice(&(index_entry_flags::END | index_entry_flags::NODE).to_le_bytes());
        last[16..24].copy_from_slice(&7u64.to_le_bytes());
        entry.extend(last);
        entry
    }

    #[test]
    fn index_roots_walk_their_entries_to_the_last() {
        let key = file_name("child");
        let mut value = vec![0; IndexRoot::HEADER];
        value[0..4].copy_from_slice(&0x30u32.to_le_bytes());
        value[4..8].copy_from_slice(&collation::FILE_NAME.to_le_bytes());
        value[8..12].copy_from_slice(&4096u32.to_le_bytes());
        value[12] = 1;
        value[0x10..0x14].copy_from_slice(&16u32.to_le_bytes());
        value.extend(node_entries(&key));
        let index_length = u32::try_from(value.len() - IndexRoot::NODE).unwrap();
        value[0x14..0x18].copy_from_slice(&index_length.to_le_bytes());
        value[0x18..0x1c].copy_from_slice(&index_length.to_le_bytes());

        let root = IndexRoot::parse(&value).unwrap();
        assert!(collation::is_known(root.collation_rule));
        assert_eq!(root.node_end(), value.len());
        let entries: Vec<_> = root
            .entries(&value)
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].file_reference().record, 64);
        let name = FileName::parse(entries[0].key().unwrap()).unwrap();
        assert_eq!(name.name.to_string_lossy(), "child");
        assert!(entries[1].is_last());
        assert_eq!(entries[1].child_vcn(), Some(7));
        assert_eq!(entries[0].child_vcn(), None);

        let mut bad = value.clone();
        bad[0x20 + 8..0x20 + 10].copy_from_slice(&8u16.to_le_bytes());
        assert!(matches!(
            root.entries(&bad).unwrap().next(),
            Some(Err(FormatError::IndexEntryLength { .. }))
        ));
        assert!(root.entries(&value[..40]).unwrap().next().unwrap().is_err());
        assert!(IndexRoot::parse(&value[..31]).is_err());
        assert!(!collation::is_known(3));
        let mut misplaced = root;
        misplaced.node.entries_offset = 8;
        assert!(misplaced.entries(&value).is_err());
    }

    #[test]
    fn index_blocks_fix_up_and_expose_their_slack() {
        let key = file_name("child");
        let mut block = vec![0; 4096];
        block[0..4].copy_from_slice(b"INDX");
        block[4..6].copy_from_slice(&0x28u16.to_le_bytes());
        block[6..8].copy_from_slice(&9u16.to_le_bytes());
        block[0x10..0x18].copy_from_slice(&3u64.to_le_bytes());
        let entries = node_entries(&key);
        let entries_offset = 0x40u32; // after the update sequence array
        block[0x18..0x1c].copy_from_slice(&entries_offset.to_le_bytes());
        let used = entries_offset + u32::try_from(entries.len()).unwrap();
        block[0x1c..0x20].copy_from_slice(&used.to_le_bytes());
        block[0x20..0x24].copy_from_slice(&(4096u32 - 0x18).to_le_bytes());
        let start = 0x18 + entries_offset as usize;
        block[start..start + entries.len()].copy_from_slice(&entries);
        // Update sequence: number 0x0102, saved tails all zero.
        block[0x28..0x2a].copy_from_slice(&[1, 2]);
        for sector in 1..=8 {
            block[sector * 512 - 2..sector * 512].copy_from_slice(&[1, 2]);
        }
        let mut torn = block.clone();
        torn[4095] = 9;
        assert!(IndexBlock::fixup(&mut torn).is_err());
        let parsed = IndexBlock::fixup(&mut block).unwrap();
        assert_eq!(parsed.vcn, 3);
        assert_eq!(parsed.entries().unwrap().count(), 2);
        assert_eq!(parsed.slack().len(), 4096 - 0x18 - used as usize);
        assert_eq!(parsed.data()[510..512], [0, 0]);
    }
}
