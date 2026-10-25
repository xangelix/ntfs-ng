//! `$MFT` as a space of records: a standalone dump, or a volume's `$MFT` mapped through every
//! extent of its `$DATA`.
//!
//! On a volume, `$MFT` describes itself: record 0's unnamed `$DATA` holds the first extents,
//! and when `$MFT` is fragmented enough, its `$ATTRIBUTE_LIST` names further extents held in
//! extension records, which may themselves lie in parts of `$MFT` only those extents map.
//! [`Mft::from_volume`] maps them all, pass by pass, following an entry only into a record that
//! is in use, has the entry's sequence number and names record 0 as its base. A record no
//! extent maps is reported ([`Mft::problems`]) and read as [`crate::Error::Unmapped`], never as
//! another record's bytes.

use std::ops::Range;

use crate::{
    Error, FormatError,
    attribute::NonResident,
    boot::BootSector,
    record::{FileReference, Record},
    runs::Run,
    source::{ReadAt, Segment, read_at},
    types,
    value::AttributeList,
};

/// Most bytes of an attribute list read: NTFS never lets a list grow past 256 KiB.
pub const MAX_LIST_BYTES: u64 = 256 << 10;
/// Most passes over `$MFT`'s attribute list: each maps the extension records the previous
/// passes made reachable.
const MAX_LIST_PASSES: usize = 64;
/// Most extents a map holds (a stream fragmented further is refused).
pub const MAX_EXTENTS: usize = 1 << 22;

/// One extent of a stream: `len` bytes from `offset` in the stream, at `place` on the volume
/// (`None` for a sparse run, which reads as zeros).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    /// Where the extent starts in the stream.
    pub offset: u64,
    /// Its length.
    pub len: u64,
    /// Where it is on the volume; `None` when sparse.
    pub place: Option<u64>,
}

/// Where a piece of a stream is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// At this volume offset.
    Volume(u64),
    /// Sparse: zeros, nothing stored.
    Sparse,
    /// No extent maps it.
    Unmapped,
}

/// A stream's extents, sorted and without overlaps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtentMap {
    extents: Vec<Extent>,
    sorted: bool,
}

impl ExtentMap {
    /// A map of a stream stored contiguously from offset 0 (a file read as it is).
    #[must_use]
    pub fn identity(len: u64) -> Self {
        Self {
            extents: vec![Extent {
                offset: 0,
                len,
                place: Some(0),
            }],
            sorted: true,
        }
    }

    /// Adds the runs of one attribute extent, in clusters of `cluster_size` bytes.
    ///
    /// # Errors
    /// If a run is invalid, overflows, or the map holds too many extents.
    pub fn add_runs(
        &mut self,
        runs: impl Iterator<Item = Result<Run, FormatError>>,
        cluster_size: u64,
    ) -> Result<(), Error> {
        for run in runs {
            let run = run?;
            let overflow = || {
                Error::Format(FormatError::LcnRange {
                    at: 0,
                    lcn: i64::try_from(run.lcn.unwrap_or(0)).unwrap_or(i64::MAX),
                    length: run.length,
                })
            };
            let offset = run.vcn.checked_mul(cluster_size).ok_or_else(overflow)?;
            let len = run.length.checked_mul(cluster_size).ok_or_else(overflow)?;
            offset.checked_add(len).ok_or_else(overflow)?;
            let place = match run.lcn {
                Some(lcn) => {
                    let place = lcn.checked_mul(cluster_size).ok_or_else(overflow)?;
                    place.checked_add(len).ok_or_else(overflow)?;
                    Some(place)
                }
                None => None,
            };
            if self.extents.len() >= MAX_EXTENTS {
                return Err(Error::Limit {
                    what: "extents",
                    limit: MAX_EXTENTS as u64,
                });
            }
            self.extents.push(Extent { offset, len, place });
        }
        self.sorted = false;
        Ok(())
    }

    /// Sorts the extents and drops overlaps (the first extent added for a range wins).
    pub fn finish(&mut self) {
        if self.sorted {
            return;
        }
        self.extents.sort_by_key(|extent| extent.offset);
        let mut kept: Vec<Extent> = Vec::with_capacity(self.extents.len());
        for extent in self.extents.drain(..) {
            match kept.last() {
                Some(last) if extent.offset < last.offset + last.len => {}
                _ => kept.push(extent),
            }
        }
        self.extents = kept;
        self.sorted = true;
    }

