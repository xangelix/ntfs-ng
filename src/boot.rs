//! The NTFS boot sector: the volume's geometry and where `$MFT` starts.
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0x03 | 8 | OEM identifier, `NTFS    ` |
//! | 0x0B | 2 | bytes per sector |
//! | 0x0D | 1 | sectors per cluster (or, above 0x80, a negative power of two) |
//! | 0x28 | 8 | total sectors |
//! | 0x30 | 8 | `$MFT`'s first cluster |
//! | 0x38 | 8 | `$MFTMirr`'s first cluster |
//! | 0x40 | 1 | clusters per file record (or, negative, a power of two bytes) |
//! | 0x44 | 1 | clusters per index block (likewise) |
//! | 0x48 | 8 | volume serial number |
//! | 0x1FE | 2 | `55 AA` |

use crate::{
    FormatError,
    bytes::{byte, field, le16, le64, take},
    record::MAX_SIZE,
};

/// The largest cluster size accepted: 2 MiB, the most Windows formats.
pub const MAX_CLUSTER: u64 = 2 << 20;

/// A boot sector's geometry, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootSector {
    /// The OEM identifier as stored.
    pub oem_id: [u8; 8],
    /// Bytes per sector: a power of two from 256 to 4096.
    pub bytes_per_sector: u32,
    /// Bytes per cluster: a power of two, at most [`MAX_CLUSTER`].
    pub cluster_size: u64,
    /// Sectors on the volume.
    pub total_sectors: u64,
    /// `$MFT`'s first cluster.
    pub mft_lcn: u64,
    /// `$MFTMirr`'s first cluster.
    pub mft_mirror_lcn: u64,
    /// Bytes per file record: a power of two from 512 to 65536.
    pub record_size: u32,
    /// Bytes per index block: a power of two from 512 to 65536.
    pub index_block_size: u32,
    /// The volume serial number.
    pub serial_number: u64,
}

impl BootSector {
    /// The OEM identifier NTFS writes.
    pub const OEM_ID: [u8; 8] = *b"NTFS    ";

    /// Read and check the boot sector at the start of `sector` (at least 512 bytes). The OEM
    /// identifier is kept, not checked: [`Self::is_ntfs`] does.
    ///
    /// # Errors
    /// If the signature is missing or a field is out of range.
    pub fn parse(sector: &[u8]) -> Result<Self, FormatError> {
        let sector = take(sector, 0, 512, "boot sector")?;
        if field::<2>(sector, 0x1fe) != [0x55, 0xaa] {
            return Err(FormatError::BootSector { field: "signature" });
        }
        let bytes_per_sector = u32::from(le16(sector, 0x0b));
        if !(256..=4096).contains(&bytes_per_sector) || !bytes_per_sector.is_power_of_two() {
            return Err(FormatError::BootSector {
                field: "bytes per sector",
            });
        }
        let raw = byte(sector, 0x0d);
        let cluster_size = if raw <= 0x80 {
            if raw == 0 || !raw.is_power_of_two() {
                return Err(FormatError::BootSector {
                    field: "sectors per cluster",
                });
            }
            u64::from(raw) * u64::from(bytes_per_sector)
        } else {
            // A negative exponent of 2: the cluster size in bytes (for clusters past 64 KiB).
            let shift = 256 - u32::from(raw);
            if shift > 31 {
                return Err(FormatError::BootSector {
                    field: "sectors per cluster",
                });
            }
            1u64 << shift
        };
        if cluster_size > MAX_CLUSTER || cluster_size < u64::from(bytes_per_sector) {
            return Err(FormatError::BootSector {
                field: "cluster size",
            });
        }
        let total_sectors = le64(sector, 0x28);
        let mft_lcn = le64(sector, 0x30);
        if total_sectors == 0
            || mft_lcn == 0
            || mft_lcn
                .checked_mul(cluster_size)
                .is_none_or(|start| start / u64::from(bytes_per_sector) >= total_sectors)
        {
            return Err(FormatError::BootSector {
                field: "$MFT position",
            });
        }
        let record_size =
            size(byte(sector, 0x40), cluster_size).ok_or(FormatError::BootSector {
                field: "file record size",
            })?;
        let index_block_size =
            size(byte(sector, 0x44), cluster_size).ok_or(FormatError::BootSector {
                field: "index block size",
            })?;
        Ok(Self {
            oem_id: field(sector, 0x03),
            bytes_per_sector,
            cluster_size,
            total_sectors,
            mft_lcn,
            mft_mirror_lcn: le64(sector, 0x38),
            record_size,
            index_block_size,
            serial_number: le64(sector, 0x48),
        })
    }

