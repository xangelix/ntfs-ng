//! The values of the attributes a `FILE` record describes itself with. Each reads a resident
//! value (or, for index entries, an index node) in place. Times are FILETIMEs, 100-nanosecond
//! intervals since 1601-01-01 UTC, as stored ([`crate::time`] converts them).

mod attribute_list;
mod file_name;
mod index;
mod object_id;
mod reparse;
mod standard_information;
mod volume;

pub use attribute_list::{AttributeList, AttributeListEntry};
pub use file_name::{FileName, Namespace};
pub use index::{
    IndexBlock, IndexEntries, IndexEntry, IndexNodeHeader, IndexRoot, collation, index_entry_flags,
};
pub use object_id::ObjectId;
pub use reparse::{LinkTarget, ReparsePoint, tags};
pub use standard_information::{StandardInformation, StandardInformationExtended, file_attributes};
pub use volume::{VolumeInformation, volume_flags};

use crate::bytes::le64;

/// The four times NTFS keeps per file, as FILETIMEs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Times {
    /// Created.
    pub created: u64,
    /// Content last modified.
    pub modified: u64,
    /// The record itself last changed ("MFT modified", "changed").
    pub changed: u64,
    /// Last accessed.
    pub accessed: u64,
}

impl Times {
    /// The 32 bytes at `at` of `data`, which the caller has checked are there.
    pub(crate) fn read(data: &[u8], at: usize) -> Self {
        Self {
            created: le64(data, at),
            modified: le64(data, at + 8),
            changed: le64(data, at + 16),
            accessed: le64(data, at + 24),
        }
    }

    /// The times in order: created, modified, changed, accessed.
    #[must_use]
    pub const fn all(&self) -> [u64; 4] {
        [self.created, self.modified, self.changed, self.accessed]
    }
}