    /// Merges another map's extents into this one.
    pub fn merge(&mut self, other: Self) {
        self.extents.extend(other.extents);
        self.sorted = false;
        self.finish();
    }

    /// The extents, sorted.
    #[must_use]
    pub fn extents(&self) -> &[Extent] {
        &self.extents
    }

    /// Where the bytes of `range` are, piece by piece, gaps included ([`Place::Unmapped`]).
    #[must_use]
    pub fn pieces(&self, range: Range<u64>) -> Pieces<'_> {
        let index = self
            .extents
            .partition_point(|extent| extent.offset + extent.len <= range.start);
        Pieces {
            extents: &self.extents,
            index,
            at: range.start,
            end: range.end,
        }
    }

    /// Whether every byte of `range` is mapped (stored or sparse).
    #[must_use]
    pub fn covers(&self, range: Range<u64>) -> bool {
        self.pieces(range)
            .all(|(_, _, place)| !matches!(place, Place::Unmapped))
    }
}

/// The pieces of a range of a stream: `(offset in the stream, length, place)`.
#[derive(Clone, Debug)]
pub struct Pieces<'a> {
    extents: &'a [Extent],
    index: usize,
    at: u64,
    end: u64,
}

impl Iterator for Pieces<'_> {
    type Item = (u64, u64, Place);

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.end {
            return None;
        }
        while let Some(extent) = self.extents.get(self.index) {
            if extent.offset + extent.len > self.at {
                break;
            }
            self.index += 1;
        }
        let at = self.at;
        let item = match self.extents.get(self.index) {
            Some(extent) if extent.offset <= at => {
                let len = (extent.offset + extent.len).min(self.end) - at;
                let place = extent.place.map_or(Place::Sparse, |place| {
                    Place::Volume(place + (at - extent.offset))
                });
                (at, len, place)
            }
            next => {
                let until = next.map_or(self.end, |extent| extent.offset.min(self.end));
                (at, until - at, Place::Unmapped)
            }
        };
        self.at += item.1;
        Some(item)
    }
}

/// `$MFT`: its records, where they are, and what could not be mapped.
#[derive(Debug)]
pub struct Mft<S> {
    source: S,
    record_size: u32,
    size: u64,
    map: ExtentMap,
    problems: Vec<String>,
}

impl<S: ReadAt> Mft<S> {
    /// A standalone `$MFT` (an exported dump): records stored contiguously from offset 0, their
    /// size taken from record 0's header.
    ///
    /// # Errors
    /// If record 0 cannot be read or does not state a usable record size.
    pub fn from_file(source: S) -> Result<Self, Error> {
        let mut head = [0u8; 48];
        read_at(&source, 0, &mut head)?;
        if head.first_chunk::<4>() != Some(b"FILE") {
            return Err(FormatError::Signature {
                found: crate::bytes::field(&head, 0),
            }
            .into());
        }
        let record_size = u32::from_le_bytes(crate::bytes::field(&head, 0x1c));
        Self::from_file_with_record_size(source, record_size)
    }

    /// A standalone `$MFT` whose records are `record_size` bytes (when record 0 cannot tell).
    ///
    /// # Errors
    /// If `record_size` is not a power of two from 512 to 65536.
    pub fn from_file_with_record_size(source: S, record_size: u32) -> Result<Self, Error> {
        check_record_size(record_size)?;
        let records = source.len() / u64::from(record_size);
        let size = records * u64::from(record_size);
        Ok(Self::from_map(
            source,
            record_size,
            size,
            ExtentMap::identity(size),
        ))
    }

