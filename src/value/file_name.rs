//! `$FILE_NAME` (0x30).

use super::Times;
use crate::{
    FormatError,
    bytes::{byte, le32, le64, take},
    name::Name,
    record::FileReference,
};

/// A `$FILE_NAME`'s namespace: which naming rules its name follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Namespace {
    /// Case-sensitive, any character but NUL and `/`.
    Posix,
    /// A long name, Win32 rules.
    Win32,
    /// An 8.3 short name.
    Dos,
    /// A name valid in both, kept once.
    Win32AndDos,
}

impl Namespace {
    /// The namespace stored as `value`.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Posix,
            1 => Self::Win32,
            2 => Self::Dos,
            3 => Self::Win32AndDos,
            _ => return None,
        })
    }

    /// The namespace's code as stored.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Posix => 0,
            Self::Win32 => 1,
            Self::Dos => 2,
            Self::Win32AndDos => 3,
        }
    }

    /// Whether this is a DOS-only short name: an alias of a long name, never a link of its own.
    #[must_use]
    pub const fn is_dos_only(self) -> bool {
        matches!(self, Self::Dos)
    }

    /// The preference for display and paths, lower first: Win32 (or Win32-and-DOS), then
    /// POSIX, then DOS.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Win32 | Self::Win32AndDos => 0,
            Self::Posix => 1,
            Self::Dos => 2,
        }
    }
}

/// `$FILE_NAME` (0x30): one of a file's names and the directory holding it, with times NTFS
/// sets itself; also the key of each `$I30` index entry.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0x00 | 8 | parent directory reference |
/// | 0x08 | 32 | created, modified, changed, accessed |
/// | 0x28 | 8 | allocated size |
/// | 0x30 | 8 | data size |
/// | 0x38 | 4 | file attributes |
/// | 0x3C | 4 | extended attribute size, or reparse tag |
/// | 0x40 | 1 | name length, in UTF-16 code units |
/// | 0x41 | 1 | namespace |
/// | 0x42 | … | name |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileName<'a> {
    /// The directory holding the name.
    pub parent: FileReference,
    /// The times, as NTFS last updated them in this name.
    pub times: Times,
    /// Bytes allocated, as last updated in this name.
    pub allocated_size: u64,
    /// Bytes of data, as last updated in this name.
    pub data_size: u64,
    /// File attribute bits.
    pub attributes: u32,
    /// The extended attributes' size, or the reparse tag when the file has a reparse point.
    pub reparse: u32,
    /// The namespace.
    pub namespace: Namespace,
    /// The name.
    pub name: Name<'a>,
}

impl<'a> FileName<'a> {
    /// The fixed part before the name.
    pub const HEADER: usize = 0x42;

    /// Read `value`; bytes after the name (padding) are ignored.
    ///
    /// # Errors
    /// If it is shorter than its fixed part or its name, or the namespace is unknown.
    #[inline]
    pub fn parse(value: &'a [u8]) -> Result<Self, FormatError> {
        let fixed = take(value, 0, Self::HEADER, "$FILE_NAME")?;
        let name_length = byte(fixed, 0x40);
        let namespace = Namespace::from_u8(byte(fixed, 0x41)).ok_or(FormatError::Namespace {
            value: byte(fixed, 0x41),
        })?;
        let name = take(
            value,
            Self::HEADER,
            usize::from(name_length) * 2,
            "$FILE_NAME name",
        )?;
        Ok(Self {
            parent: FileReference::from_u64(le64(fixed, 0)),
            times: Times::read(fixed, 0x08),
            allocated_size: le64(fixed, 0x28),
            data_size: le64(fixed, 0x30),
            attributes: le32(fixed, 0x38),
            reparse: le32(fixed, 0x3c),
            namespace,
            name: Name::new(name),
        })
    }

    /// The parent, namespace and name alone: the cheapest read of a `$FILE_NAME` for paths.
    ///
    /// # Errors
    /// As [`Self::parse`].
    #[inline]
    pub fn link(value: &'a [u8]) -> Result<(FileReference, Namespace, Name<'a>), FormatError> {
        let fixed = take(value, 0, Self::HEADER, "$FILE_NAME")?;
        let namespace = Namespace::from_u8(byte(fixed, 0x41)).ok_or(FormatError::Namespace {
            value: byte(fixed, 0x41),
        })?;
        let name = take(
            value,
            Self::HEADER,
            usize::from(byte(fixed, 0x40)) * 2,
            "$FILE_NAME name",
        )?;
        Ok((
            FileReference::from_u64(le64(fixed, 0)),
            namespace,
            Name::new(name),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{vec, vec::Vec};

    use super::*;

    pub(crate) fn file_name(name: &str, namespace: u8) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut value = vec![0; FileName::HEADER];
        value[0..8].copy_from_slice(&(5u64 | 5 << 48).to_le_bytes());
        value[0x08..0x10].copy_from_slice(&1u64.to_le_bytes());
        value[0x28..0x30].copy_from_slice(&4096u64.to_le_bytes());
        value[0x30..0x38].copy_from_slice(&10u64.to_le_bytes());
        value[0x40] = u8::try_from(name.encode_utf16().count()).unwrap();
        value[0x41] = namespace;
        value.extend(units);
        value
    }

    #[test]
    fn file_names_read_their_name_and_namespace() {
        let value = file_name("日本語.txt", 1);
        let name = FileName::parse(&value).unwrap();
        assert_eq!(name.name.to_string_lossy(), "日本語.txt");
        assert_eq!(name.namespace, Namespace::Win32);
        assert_eq!((name.parent.record, name.parent.sequence), (5, 5));
        assert_eq!((name.allocated_size, name.data_size), (4096, 10));
        assert_eq!(name.times.created, 1);
        let (parent, namespace, link) = FileName::link(&value).unwrap();
        assert_eq!(
            (parent, namespace, link),
            (name.parent, name.namespace, name.name)
        );
        let mut padded = value.clone();
        padded.extend([0; 6]);
        assert_eq!(FileName::parse(&padded).unwrap().name, name.name);
        assert!(matches!(
            FileName::parse(&file_name("x", 4)),
            Err(FormatError::Namespace { value: 4 })
        ));
        assert!(FileName::parse(&value[..value.len() - 1]).is_err());
        assert!(FileName::parse(&value[..65]).is_err());
        assert!(Namespace::Win32AndDos.rank() < Namespace::Posix.rank());
        assert!(Namespace::Posix.rank() < Namespace::Dos.rank());
    }
}
