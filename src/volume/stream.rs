//! Reading non-resident streams, every hole reported.
//!
//! [`StreamReader::read`] hands a range of a stream to a sink piece by piece, in order:
//! [`Piece::Data`] for bytes read from the volume, [`Piece::Hole`] for bytes it did not read,
//! with the reason ([`HoleKind`]): a sparse run, past the initialized size, unmapped by the
//! runs, past the end of the volume, unreadable, a compression unit that does not decode,
//! clusters a deleted stream no longer owns (when checked against `$Bitmap`), or past the
//! read budget. A hole is never filled with invented bytes: what the sink writes for it is its
//! own choice.
//!
//! Compressed streams are decoded one compression unit at a time ([`crate::lznt1`]): a unit
//! entirely sparse is a hole, a unit stored whole is read as is, any other is decompressed.

use std::{io, ops::Range};

use crate::{
    lznt1,
    mft::{ExtentMap, Place},
    source::{ReadAt, read_at},
    volume::{StreamSizes, Volume, bitmap::ClusterBitmap},
};

/// Bytes read at a time.
pub const CHUNK: usize = 1 << 20;
/// The largest compression unit decoded: 64 KiB (16 clusters of 4 KiB), the most Windows
/// writes.
pub const MAX_UNIT: u64 = 64 << 10;

/// Why bytes of a stream were not read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoleKind {
    /// A sparse run: nothing is stored, the stream reads as zeros.
    Sparse,
    /// Past the initialized size: the stream reads as zeros (the stored bytes are slack).
    Uninitialized,
    /// No run maps the bytes.
    Unmapped,
    /// The runs place the bytes past the end of the volume.
    PastVolume,
    /// The volume could not be read there.
    Unreadable(io::ErrorKind),
    /// A compression unit that does not decode (or whose unit size is unsupported).
    Undecodable,
    /// Clusters that `$Bitmap` shows in use: they belong to another file now.
    Reallocated,
    /// Past the reader's byte budget.
    Budget,
}

/// A piece of a stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Piece<'a> {
    /// Bytes read.
    Data(&'a [u8]),
    /// Bytes not read, and why.
    Hole(u64, HoleKind),
}

/// Reads one non-resident stream of a volume.
#[derive(Debug)]
pub struct StreamReader<'v, S> {
    volume: &'v Volume<S>,
    map: ExtentMap,
    sizes: StreamSizes,
    budget: u64,
    buffer: Vec<u8>,
    stored: Vec<u8>,
    unit: Vec<u8>,
}

impl<'v, S: ReadAt> StreamReader<'v, S> {
    /// A reader of the stream `map` describes, whose sizes are `sizes` (from
    /// [`Volume::runs`]).
    #[must_use]
    pub const fn new(volume: &'v Volume<S>, map: ExtentMap, sizes: StreamSizes) -> Self {
        Self {
            volume,
            map,
            sizes,
            budget: u64::MAX,
            buffer: Vec::new(),
            stored: Vec::new(),
            unit: Vec::new(),
        }
    }

    /// Reads at most `bytes` from the volume; past them, [`HoleKind::Budget`].
    #[must_use]
    pub const fn budget(mut self, bytes: u64) -> Self {
        self.budget = bytes;
        self
    }

    /// The stream's sizes.
    #[must_use]
    pub const fn sizes(&self) -> &StreamSizes {
        &self.sizes
    }

    /// The stream's runs.
    #[must_use]
    pub const fn map(&self) -> &ExtentMap {
        &self.map
    }