    /// `$MFT` of the volume `source`, whose boot sector is `boot`: mapped through every extent
    /// of its `$DATA`, those in extension records included.
    ///
    /// # Errors
    /// If record 0 cannot be read or holds no first extent of `$DATA`.
    pub fn from_volume(source: S, boot: &BootSector) -> Result<Self, Error> {
        let record_size = boot.record_size;
        check_record_size(record_size)?;
        let cluster = boot.cluster_size;
        let mut zero = vec![0u8; record_size as usize];
        read_at(&source, boot.mft_offset(), &mut zero)?;
        let record = Record::fixup(&mut zero)?;
        record.check_number(0)?;
        let base = record.reference(0);
        let mut map = ExtentMap::default();
        let mut size = None;
        let mut list = None;
        for attribute in record.attributes() {
            let attribute = attribute?;
            match attribute.type_code() {
                types::DATA if attribute.is_unnamed() => {
                    let extent = attribute.non_resident()?;
                    if extent.is_first() && size.is_none() {
                        size = Some(extent.data_size);
                    }
                    map.add_runs(extent.runs(), cluster)?;
                }
                types::ATTRIBUTE_LIST if list.is_none() => list = Some(attribute),
                _ => {}
            }
        }
        let size = size.ok_or(Error::Missing {
            what: "$MFT's first extent",
        })?;
        map.finish();
        let list = match list {
            Some(attribute) => Some(match attribute.form()? {
                crate::Form::Resident(value) => value.to_vec(),
                crate::Form::NonResident(extent) => read_non_resident(
                    &source,
                    &extent,
                    cluster,
                    MAX_LIST_BYTES,
                    boot.volume_size(),
                )?,
            }),
            None => None,
        };
        let mut mft = Self::from_map(source, record_size, size, map);
        if let Some(list) = list {
            mft.follow_list(&list, base, cluster)?;
        }
        for (offset, len, place) in mft.map.pieces(0..size) {
            if matches!(place, Place::Unmapped) {
                let end = offset + len;
                mft.problems.push(format!(
                    "bytes {offset}..{end} (records {}..{}) are not mapped by the runs of $MFT",
                    offset / u64::from(record_size),
                    end.div_ceil(u64::from(record_size))
                ));
            }
        }
        Ok(mft)
    }

