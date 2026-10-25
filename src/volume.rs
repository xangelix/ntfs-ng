//! An NTFS volume: its boot sector, `$MFT`, records, attribute lists, streams and `$Bitmap`.
//!
//! A [`Volume`] reads a raw volume, a shadow copy device or an image through any [`ReadAt`].
//! It reads exactly what it needs: one record is one aligned read of the record's own bytes
//! (never a large buffered read for a small record), an attribute list is read once and
//! bounded, and a stream is read through its runs with every hole reported, never invented
//! (see [`crate::volume::stream`]).
//!
//! Extension records are followed only where they are the base record's own: in use, with the
//! sequence number the attribute-list entry expects, and naming the base record (number and
//! sequence) as their base.

pub mod bitmap;
pub mod stream;

use crate::{
    Error, FormatError,
    attribute::Attribute,
    boot::BootSector,
    mft::{ExtentMap, MAX_LIST_BYTES, Mft, read_non_resident},
    record::{FileReference, Record},
    source::{ReadAt, read_at},
    types,
    value::{AttributeList, AttributeListEntry, VolumeInformation},
};

/// An NTFS volume.
#[derive(Debug)]
pub struct Volume<S> {
    boot: BootSector,
    mft: Mft<S>,
}

impl<S: ReadAt> Volume<S> {
    /// Opens the volume `source`: reads its boot sector and maps `$MFT`.
    ///
    /// # Errors
    /// If the boot sector is not NTFS's, or `$MFT` cannot be mapped.
    pub fn open(source: S) -> Result<Self, Error> {
        let mut sector = [0u8; 512];
        read_at(&source, 0, &mut sector)?;
        let boot = BootSector::parse(&sector)?;
        if !boot.is_ntfs() {
            return Err(FormatError::BootSector {
                field: "OEM identifier",
            }
            .into());
        }
        let mft = Mft::from_volume(source, &boot)?;
        Ok(Self { boot, mft })
    }

    /// The boot sector.
    #[must_use]
    pub const fn boot(&self) -> &BootSector {
        &self.boot
    }

    /// `$MFT`.
    #[must_use]
    pub const fn mft(&self) -> &Mft<S> {
        &self.mft
    }

    /// The source.
    #[must_use]
    pub const fn source(&self) -> &S {
        self.mft.source()
    }

    /// Bytes per cluster.
    #[must_use]
    pub const fn cluster_size(&self) -> u64 {
        self.boot.cluster_size
    }