    /// Whether the OEM identifier is NTFS's.
    #[must_use]
    pub fn is_ntfs(&self) -> bool {
        self.oem_id == Self::OEM_ID
    }

    /// The volume's size in bytes.
    #[must_use]
    pub const fn volume_size(&self) -> u64 {
        self.total_sectors
            .saturating_mul(self.bytes_per_sector as u64)
    }

    /// Clusters on the volume.
    #[must_use]
    pub const fn clusters(&self) -> u64 {
        self.volume_size() / self.cluster_size
    }

    /// Where `$MFT` starts, in bytes.
    #[must_use]
    pub const fn mft_offset(&self) -> u64 {
        self.mft_lcn.saturating_mul(self.cluster_size)
    }
}

/// A record or index block size as the boot sector stores it: a positive count of clusters,
/// or a negative power of two bytes.
fn size(raw: u8, cluster_size: u64) -> Option<u32> {
    #[allow(clippy::cast_possible_wrap)]
    let signed = raw as i8;
    let bytes = if signed > 0 {
        cluster_size.checked_mul(u64::from(raw))?
    } else {
        let shift = u32::from(signed.unsigned_abs());
        if shift > 31 {
            return None;
        }
        1u64 << shift
    };
    (bytes.is_power_of_two() && (512..=MAX_SIZE as u64).contains(&bytes))
        .then(|| u32::try_from(bytes).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{vec, vec::Vec};

    use super::*;

    pub(crate) fn sector(sectors_per_cluster: u8, record: u8) -> Vec<u8> {
        let mut boot = vec![0u8; 512];
        boot[3..11].copy_from_slice(b"NTFS    ");
        boot[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
        boot[0x0d] = sectors_per_cluster;
        boot[0x28..0x30].copy_from_slice(&(1u64 << 30).to_le_bytes());
        boot[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
        boot[0x38..0x40].copy_from_slice(&2u64.to_le_bytes());
        boot[0x40] = record;
        boot[0x44] = 1;
        boot[0x48..0x50].copy_from_slice(&0xfeed_u64.to_le_bytes());
        boot[0x1fe] = 0x55;
        boot[0x1ff] = 0xaa;
        boot
    }

    #[test]
    fn geometry_is_read_and_checked() {
        let boot = BootSector::parse(&sector(8, 0xf6)).unwrap();
        assert!(boot.is_ntfs());
        assert_eq!(boot.cluster_size, 4096);
        assert_eq!(boot.record_size, 1024);
        assert_eq!(boot.index_block_size, 4096);
        assert_eq!(boot.mft_offset(), 4 * 4096);
        assert_eq!(boot.volume_size(), 512 << 30);
        assert_eq!(boot.serial_number, 0xfeed);
        // 4 KiB records as a power of two; 2 MiB clusters as a negative exponent.
        assert_eq!(
            BootSector::parse(&sector(8, 0xf4)).unwrap().record_size,
            4096
        );
        let mut big = sector(0xeb, 0xf6);
        big[0x44] = 0xf4;
        let big = BootSector::parse(&big).unwrap();
        assert_eq!(big.cluster_size, 2 << 20);
        for (spc, record) in [
            (3, 0xf6),
            (0, 0xf6),
            (8, 0xef),
            (8, 0x00),
            (0xe0, 0xf6),
            (8, 0x20),
        ] {
            assert!(
                BootSector::parse(&sector(spc, record)).is_err(),
                "{spc} {record}"
            );
        }
        let mut unsigned = sector(8, 0xf6);
        unsigned[0x1ff] = 0;
        assert!(BootSector::parse(&unsigned).is_err());
        let mut no_mft = sector(8, 0xf6);
        no_mft[0x30..0x38].copy_from_slice(&0u64.to_le_bytes());
        assert!(BootSector::parse(&no_mft).is_err());
        let mut past_end = sector(8, 0xf6);
        past_end[0x30..0x38].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert!(BootSector::parse(&past_end).is_err());
        assert!(BootSector::parse(&sector(8, 0xf6)[..511]).is_err());
    }
}
