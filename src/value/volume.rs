//! `$VOLUME_INFORMATION` (0x70).

use crate::{
    FormatError,
    bytes::{byte, le16, take},
};

/// `$VOLUME_INFORMATION` flags.
pub mod volume_flags {
    /// The volume is dirty: `chkdsk` will run.
    pub const DIRTY: u16 = 0x0001;
    /// Resize `$LogFile`.
    pub const RESIZE_LOG_FILE: u16 = 0x0002;
    /// Upgrade on mount.
    pub const UPGRADE_ON_MOUNT: u16 = 0x0004;
    /// Mounted on NT 4.
    pub const MOUNTED_ON_NT4: u16 = 0x0008;
    /// The change journal is being deleted.
    pub const DELETE_USN_UNDERWAY: u16 = 0x0010;
    /// Repair object identifiers.
    pub const REPAIR_OBJECT_ID: u16 = 0x0020;
    /// `chkdsk` is running.
    pub const CHKDSK_UNDERWAY: u16 = 0x4000;
    /// Modified by `chkdsk`.
    pub const MODIFIED_BY_CHKDSK: u16 = 0x8000;
}

/// `$VOLUME_INFORMATION` (0x70): the NTFS version and the volume's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeInformation {
    /// The major version (3 for NTFS 3.x).
    pub major: u8,
    /// The minor version (1 for NTFS 3.1).
    pub minor: u8,
    /// [`volume_flags`].
    pub flags: u16,
}

impl VolumeInformation {
    /// Read `value`.
    ///
    /// # Errors
    /// If it is shorter than 12 bytes.
    pub fn parse(value: &[u8]) -> Result<Self, FormatError> {
        let value = take(value, 0, 12, "$VOLUME_INFORMATION")?;
        Ok(Self {
            major: byte(value, 8),
            minor: byte(value, 9),
            flags: le16(value, 10),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn versions_and_flags() {
        let mut value = [0u8; 12];
        value[8] = 3;
        value[9] = 1;
        value[10..12].copy_from_slice(&volume_flags::DELETE_USN_UNDERWAY.to_le_bytes());
        let info = VolumeInformation::parse(&value).unwrap();
        assert_eq!((info.major, info.minor), (3, 1));
        assert_eq!(info.flags, volume_flags::DELETE_USN_UNDERWAY);
        assert!(VolumeInformation::parse(&value[..11]).is_err());
    }
}
