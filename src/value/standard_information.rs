//! `$STANDARD_INFORMATION` (0x10).

use super::Times;
use crate::{
    FormatError,
    bytes::{le32, le64},
};

/// `FILE_ATTRIBUTE_*` bits, as `$STANDARD_INFORMATION` and `$FILE_NAME` store them.
pub mod file_attributes {
    /// Read-only.
    pub const READONLY: u32 = 0x0000_0001;
    /// Hidden.
    pub const HIDDEN: u32 = 0x0000_0002;
    /// System.
    pub const SYSTEM: u32 = 0x0000_0004;
    /// A directory (`$FILE_NAME` only; NTFS's own bit for an `$I30` index).
    pub const DIRECTORY: u32 = 0x1000_0000;
    /// Archive.
    pub const ARCHIVE: u32 = 0x0000_0020;
    /// Device.
    pub const DEVICE: u32 = 0x0000_0040;
    /// Normal.
    pub const NORMAL: u32 = 0x0000_0080;
    /// Temporary.
    pub const TEMPORARY: u32 = 0x0000_0100;
    /// Sparse.
    pub const SPARSE_FILE: u32 = 0x0000_0200;
    /// Has a reparse point.
    pub const REPARSE_POINT: u32 = 0x0000_0400;
    /// Compressed.
    pub const COMPRESSED: u32 = 0x0000_0800;
    /// Offline: the data is elsewhere (a cloud placeholder, an HSM).
    pub const OFFLINE: u32 = 0x0000_1000;
    /// Not content indexed.
    pub const NOT_CONTENT_INDEXED: u32 = 0x0000_2000;
    /// Encrypted (EFS).
    pub const ENCRYPTED: u32 = 0x0000_4000;
    /// Integrity stream (ReFS; reserved on NTFS).
    pub const INTEGRITY_STREAM: u32 = 0x0000_8000;
    /// Virtual.
    pub const VIRTUAL: u32 = 0x0001_0000;
    /// No scrub data.
    pub const NO_SCRUB_DATA: u32 = 0x0002_0000;
    /// Extended attributes present (on `$FILE_NAME`: the `reparse` field holds the EA size).
    pub const EA: u32 = 0x0004_0000;
    /// Pinned (cloud files: keep local).
    pub const PINNED: u32 = 0x0008_0000;
    /// Unpinned (cloud files: may dehydrate).
    pub const UNPINNED: u32 = 0x0010_0000;
    /// Recall on open (cloud placeholder).
    pub const RECALL_ON_OPEN: u32 = 0x0004_0000;
    /// Recall on data access (cloud placeholder).
    pub const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    /// A view index (NTFS's own bit: `$Secure`, `$ObjId`, `$Quota`, `$Reparse`).
    pub const INDEX_VIEW: u32 = 0x2000_0000;
}

/// `$STANDARD_INFORMATION` (0x10): the times and attributes programs see.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0x00 | 32 | created, modified, changed, accessed |
/// | 0x20 | 4 | file attributes ([`file_attributes`]) |
/// | 0x24 | 4 | maximum number of versions |
/// | 0x28 | 4 | version number |
/// | 0x2C | 4 | class identifier |
/// | 0x30 | 4 | owner identifier (NTFS 3.0 and later, as are the rest) |
/// | 0x34 | 4 | security identifier, a key into `$Secure` |
/// | 0x38 | 8 | quota charged |
/// | 0x40 | 8 | update sequence number (USN) of the last change |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StandardInformation {
    /// The times.
    pub times: Times,
    /// [`file_attributes`] bits, every one as stored.
    pub attributes: u32,
    /// Maximum number of versions.
    pub maximum_versions: u32,
    /// Version number.
    pub version: u32,
    /// Class identifier.
    pub class_id: u32,
    /// The fields NTFS 3.0 added; `None` in the 48-byte layout.
    pub extended: Option<StandardInformationExtended>,
}

/// The fields of the 72-byte `$STANDARD_INFORMATION`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StandardInformationExtended {
    /// Owner identifier (quotas).
    pub owner_id: u32,
    /// Security identifier, a key into `$Secure`.
    pub security_id: u32,
    /// Quota charged.
    pub quota_charged: u64,
    /// USN of the last change.
    pub usn: u64,
}

impl StandardInformation {
    /// The 48-byte layout of NTFS 1.2.
    pub const SHORT: usize = 48;
    /// The 72-byte layout of NTFS 3.0 and later.
    pub const LONG: usize = 72;

    /// Read `value`. A value longer than 72 bytes is read as the 72-byte layout.
    ///
    /// # Errors
    /// If it is neither 48 nor at least 72 bytes long.
    pub fn parse(value: &[u8]) -> Result<Self, FormatError> {
        if value.len() != Self::SHORT && value.len() < Self::LONG {
            return Err(FormatError::StandardInformationSize { len: value.len() });
        }
        Ok(Self {
            times: Times::read(value, 0),
            attributes: le32(value, 0x20),
            maximum_versions: le32(value, 0x24),
            version: le32(value, 0x28),
            class_id: le32(value, 0x2c),
            extended: (value.len() >= Self::LONG).then(|| StandardInformationExtended {
                owner_id: le32(value, 0x30),
                security_id: le32(value, 0x34),
                quota_charged: le64(value, 0x38),
                usn: le64(value, 0x40),
            }),
        })
    }

    /// The times alone, from a value at least 32 bytes long: the cheapest read of a value whose
    /// layout is otherwise not checked.
    #[must_use]
    pub fn times_of(value: &[u8]) -> Option<Times> {
        (value.len() >= 32).then(|| Times::read(value, 0))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::vec;

    use super::*;

    #[test]
    fn standard_information_reads_both_layouts() {
        let mut long = vec![0; 72];
        long[0x20..0x24].copy_from_slice(&0x20u32.to_le_bytes());
        long[0x40..0x48].copy_from_slice(&99u64.to_le_bytes());
        let info = StandardInformation::parse(&long).unwrap();
        assert_eq!(info.attributes, 0x20);
        assert_eq!(info.extended.unwrap().usn, 99);
        let short = StandardInformation::parse(&long[..48]).unwrap();
        assert_eq!(short.extended, None);
        assert_eq!(
            StandardInformation::parse(&[0; 80])
                .unwrap()
                .extended
                .unwrap()
                .usn,
            0
        );
        for len in [0, 47, 49, 71] {
            assert!(StandardInformation::parse(&long[..len]).is_err(), "{len}");
        }
        assert!(StandardInformation::times_of(&long[..31]).is_none());
        assert!(StandardInformation::times_of(&long[..32]).is_some());
    }
}
