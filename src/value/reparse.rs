//! `$REPARSE_POINT` (0xC0).

use crate::{
    FormatError,
    bytes::{le16, le32, take},
    name::Name,
};

/// Reparse tags.
pub mod tags {
    /// A junction (mount point).
    pub const MOUNT_POINT: u32 = 0xA000_0003;
    /// A symbolic link.
    pub const SYMLINK: u32 = 0xA000_000C;
    /// Hierarchical storage management.
    pub const HSM: u32 = 0xC000_0004;
    /// Data deduplication.
    pub const DEDUP: u32 = 0x8000_0013;
    /// Network File System.
    pub const NFS: u32 = 0x8000_0014;
    /// Windows Overlay Filter: the data is compressed into a named stream
    /// (`WofCompressedData`).
    pub const WOF: u32 = 0x8000_0017;
    /// Windows Container Isolation.
    pub const WCI: u32 = 0x8000_0018;
    /// A cloud files placeholder (`CLOUD` through `CLOUD_F`: see [`is_cloud`]).
    pub const CLOUD: u32 = 0x9000_001A;
    /// An app execution alias.
    pub const APPEXECLINK: u32 = 0x8000_001B;
    /// Projected file system.
    pub const PROJFS: u32 = 0x9000_001C;
    /// A WSL symbolic link.
    pub const LX_SYMLINK: u32 = 0xA000_001D;
    /// `OneDrive`.
    pub const ONEDRIVE: u32 = 0x8000_0021;
    /// A Unix domain socket.
    pub const AF_UNIX: u32 = 0x8000_0023;

    /// Whether `tag` is one of the 16 cloud files tags.
    #[must_use]
    pub const fn is_cloud(tag: u32) -> bool {
        tag & 0xFFFF_0FFF == CLOUD
    }

    /// Whether `tag` is Microsoft's (its data has no GUID).
    #[must_use]
    pub const fn is_microsoft(tag: u32) -> bool {
        tag & 0x8000_0000 != 0
    }

    /// Whether `tag` names another file (a symbolic link, a junction).
    #[must_use]
    pub const fn is_name_surrogate(tag: u32) -> bool {
        tag & 0x2000_0000 != 0
    }
}

/// A reparse point: its tag and data.
///
/// | Offset | Size | Field |
/// |---|---|---|
/// | 0x00 | 4 | tag ([`tags`]) |
/// | 0x04 | 2 | data length |
/// | 0x06 | 2 | reserved |
/// | 0x08 | 16 | a GUID, for tags that are not Microsoft's |
/// | … | … | data |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReparsePoint<'a> {
    /// The tag.
    pub tag: u32,
    /// The GUID of a tag that is not Microsoft's.
    pub guid: Option<[u8; 16]>,
    /// The data, as long as declared (clamped to the value).
    pub data: &'a [u8],
}

/// A symbolic link's or junction's target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkTarget<'a> {
    /// The name the file system resolves (`\??\C:\target`).
    pub substitute: Name<'a>,
    /// The name shown.
    pub print: Name<'a>,
    /// Whether a symbolic link's target is relative to the link.
    pub relative: bool,
}

impl<'a> ReparsePoint<'a> {
    /// Read `value`.
    ///
    /// # Errors
    /// If it is shorter than its header (and GUID).
    pub fn parse(value: &'a [u8]) -> Result<Self, FormatError> {
        let header = take(value, 0, 8, "reparse point")?;
        let tag = le32(header, 0);
        let length = usize::from(le16(header, 4));
        let (guid, start) = if tags::is_microsoft(tag) {
            (None, 8)
        } else {
            (
                Some(crate::bytes::field(take(value, 8, 16, "reparse GUID")?, 0)),
                24,
            )
        };
        let rest = value.get(start..).unwrap_or_default();
        Ok(Self {
            tag,
            guid,
            data: rest.get(..length).unwrap_or(rest),
        })
    }