    /// Hands `range` of the stream (clamped to its data size) to `sink`, piece by piece, in
    /// order: `sink(offset, piece)`. With `bitmap`, clusters in use (before or after each read)
    /// are holes ([`HoleKind::Reallocated`]): for deleted streams, whose clusters may belong to
    /// another file by now.
    ///
    /// # Errors
    /// The sink's first error. Reading never fails: what cannot be read is a hole.
    pub fn read<E>(
        &mut self,
        range: Range<u64>,
        mut bitmap: Option<&mut ClusterBitmap>,
        sink: &mut dyn FnMut(u64, Piece<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let end = range.end.min(self.sizes.data);
        let start = range.start.min(end);
        let compressed = self.sizes.compressed && self.sizes.compression_unit != 0;
        if compressed {
            return self.read_compressed(start..end, bitmap.as_deref_mut(), sink);
        }
        let initialized = self.sizes.initialized.clamp(start, end);
        let pieces: Vec<_> = self.map.pieces(start..initialized).collect();
        for (offset, len, place) in pieces {
            match place {
                Place::Sparse => sink(offset, Piece::Hole(len, HoleKind::Sparse))?,
                Place::Unmapped => sink(offset, Piece::Hole(len, HoleKind::Unmapped))?,
                Place::Volume(position) => {
                    self.read_volume(offset, position, len, bitmap.as_deref_mut(), sink)?;
                }
            }
        }
        if initialized < end {
            sink(
                initialized,
                Piece::Hole(end - initialized, HoleKind::Uninitialized),
            )?;
        }
        Ok(())
    }

    /// Reads `len` bytes of the stream at `offset`, stored at volume `position`.
    fn read_volume<E>(
        &mut self,
        mut offset: u64,
        mut position: u64,
        mut len: u64,
        mut bitmap: Option<&mut ClusterBitmap>,
        sink: &mut dyn FnMut(u64, Piece<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let volume_size = self.volume.size();
        while len > 0 {
            if position >= volume_size {
                return sink(offset, Piece::Hole(len, HoleKind::PastVolume));
            }
            let take = len.min(CHUNK as u64).min(volume_size - position);
            let granted = take.min(self.budget);
            if granted == 0 {
                return sink(offset, Piece::Hole(len, HoleKind::Budget));
            }
            self.budget -= granted;
            let size = usize::try_from(granted).unwrap_or(CHUNK);
            self.buffer.resize(size, 0);
            let mut used = match bitmap.as_deref_mut() {
                Some(bitmap) => bitmap
                    .in_use(self.volume.source(), position, granted, false, false)
                    .unwrap_or_else(|_| core::iter::once(0..granted).collect()),
                None => Vec::new(),
            };
            let read = read_at(self.volume.source(), position, &mut self.buffer);
            if let Some(bitmap) = bitmap.as_deref_mut() {
                // Read again after the read: clusters taken meanwhile are not this stream's.
                let after = bitmap
                    .in_use(self.volume.source(), position, granted, true, false)
                    .unwrap_or_else(|_| core::iter::once(0..granted).collect());
                used = union(&used, &after);
            }
            match read {
                Err(error) => sink(
                    offset,
                    Piece::Hole(granted, HoleKind::Unreadable(error.kind())),
                )?,
                Ok(()) => {
                    let mut at = 0u64;
                    for hole in used.iter().chain(std::iter::once(&(granted..granted))) {
                        if hole.start > at {
                            let data = self
                                .buffer
                                .get(at as usize..hole.start as usize)
                                .unwrap_or_default();
                            sink(offset + at, Piece::Data(data))?;
                        }
                        if hole.end > hole.start {
                            sink(
                                offset + hole.start,
                                Piece::Hole(hole.end - hole.start, HoleKind::Reallocated),
                            )?;
                        }
                        at = hole.end;
                    }
                }
            }
            offset += granted;
            position += granted;
            len -= granted;
        }
        Ok(())
    }

    /// Reads `range` of a compressed stream, a compression unit at a time.
    fn read_compressed<E>(
        &mut self,
        range: Range<u64>,
        mut bitmap: Option<&mut ClusterBitmap>,
        sink: &mut dyn FnMut(u64, Piece<'_>) -> Result<(), E>,
    ) -> Result<(), E> {
        let cluster = self.volume.cluster_size();
        let unit = cluster
            .checked_shl(u32::from(self.sizes.compression_unit))
            .filter(|unit| *unit <= MAX_UNIT && *unit >= cluster);
        let Some(unit) = unit else {
            return if range.is_empty() {
                Ok(())
            } else {
                sink(
                    range.start,
                    Piece::Hole(range.end - range.start, HoleKind::Undecodable),
                )
            };
        };
        let mut unit_start = range.start - range.start % unit;
        while unit_start < range.end {
            // A stream may end near `u64::MAX`: its last unit ends there.
            let unit_end = unit_start.saturating_add(unit);
            let want = range.start.max(unit_start)..range.end.min(unit_end);
            if want.start >= self.sizes.initialized {
                // Everything from here is past the initialized size: one hole.
                return sink(
                    want.start,
                    Piece::Hole(range.end - want.start, HoleKind::Uninitialized),
                );
            }
            // Whole units inside one run are handled together, however many: inside a sparse
            // or unmapped run they are one hole, inside a run on the volume they are stored as
            // they are and read straight through (past the volume or the budget, one hole).
            // The work is bounded by the runs, not by the sizes a header declares.
            if let Some((_, len, place)) = self.map.pieces(unit_start..range.end).next()
                && len >= unit
            {
                let skipped = (unit_start + len - len % unit).min(range.end);
                let end = skipped.min(self.sizes.initialized.max(want.start));
                match place {
                    Place::Volume(_) => {
                        for (offset, len, place) in
                            self.map.pieces(want.start..end).collect::<Vec<_>>()
                        {
                            if let Place::Volume(position) = place {
                                self.read_volume(
                                    offset,
                                    position,
                                    len,
                                    bitmap.as_deref_mut(),
                                    sink,
                                )?;
                            }
                        }
                    }
                    Place::Sparse | Place::Unmapped if end > want.start => {
                        let kind = if place == Place::Sparse {
                            HoleKind::Sparse
                        } else {
                            HoleKind::Unmapped
                        };
                        sink(want.start, Piece::Hole(end - want.start, kind))?;
                    }
                    Place::Sparse | Place::Unmapped => {}
                }
                if skipped > end {
                    sink(end, Piece::Hole(skipped - end, HoleKind::Uninitialized))?;
                }
                unit_start = skipped
                    .checked_next_multiple_of(unit)
                    .unwrap_or(u64::MAX)
                    .max(unit_end);
                continue;
            }
            let base = unit_start;
            unit_start = unit_end;
            let pieces: Vec<_> = self.map.pieces(base..unit_end).collect();
            if pieces.iter().all(|(_, _, place)| *place == Place::Sparse) {
                sink(
                    want.start,
                    Piece::Hole(want.end - want.start, HoleKind::Sparse),
                )?;
                continue;
            }
            if pieces.iter().any(|(_, _, place)| *place == Place::Unmapped) {
                sink(
                    want.start,
                    Piece::Hole(want.end - want.start, HoleKind::Unmapped),
                )?;
                continue;
            }
            let stored: u64 = pieces
                .iter()
                .filter(|(_, _, place)| matches!(place, Place::Volume(_)))
                .map(|(_, len, _)| *len)
                .sum();
            if stored == unit {
                // Stored whole: not compressed.
                let initialized = self.sizes.initialized.clamp(want.start, want.end);
                for (offset, len, place) in
                    self.map.pieces(want.start..initialized).collect::<Vec<_>>()
                {
                    if let Place::Volume(position) = place {
                        self.read_volume(offset, position, len, bitmap.as_deref_mut(), sink)?;
                    }
                }
                if initialized < want.end {
                    sink(
                        initialized,
                        Piece::Hole(want.end - initialized, HoleKind::Uninitialized),
                    )?;
                }
                continue;
            }
            // Compressed: the stored clusters come first, the rest of the unit is sparse.
            if stored > self.budget {
                sink(
                    want.start,
                    Piece::Hole(want.end - want.start, HoleKind::Budget),
                )?;
                continue;
            }
            self.budget -= stored;
            self.stored.clear();
            let mut failed = None;
            let mut reallocated = false;
            for (_, len, place) in &pieces {
                let Place::Volume(position) = *place else {
                    continue;
                };
                if position.saturating_add(*len) > self.volume.size() {
                    failed = Some(HoleKind::PastVolume);
                    break;
                }
                if let Some(bitmap) = bitmap.as_deref_mut()
                    && !bitmap
                        .in_use(self.volume.source(), position, *len, false, true)
                        .unwrap_or_default()
                        .is_empty()
                {
                    reallocated = true;
                }
                let at = self.stored.len();
                self.stored.resize(at + *len as usize, 0);
                if let Err(error) = read_at(
                    self.volume.source(),
                    position,
                    self.stored.get_mut(at..).unwrap_or_default(),
                ) {
                    failed = Some(HoleKind::Unreadable(error.kind()));
                    break;
                }
            }
            if reallocated {
                failed = failed.or(Some(HoleKind::Reallocated));
            }
            if let Some(kind) = failed {
                sink(want.start, Piece::Hole(want.end - want.start, kind))?;
                continue;
            }
            self.unit.clear();
            self.unit.resize(unit as usize, 0);
            if lznt1::decompress(&self.stored, &mut self.unit).is_err() {
                sink(
                    want.start,
                    Piece::Hole(want.end - want.start, HoleKind::Undecodable),
                )?;
                continue;
            }
            let initialized = self.sizes.initialized.clamp(want.start, want.end);
            if initialized > want.start {
                let data = self
                    .unit
                    .get((want.start - base) as usize..(initialized - base) as usize)
                    .unwrap_or_default();
                sink(want.start, Piece::Data(data))?;
            }
            if initialized < want.end {
                sink(
                    initialized,
                    Piece::Hole(want.end - initialized, HoleKind::Uninitialized),
                )?;
            }
        }
        Ok(())
    }
}

/// The union of two sorted, merged range lists.
fn union(a: &[Range<u64>], b: &[Range<u64>]) -> Vec<Range<u64>> {
    let mut all: Vec<Range<u64>> = a.iter().chain(b).cloned().collect();
    all.sort_by_key(|range| range.start);
    let mut out: Vec<Range<u64>> = Vec::with_capacity(all.len());
    for range in all {
        match out.last_mut() {
            Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
            _ => out.push(range),
        }
    }
    out
}