    /// The volume's size in bytes.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.boot.volume_size()
    }

    /// Record `number`, read, fixed up and checked into `buf`.
    ///
    /// # Errors
    /// As [`Mft::record`].
    pub fn record<'b>(&self, number: u64, buf: &'b mut Vec<u8>) -> Result<Record<'b>, Error> {
        self.mft.record(number, buf)
    }

    /// `$Volume`'s `$VOLUME_INFORMATION`.
    ///
    /// # Errors
    /// If `$Volume` cannot be read or has none.
    pub fn information(&self) -> Result<VolumeInformation, Error> {
        let mut buf = Vec::new();
        let record = self.record(crate::records::VOLUME, &mut buf)?;
        let attribute = record
            .find(types::VOLUME_INFORMATION)
            .ok_or(Error::Missing {
                what: "$VOLUME_INFORMATION",
            })?;
        Ok(VolumeInformation::parse(attribute.value()?)?)
    }

    /// `$Volume`'s label (empty when it has none).
    ///
    /// # Errors
    /// If `$Volume` cannot be read.
    pub fn label(&self) -> Result<String, Error> {
        let mut buf = Vec::new();
        let record = self.record(crate::records::VOLUME, &mut buf)?;
        Ok(match record.find(types::VOLUME_NAME) {
            Some(attribute) => crate::name::Name::new(attribute.value()?).to_string_lossy(),
            None => String::new(),
        })
    }

    /// The value of `attribute`, an attribute list, as bytes: resident, or read through its
    /// runs (at most 256 KiB, never past the volume).
    ///
    /// # Errors
    /// If the attribute is invalid or its clusters cannot be read.
    pub fn attribute_list_bytes(&self, attribute: &Attribute<'_>) -> Result<Vec<u8>, Error> {
        match attribute.form()? {
            crate::Form::Resident(value) => Ok(value
                .get(..usize::try_from(MAX_LIST_BYTES).unwrap_or(usize::MAX))
                .unwrap_or(value)
                .to_vec()),
            crate::Form::NonResident(extent) => read_non_resident(
                self.source(),
                &extent,
                self.cluster_size(),
                MAX_LIST_BYTES,
                self.size(),
            ),
        }
    }

    /// The attribute list of `record`, read once, if it has one.
    ///
    /// # Errors
    /// If the list cannot be read.
    pub fn attribute_list(&self, record: &Record<'_>) -> Result<Option<Vec<u8>>, Error> {
        record
            .find(types::ATTRIBUTE_LIST)
            .map(|attribute| self.attribute_list_bytes(&attribute))
            .transpose()
    }

    /// Reads the extension record `entry` names into `buf`, if it is `base`'s own: in use, at
    /// the entry's sequence, naming `base` as its base.
    ///
    /// # Errors
    /// [`Error::Missing`] when the record is not `base`'s extension record; read errors.
    pub fn extension<'b>(
        &self,
        base: FileReference,
        entry: &AttributeListEntry<'_>,
        buf: &'b mut Vec<u8>,
    ) -> Result<Record<'b>, Error> {
        let record = self.record(entry.segment.record, buf)?;
        let header = record.header();
        if header.in_use()
            && header.sequence == entry.segment.sequence
            && header.base_record == base
        {
            Ok(record)
        } else {
            Err(Error::Missing {
                what: "an extension record of this file",
            })
        }
    }

    /// The runs of every extent of the non-resident attribute `type_code` named `name` of the
    /// base record `record` (`number`): its own extents, and, through its attribute list
    /// (`list`, as read by [`Self::attribute_list`]), those in its extension records. Returns
    /// the map and the first extent's sizes (`data`, `initialized`, `allocated`, compression
    /// unit), or `None` when the record has no such non-resident attribute.
    ///
    /// # Errors
    /// If an extent's runs are invalid.
    pub fn runs(
        &self,
        number: u64,
        record: &Record<'_>,
        list: Option<&[u8]>,
        type_code: u32,
        name: &[u16],
    ) -> Result<Option<(ExtentMap, StreamSizes)>, Error> {
        let mut map = ExtentMap::default();
        let mut sizes = None;
        let matches = |attribute: &Attribute<'_>| {
            attribute.type_code() == type_code
                && attribute.name().is_ok_and(|own| own.eq_units(name))
        };
        let cluster = self.cluster_size();
        let mut add = |attribute: &Attribute<'_>, map: &mut ExtentMap| -> Result<(), Error> {
            let extent = attribute.non_resident()?;
            if extent.is_first() && sizes.is_none() {
                sizes = Some(StreamSizes {
                    allocated: extent.allocated_size,
                    data: extent.data_size,
                    initialized: extent.initialized_size,
                    compression_unit: extent.compression_unit,
                    compressed: attribute.is_compressed(),
                    sparse: attribute.is_sparse(),
                });
            }
            map.add_runs(extent.runs(), cluster)
        };
        for attribute in record.attributes() {
            let attribute = attribute?;
            if matches(&attribute) {
                if attribute.is_resident() {
                    return Ok(None);
                }
                add(&attribute, &mut map)?;
            }
        }
        if let Some(list) = list {
            let base = record.reference(number);
            let mut buf = Vec::new();
            for entry in AttributeList::new(list) {
                let entry = entry?;
                if entry.attribute_type != type_code
                    || entry.segment.record == number
                    || !entry.name.eq_units(name)
                {
                    continue;
                }
                let Ok(extension) = self.extension(base, &entry, &mut buf) else {
                    continue;
                };
                let found = extension
                    .attributes()
                    .map_while(Result::ok)
                    .find(|attribute| {
                        attribute.instance() == entry.attribute_id && matches(attribute)
                    });
                if let Some(attribute) = found
                    && attribute
                        .non_resident()
                        .is_ok_and(|extent| extent.lowest_vcn == entry.lowest_vcn)
                {
                    add(&attribute, &mut map)?;
                }
            }
        }
        map.finish();
        Ok(sizes.map(|sizes| (map, sizes)))
    }
}

/// A non-resident stream's sizes, from its first extent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StreamSizes {
    /// Bytes allocated.
    pub allocated: u64,
    /// Bytes of data.
    pub data: u64,
    /// Bytes initialized: past them the stream reads as zeros.
    pub initialized: u64,
    /// Clusters per compression unit, as a power of two; 0 uncompressed.
    pub compression_unit: u16,
    /// Whether the stream is compressed.
    pub compressed: bool,
    /// Whether the stream is sparse.
    pub sparse: bool,
}