    /// The target of a symbolic link or junction; `None` for other tags.
    ///
    /// # Errors
    /// If the data is shorter than its layout or its names.
    pub fn link_target(&self) -> Option<Result<LinkTarget<'a>, FormatError>> {
        // A symbolic link's names follow its flags; a junction has none.
        let (names_at, flags_at) = match self.tag {
            tags::SYMLINK => (12, Some(8)),
            tags::MOUNT_POINT => (8, None),
            _ => return None,
        };
        Some(self.names(names_at).and_then(|(substitute, print)| {
            let relative = match flags_at {
                Some(at) => take(self.data, at, 4, "symbolic link flags")
                    .map_err(|_| FormatError::ReparseData { what: "flags" })
                    .map(|flags| le32(flags, 0) & 1 != 0)?,
                None => false,
            };
            Ok(LinkTarget {
                substitute,
                print,
                relative,
            })
        }))
    }

    fn names(&self, buffer_at: usize) -> Result<(Name<'a>, Name<'a>), FormatError> {
        let offsets = take(self.data, 0, 8, "reparse names")
            .map_err(|_| FormatError::ReparseData { what: "names" })?;
        let buffer = self.data.get(buffer_at..).unwrap_or_default();
        let name = |offset: usize, length: usize| {
            take(buffer, offset, length, "reparse name")
                .map(Name::new)
                .map_err(|_| FormatError::ReparseData { what: "names" })
        };
        Ok((
            name(usize::from(le16(offsets, 0)), usize::from(le16(offsets, 2)))?,
            name(usize::from(le16(offsets, 4)), usize::from(le16(offsets, 6)))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{vec, vec::Vec};

    use super::*;

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    #[test]
    fn links_and_junctions_name_their_targets() {
        let substitute = utf16("\\??\\C:\\target");
        let print = utf16("C:\\target");
        let mut data = vec![0; 12];
        data[0..2].copy_from_slice(&0u16.to_le_bytes());
        data[2..4].copy_from_slice(&u16::try_from(substitute.len()).unwrap().to_le_bytes());
        data[4..6].copy_from_slice(&u16::try_from(substitute.len()).unwrap().to_le_bytes());
        data[6..8].copy_from_slice(&u16::try_from(print.len()).unwrap().to_le_bytes());
        data[8..12].copy_from_slice(&1u32.to_le_bytes());
        data.extend(&substitute);
        data.extend(&print);
        let mut value = vec![0; 8];
        value[0..4].copy_from_slice(&tags::SYMLINK.to_le_bytes());
        value[4..6].copy_from_slice(&u16::try_from(data.len()).unwrap().to_le_bytes());
        value.extend(&data);
        let point = ReparsePoint::parse(&value).unwrap();
        let target = point.link_target().unwrap().unwrap();
        assert_eq!(target.substitute.to_string_lossy(), "\\??\\C:\\target");
        assert_eq!(target.print.to_string_lossy(), "C:\\target");
        assert!(target.relative);

        let mut short = value.clone();
        short.truncate(20);
        short[4..6].copy_from_slice(&12u16.to_le_bytes());
        assert!(
            ReparsePoint::parse(&short)
                .unwrap()
                .link_target()
                .unwrap()
                .is_err()
        );

        let mut wof = vec![0; 8];
        wof[0..4].copy_from_slice(&tags::WOF.to_le_bytes());
        assert!(ReparsePoint::parse(&wof).unwrap().link_target().is_none());
        assert!(tags::is_cloud(0x9000_701A));
        assert!(!tags::is_cloud(tags::WOF));

        let mut third_party = vec![0; 24];
        third_party[0..4].copy_from_slice(&0x0000_1234u32.to_le_bytes());
        third_party[8] = 0xAB;
        assert_eq!(
            ReparsePoint::parse(&third_party).unwrap().guid.unwrap()[0],
            0xAB
        );
        assert!(ReparsePoint::parse(&third_party[..20]).is_err());
    }
}
