//! Byte-exact builders of `FILE` records and attributes.

use crate::{
    record::{FileReference, SECTOR, flags},
    types,
    value::{Namespace, Times},
};

/// The UTF-16LE bytes of `text`.
#[must_use]
pub fn utf16(text: &str) -> Vec<u8> {
    text.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// A resident attribute record: `type_code`, identifier `instance`, named `name` (UTF-16 code
/// units as bytes), holding `value`, with `attribute_flags`.
#[must_use]
pub fn resident(
    type_code: u32,
    instance: u16,
    name: &[u8],
    value: &[u8],
    attribute_flags: u16,
) -> Vec<u8> {
    let name_at = 0x18;
    let value_at = (name_at + name.len()).next_multiple_of(8);
    let length = (value_at + value.len()).next_multiple_of(8);
    let mut out = vec![0u8; length];
    put(&mut out, 0, &type_code.to_le_bytes());
    put(&mut out, 4, &u32_of(length).to_le_bytes());
    put(&mut out, 9, &[u8_of(name.len() / 2)]);
    put(&mut out, 0x0a, &u16_of(name_at).to_le_bytes());
    put(&mut out, 0x0c, &attribute_flags.to_le_bytes());
    put(&mut out, 0x0e, &instance.to_le_bytes());
    put(&mut out, 0x10, &u32_of(value.len()).to_le_bytes());
    put(&mut out, 0x14, &u16_of(value_at).to_le_bytes());
    if type_code == types::FILE_NAME {
        put(&mut out, 0x16, &[1]);
    }
    put(&mut out, name_at, name);
    put(&mut out, value_at, value);
    out
}

/// A non-resident attribute's layout.
#[derive(Clone, Debug, Default)]
pub struct NonResidentSpec {
    /// The first VCN this extent maps.
    pub lowest_vcn: u64,
    /// Runs: `(clusters, Some(LCN))` stored, `(clusters, None)` sparse.
    pub runs: Vec<(u64, Option<u64>)>,
    /// Bytes allocated (first extent only; 0 elsewhere).
    pub allocated: u64,
    /// Bytes of data.
    pub data: u64,
    /// Bytes initialized.
    pub initialized: u64,
    /// Compression unit exponent (4 for LZNT1).
    pub compression_unit: u16,
    /// Attribute flags.
    pub flags: u16,
    /// Bytes actually allocated, for compressed and sparse streams.
    pub total_allocated: Option<u64>,
}

/// Encodes runs as mapping pairs, ending with the zero byte.
#[must_use]
pub fn mapping_pairs(runs: &[(u64, Option<u64>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut previous: i64 = 0;
    for &(length, lcn) in runs {
        let length_bytes = unsigned_bytes(length);
        match lcn {
            Some(lcn) => {
                let lcn = i64::try_from(lcn).unwrap_or(i64::MAX);
                let delta = lcn - previous;
                previous = lcn;
                let delta_bytes = signed_bytes(delta);
                out.push(u8_of(length_bytes.len()) | (u8_of(delta_bytes.len()) << 4));
                out.extend(length_bytes);
                out.extend(delta_bytes);
            }
            None => {
                out.push(u8_of(length_bytes.len()));
                out.extend(length_bytes);
            }
        }
    }
    out.push(0);
    out
}

fn unsigned_bytes(value: u64) -> Vec<u8> {
    let bytes = value.to_le_bytes();
    let len = (8 - value.leading_zeros() as usize / 8).max(1);
    bytes.get(..len).unwrap_or(&bytes).to_vec()
}

fn signed_bytes(value: i64) -> Vec<u8> {
    let bytes = value.to_le_bytes();
    for len in 1..=8 {
        let shift = 64 - 8 * len;
        if (value << shift) >> shift == value {
            return bytes.get(..len).unwrap_or(&bytes).to_vec();
        }
    }
    bytes.to_vec()
}

/// A non-resident attribute record.
#[must_use]
pub fn non_resident(type_code: u32, instance: u16, name: &[u8], spec: &NonResidentSpec) -> Vec<u8> {
    let header = if spec.compression_unit != 0 || spec.total_allocated.is_some() {
        0x48
    } else {
        0x40
    };
    let name_at = header;
    let pairs_at = (name_at + name.len()).next_multiple_of(8);
    let pairs = mapping_pairs(&spec.runs);
    let length = (pairs_at + pairs.len()).next_multiple_of(8);
    let clusters: u64 = spec.runs.iter().map(|(length, _)| *length).sum();
    let highest = if clusters == 0 {
        u64::MAX
    } else {
        spec.lowest_vcn + clusters - 1
    };
    let mut out = vec![0u8; length];
    put(&mut out, 0, &type_code.to_le_bytes());
    put(&mut out, 4, &u32_of(length).to_le_bytes());
    put(&mut out, 8, &[1]);
    put(&mut out, 9, &[u8_of(name.len() / 2)]);
    put(&mut out, 0x0a, &u16_of(name_at).to_le_bytes());
    put(&mut out, 0x0c, &spec.flags.to_le_bytes());
    put(&mut out, 0x0e, &instance.to_le_bytes());
    put(&mut out, 0x10, &spec.lowest_vcn.to_le_bytes());
    put(&mut out, 0x18, &highest.to_le_bytes());
    put(&mut out, 0x20, &u16_of(pairs_at).to_le_bytes());
    put(&mut out, 0x22, &spec.compression_unit.to_le_bytes());
    put(&mut out, 0x28, &spec.allocated.to_le_bytes());
    put(&mut out, 0x30, &spec.data.to_le_bytes());
    put(&mut out, 0x38, &spec.initialized.to_le_bytes());
    if header == 0x48 {
        put(
            &mut out,
            0x40,
            &spec.total_allocated.unwrap_or(spec.allocated).to_le_bytes(),
        );
    }
    put(&mut out, name_at, name);
    put(&mut out, pairs_at, &pairs);
    out
}

/// A `$STANDARD_INFORMATION` value (72 bytes).
#[must_use]
pub fn standard_information(times: Times, file_attributes: u32) -> Vec<u8> {
    let mut value = vec![0u8; 72];
    for (at, time) in times.all().iter().enumerate() {
        put(&mut value, at * 8, &time.to_le_bytes());
    }
    put(&mut value, 0x20, &file_attributes.to_le_bytes());
    value
}

/// A `$FILE_NAME` value.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn file_name(
    parent: FileReference,
    namespace: Namespace,
    name: &str,
    times: Times,
    allocated: u64,
    data: u64,
    file_attributes: u32,
) -> Vec<u8> {
    let units = utf16(name);
    let mut value = vec![0u8; 0x42];
    put(&mut value, 0, &parent.to_u64().to_le_bytes());
    for (at, time) in times.all().iter().enumerate() {
        put(&mut value, 8 + at * 8, &time.to_le_bytes());
    }
    put(&mut value, 0x28, &allocated.to_le_bytes());
    put(&mut value, 0x30, &data.to_le_bytes());
    put(&mut value, 0x38, &file_attributes.to_le_bytes());
    put(
        &mut value,
        0x40,
        &[u8_of(units.len() / 2), namespace.to_u8()],
    );
    value.extend(units);
    value
}

/// An `$ATTRIBUTE_LIST` entry.
#[must_use]
pub fn attribute_list_entry(
    type_code: u32,
    name: &[u8],
    lowest_vcn: u64,
    segment: FileReference,
    instance: u16,
) -> Vec<u8> {
    let length = (0x1a + name.len()).next_multiple_of(8);
    let mut entry = vec![0u8; length];
    put(&mut entry, 0, &type_code.to_le_bytes());
    put(&mut entry, 4, &u16_of(length).to_le_bytes());
    put(&mut entry, 6, &[u8_of(name.len() / 2), 0x1a]);
    put(&mut entry, 8, &lowest_vcn.to_le_bytes());
    put(&mut entry, 0x10, &segment.to_u64().to_le_bytes());
    put(&mut entry, 0x18, &instance.to_le_bytes());
    put(&mut entry, 0x1a, name);
    entry
}

/// A `FILE` record under construction.
#[derive(Clone, Debug)]
pub struct RecordBuilder {
    size: usize,
    number: u64,
    sequence: u16,
    flags: u16,
    base: FileReference,
    lsn: u64,
    link_count: u16,
    usn: u16,
    attributes: Vec<Vec<u8>>,
    next_instance: u16,
}

impl RecordBuilder {
    /// Record `number`: 1 KiB, sequence 1, in use.
    #[must_use]
    pub const fn new(number: u64) -> Self {
        Self {
            size: 1024,
            number,
            sequence: 1,
            flags: flags::IN_USE,
            base: FileReference::new(0, 0),
            lsn: 0,
            link_count: 0,
            usn: 0x0102,
            attributes: Vec::new(),
            next_instance: 0,
        }
    }

    /// The record size (a power of two from 512).
    #[must_use]
    pub const fn size(mut self, size: usize) -> Self {
        self.size = size;
        self
    }

    /// The sequence number.
    #[must_use]
    pub const fn sequence(mut self, sequence: u16) -> Self {
        self.sequence = sequence;
        self
    }

    /// The header flags.
    #[must_use]
    pub const fn flags(mut self, flags: u16) -> Self {
        self.flags = flags;
        self
    }

    /// In use or not.
    #[must_use]
    pub const fn in_use(mut self, in_use: bool) -> Self {
        if in_use {
            self.flags |= flags::IN_USE;
        } else {
            self.flags &= !flags::IN_USE;
        }
        self
    }

    /// A directory or not.
    #[must_use]
    pub const fn directory(mut self, directory: bool) -> Self {
        if directory {
            self.flags |= flags::DIRECTORY;
        } else {
            self.flags &= !flags::DIRECTORY;
        }
        self
    }

    /// An extension record of `base`.
    #[must_use]
    pub const fn base(mut self, base: FileReference) -> Self {
        self.base = base;
        self
    }

    /// The `$LogFile` sequence number.
    #[must_use]
    pub const fn lsn(mut self, lsn: u64) -> Self {
        self.lsn = lsn;
        self
    }

    /// The hard link count.
    #[must_use]
    pub const fn links(mut self, count: u16) -> Self {
        self.link_count = count;
        self
    }

    /// The update sequence number every stride ends with while stored.
    #[must_use]
    pub const fn update_sequence_number(mut self, usn: u16) -> Self {
        self.usn = usn;
        self
    }

    /// The next attribute identifier.
    #[must_use]
    pub const fn next_instance(&self) -> u16 {
        self.next_instance
    }

    /// Adds a raw attribute record.
    #[must_use]
    pub fn attribute(mut self, bytes: Vec<u8>) -> Self {
        self.attributes.push(bytes);
        self.next_instance = self.next_instance.wrapping_add(1);
        self
    }

    /// Replaces the attribute added with identifier `instance` (a placeholder) with `bytes`.
    #[must_use]
    pub fn replace(mut self, instance: u16, bytes: Vec<u8>) -> Self {
        if let Some(slot) = self.attributes.get_mut(usize::from(instance)) {
            *slot = bytes;
        }
        self
    }

    /// Adds a resident attribute.
    #[must_use]
    pub fn resident(self, type_code: u32, name: &str, value: &[u8]) -> Self {
        let instance = self.next_instance;
        self.attribute(resident(type_code, instance, &utf16(name), value, 0))
    }

    /// Adds a non-resident attribute.
    #[must_use]
    pub fn non_resident(self, type_code: u32, name: &str, spec: &NonResidentSpec) -> Self {
        let instance = self.next_instance;
        self.attribute(non_resident(type_code, instance, &utf16(name), spec))
    }

    /// Adds `$STANDARD_INFORMATION`.
    #[must_use]
    pub fn standard_information(self, times: Times, file_attributes: u32) -> Self {
        self.resident(
            types::STANDARD_INFORMATION,
            "",
            &standard_information(times, file_attributes),
        )
    }

    /// Adds a `$FILE_NAME`.
    #[must_use]
    pub fn file_name(
        self,
        parent: FileReference,
        namespace: Namespace,
        name: &str,
        times: Times,
        data: u64,
    ) -> Self {
        let allocated = data.next_multiple_of(4096);
        self.resident(
            types::FILE_NAME,
            "",
            &file_name(parent, namespace, name, times, allocated, data, 0),
        )
    }

    /// The bytes in use once built, or `None` when the attributes overflow the record.
    #[must_use]
    pub fn used(&self) -> Option<usize> {
        let first = first_attribute(self.size);
        let used = first + self.attributes.iter().map(Vec::len).sum::<usize>() + 8;
        (used <= self.size).then_some(used)
    }

    /// The record as stored, update sequence applied; `None` when the attributes overflow it.
    #[must_use]
    pub fn try_build(&self) -> Option<Vec<u8>> {
        let used = self.used()?;
        let mut out = vec![0u8; self.size];
        let count = self.size / SECTOR + 1;
        let first = first_attribute(self.size);
        put(&mut out, 0, b"FILE");
        put(&mut out, 4, &0x30u16.to_le_bytes());
        put(&mut out, 6, &u16_of(count).to_le_bytes());
        put(&mut out, 8, &self.lsn.to_le_bytes());
        put(&mut out, 0x10, &self.sequence.to_le_bytes());
        put(&mut out, 0x12, &self.link_count.to_le_bytes());
        put(&mut out, 0x14, &u16_of(first).to_le_bytes());
        put(&mut out, 0x16, &self.flags.to_le_bytes());
        put(&mut out, 0x18, &u32_of(used).to_le_bytes());
        put(&mut out, 0x1c, &u32_of(self.size).to_le_bytes());
        put(&mut out, 0x20, &self.base.to_u64().to_le_bytes());
        put(&mut out, 0x28, &self.next_instance.to_le_bytes());
        #[allow(clippy::cast_possible_truncation)]
        put(&mut out, 0x2c, &(self.number as u32).to_le_bytes());
        let mut at = first;
        for attribute in &self.attributes {
            put(&mut out, at, attribute);
            at += attribute.len();
        }
        put(&mut out, at, &u32::MAX.to_le_bytes());
        protect(&mut out, 0x30, self.usn);
        Some(out)
    }

    /// The record as stored.
    ///
    /// # Panics
    /// When the attributes overflow the record (a bug in the test building it).
    #[must_use]
    #[allow(clippy::expect_used)]
    pub fn build(&self) -> Vec<u8> {
        self.try_build().expect("the attributes fit the record")
    }
}

/// Where the first attribute starts in a record of `size` bytes (after an NTFS 3.1 header and
/// its update sequence array).
#[must_use]
pub const fn first_attribute(size: usize) -> usize {
    (0x30 + 2 * (size / SECTOR + 1)).next_multiple_of(8)
}

/// Stores `usn` at the end of every stride of `buf`, saving the bytes it replaces in the
/// update sequence array at `offset`.
pub fn protect(buf: &mut [u8], offset: usize, usn: u16) {
    put(buf, offset, &usn.to_le_bytes());
    for sector in 1..=buf.len() / SECTOR {
        let end = sector * SECTOR - 2;
        let saved = [
            buf.get(end).copied().unwrap_or(0),
            buf.get(end + 1).copied().unwrap_or(0),
        ];
        put(buf, offset + sector * 2, &saved);
        put(buf, end, &usn.to_le_bytes());
    }
}

/// Copies `bytes` into `buf` at `at` (as much as fits).
pub(crate) fn put(buf: &mut [u8], at: usize, bytes: &[u8]) {
    if let Some(slot) = buf.get_mut(at..at.saturating_add(bytes.len())) {
        slot.copy_from_slice(bytes);
    }
}

pub(crate) fn u8_of(value: usize) -> u8 {
    u8::try_from(value).unwrap_or(u8::MAX)
}

pub(crate) fn u16_of(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

pub(crate) fn u32_of(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;
    use crate::{Record, runs::MappingPairs, value::FileName};

    #[test]
    fn built_records_parse_back() {
        let parent = FileReference::new(5, 5);
        let times = Times {
            created: 1,
            modified: 2,
            changed: 3,
            accessed: 4,
        };
        for size in [512, 1024, 4096] {
            let raw = RecordBuilder::new(77)
                .size(size)
                .sequence(9)
                .standard_information(times, 0x20)
                .file_name(parent, Namespace::Win32, "hello.txt", times, 10)
                .resident(types::DATA, "", b"0123456789")
                .non_resident(
                    types::DATA,
                    "ads",
                    &NonResidentSpec {
                        runs: vec![(2, Some(100)), (3, None), (1, Some(40))],
                        allocated: 6 * 4096,
                        data: 20_000,
                        initialized: 20_000,
                        ..NonResidentSpec::default()
                    },
                )
                .build();
            let mut buf = raw.clone();
            let record = Record::fixup(&mut buf).unwrap();
            assert_eq!(record.sequence(), 9);
            assert!(record.check_number(77).is_ok());
            let attributes: Vec<_> = record.attributes().collect::<Result<_, _>>().unwrap();
            assert_eq!(attributes.len(), 4);
            let name = FileName::parse(attributes[1].value().unwrap()).unwrap();
            assert_eq!(name.name.to_string_lossy(), "hello.txt");
            assert_eq!(name.parent, parent);
            assert_eq!(attributes[2].value().unwrap(), b"0123456789");
            let extent = attributes[3].non_resident().unwrap();
            assert_eq!(attributes[3].name().unwrap().to_string_lossy(), "ads");
            let runs: Vec<_> = extent.runs().collect::<Result<_, _>>().unwrap();
            assert_eq!(runs.len(), 3);
            assert_eq!(runs[2].lcn, Some(40));
            assert_eq!(extent.clusters(), 6);
            let mut stored = Vec::new();
            record.raw_into(&mut stored);
            assert_eq!(stored, raw);
        }
    }

    #[test]
    fn mapping_pairs_round_trip() {
        let runs = [
            (1, Some(0x1234_5678)),
            (300, None),
            (70_000, Some(5)),
            (2, Some(0x7fff_ffff_ffff)),
        ];
        let pairs = mapping_pairs(&runs);
        let decoded: Vec<_> = MappingPairs::new(&pairs, 0, 0)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(decoded.len(), 4);
        for (run, (length, lcn)) in decoded.iter().zip(runs) {
            assert_eq!((run.length, run.lcn), (length, lcn));
        }
    }
}