    /// Maps the extents `$MFT`'s attribute list places in extension records: pass by pass, as
    /// each pass maps the parts of `$MFT` where the next extension records are.
    fn follow_list(&mut self, list: &[u8], base: FileReference, cluster: u64) -> Result<(), Error> {
        let mut pending = Vec::new();
        for entry in AttributeList::new(list) {
            match entry {
                Ok(entry)
                    if entry.attribute_type == types::DATA
                        && entry.name.is_empty()
                        && entry.segment.record != 0 =>
                {
                    pending.push((entry.segment, entry.attribute_id, entry.lowest_vcn));
                }
                Ok(_) => {}
                Err(error) => {
                    self.problems
                        .push(format!("$MFT's attribute list: {error}"));
                    break;
                }
            }
        }
        let mut buffer = Vec::new();
        for _ in 0..MAX_LIST_PASSES {
            let before = pending.len();
            let mut found = ExtentMap::default();
            let mut kept = Vec::new();
            for (segment, instance, lowest_vcn) in pending {
                let record_range = self.range(segment.record);
                if !record_range.is_some_and(|range| self.map.covers(range)) {
                    kept.push((segment, instance, lowest_vcn));
                    continue;
                }
                let record = match self.record(segment.record, &mut buffer) {
                    Ok(record) => record,
                    Err(error) => {
                        self.problems.push(format!(
                            "$MFT's extension record {} cannot be read: {error}",
                            segment.record
                        ));
                        continue;
                    }
                };
                let header = record.header();
                if !header.in_use()
                    || header.sequence != segment.sequence
                    || header.base_record != base
                {
                    self.problems.push(format!(
                        "$MFT's attribute list names record {}, which is not its extension record",
                        segment.record
                    ));
                    continue;
                }
                let extent = record
                    .attributes()
                    .map_while(Result::ok)
                    .find(|attribute| {
                        attribute.type_code() == types::DATA
                            && attribute.instance() == instance
                            && attribute.is_unnamed()
                    })
                    .map(|attribute| attribute.non_resident())
                    .transpose()?
                    .filter(|extent| extent.lowest_vcn == lowest_vcn);
                match extent {
                    Some(extent) => found.add_runs(extent.runs(), cluster)?,
                    None => self.problems.push(format!(
                        "$MFT's extension record {} holds no extent its list entry names",
                        segment.record
                    )),
                }
            }
            self.map.merge(found);
            pending = kept;
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        for (segment, ..) in pending {
            self.problems.push(format!(
                "an extent of $MFT in record {} lies in a part of $MFT no other extent maps",
                segment.record
            ));
        }
        Ok(())
    }

    /// `$MFT` with `size` bytes of `record_size`-byte records, mapped by `map`.
    #[must_use]
    pub fn from_map(source: S, record_size: u32, size: u64, mut map: ExtentMap) -> Self {
        map.finish();
        Self {
            source,
            record_size,
            size,
            map,
            problems: Vec::new(),
        }
    }

    /// The source.
    #[must_use]
    pub const fn source(&self) -> &S {
        &self.source
    }

    /// Bytes per record.
    #[must_use]
    pub const fn record_size(&self) -> u32 {
        self.record_size
    }

    /// Bytes of `$MFT`.
    #[must_use]
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Records in `$MFT`.
    #[must_use]
    pub const fn records(&self) -> u64 {
        self.size / self.record_size as u64
    }

    /// The map of `$MFT`'s extents.
    #[must_use]
    pub const fn map(&self) -> &ExtentMap {
        &self.map
    }

    /// What could not be mapped.
    #[must_use]
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// The bytes of record `number` within `$MFT`.
    fn range(&self, number: u64) -> Option<Range<u64>> {
        let start = number.checked_mul(u64::from(self.record_size))?;
        let end = start.checked_add(u64::from(self.record_size))?;
        (end <= self.size).then_some(start..end)
    }

    /// Where record `number` starts on the source, when mapped.
    #[must_use]
    pub fn position(&self, number: u64) -> Option<u64> {
        match self.map.pieces(self.range(number)?).next()? {
            (_, _, Place::Volume(position)) => Some(position),
            _ => None,
        }
    }

    /// Reads record `number` as stored into `buf` (resized to the record size).
    ///
    /// # Errors
    /// [`Error::Unmapped`] when no extent maps it (or it is past `$MFT`); I/O errors.
    pub fn read_record(&self, number: u64, buf: &mut Vec<u8>) -> Result<(), Error> {
        let range = self
            .range(number)
            .ok_or(Error::Unmapped { record: number })?;
        buf.resize(self.record_size as usize, 0);
        let start = range.start;
        for (offset, len, place) in self.map.pieces(range) {
            let at = usize::try_from(offset - start).unwrap_or(usize::MAX);
            let Some(slot) = buf.get_mut(at..at.saturating_add(len as usize)) else {
                return Err(Error::Unmapped { record: number });
            };
            match place {
                Place::Volume(position) => read_holey(&self.source, position, slot)?,
                Place::Sparse => slot.fill(0),
                Place::Unmapped => return Err(Error::Unmapped { record: number }),
            }
        }
        Ok(())
    }

    /// Reads record `number` into `buf`, applies its update sequence and checks the number it
    /// states.
    ///
    /// # Errors
    /// As [`Self::read_record`]; [`Error::Format`] when the record is refused.
    pub fn record<'b>(&self, number: u64, buf: &'b mut Vec<u8>) -> Result<Record<'b>, Error> {
        self.read_record(number, buf)?;
        let record = Record::fixup(buf)?;
        record.check_number(number)?;
        Ok(record)
    }
}

fn check_record_size(record_size: u32) -> Result<(), Error> {
    if record_size.is_power_of_two() && (512..=65536).contains(&record_size) {
        Ok(())
    } else {
        Err(FormatError::RecordSize {
            len: record_size as usize,
        }
        .into())
    }
}

/// Reads `buf.len()` bytes at `position`, zero-filling the holes the source reports instead of
/// reading them.
pub(crate) fn read_holey<S: ReadAt + ?Sized>(
    source: &S,
    position: u64,
    buf: &mut [u8],
) -> Result<(), Error> {
    let mut done = 0usize;
    while done < buf.len() {
        let at = position + done as u64;
        let rest = buf.len() - done;
        let (len, hole) = match source.segment(at)? {
            Segment::Data(len) => (len, false),
            Segment::Hole(len) => (len, true),
        };
        // A source that reports nothing more is read for the rest: the read errors if it ends.
        let take = usize::try_from(len).map_or(rest, |len| len.min(rest));
        let take = if take == 0 { rest } else { take };
        let Some(slot) = buf.get_mut(done..done + take) else {
            break;
        };
        if hole {
            slot.fill(0);
        } else {
            read_at(source, at, slot)?;
        }
        done += take;
    }
    Ok(())
}

/// Reads a non-resident value through its runs, at most `cap` bytes and never past
/// `volume_size`; sparse runs and bytes past the initialized size read as zeros.
pub(crate) fn read_non_resident<S: ReadAt + ?Sized>(
    source: &S,
    extent: &NonResident<'_>,
    cluster: u64,
    cap: u64,
    volume_size: u64,
) -> Result<Vec<u8>, Error> {
    let size = extent.data_size.min(cap);
    let initialized = extent.initialized_size.min(size);
    let mut map = ExtentMap::default();
    map.add_runs(extent.runs(), cluster)?;
    map.finish();
    let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(0)];
    for (offset, len, place) in map.pieces(0..initialized) {
        let at = usize::try_from(offset).unwrap_or(usize::MAX);
        let Some(slot) = bytes.get_mut(at..at.saturating_add(len as usize)) else {
            break;
        };
        match place {
            Place::Volume(position) => {
                if position.saturating_add(len) > volume_size {
                    return Err(Error::Missing {
                        what: "clusters inside the volume",
                    });
                }
                read_holey(source, position, slot)?;
            }
            Place::Sparse => {}
            Place::Unmapped => {
                return Err(Error::Missing {
                    what: "clusters for every byte of the value",
                });
            }
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn pieces_cover_ranges_with_gaps_and_sparse_runs() {
        let mut map = ExtentMap::default();
        let runs = [
            Run {
                vcn: 0,
                length: 2,
                lcn: Some(100),
            },
            Run {
                vcn: 4,
                length: 1,
                lcn: None,
            },
            Run {
                vcn: 5,
                length: 2,
                lcn: Some(50),
            },
        ];
        map.add_runs(runs.into_iter().map(Ok), 10).unwrap();
        map.finish();
        let pieces: Vec<_> = map.pieces(5..75).collect();
        assert_eq!(
            pieces,
            [
                (5, 15, Place::Volume(1005)),
                (20, 20, Place::Unmapped),
                (40, 10, Place::Sparse),
                (50, 20, Place::Volume(500)),
                (70, 5, Place::Unmapped),
            ]
        );
        assert!(map.covers(0..20));
        assert!(!map.covers(0..21));
        // An overlapping extent added later loses.
        map.merge({
            let mut other = ExtentMap::default();
            other
                .add_runs(
                    [Run {
                        vcn: 1,
                        length: 4,
                        lcn: Some(7),
                    }]
                    .into_iter()
                    .map(Ok),
                    10,
                )
                .unwrap();
            other
        });
        assert_eq!(map.extents().len(), 3);
    }

    #[test]
    fn holey_reads_skip_holes() {
        struct Holey(Vec<u8>);
        impl ReadAt for Holey {
            fn len(&self) -> u64 {
                self.0.len() as u64
            }
            fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
                assert!(
                    offset >= 16 || offset + buf.len() as u64 <= 8,
                    "a hole was read"
                );
                self.0.read_exact_at(offset, buf)
            }
            fn segment(&self, offset: u64) -> std::io::Result<Segment> {
                Ok(match offset {
                    0..8 => Segment::Data(8 - offset),
                    8..16 => Segment::Hole(16 - offset),
                    _ => Segment::Data(32u64.saturating_sub(offset)),
                })
            }
        }
        let source = Holey((0..32).collect());
        let mut buf = vec![0xee; 28];
        read_holey(&source, 2, &mut buf).unwrap();
        assert_eq!(&buf[..6], &[2, 3, 4, 5, 6, 7]);
        assert!(buf[6..14].iter().all(|b| *b == 0));
        assert_eq!(&buf[14..], &(16..30).collect::<Vec<u8>>()[..]);
    }
}
