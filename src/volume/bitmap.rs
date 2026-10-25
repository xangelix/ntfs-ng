//! `$Bitmap`: which clusters of the volume are in use.
//!
//! Pages of 4 KiB (32,768 clusters each) are read when first needed and cached (256 of them by
//! default). Clusters the bitmap cannot vouch for — past its own bytes, in a part its runs do
//! not map, or in a page that cannot be read — count as in use: a reader that checks deleted
//! data against the bitmap never takes an unknown cluster for a free one.

use std::ops::Range;

use crate::{
    Error,
    mft::{ExtentMap, Place},
    records::BITMAP,
    source::ReadAt,
    types,
    volume::Volume,
};

/// Bytes of `$Bitmap` per cached page.
pub const PAGE: u64 = 4096;
/// Clusters one page describes.
pub const PAGE_CLUSTERS: u64 = PAGE * 8;
/// Pages cached by default.
pub const PAGES: usize = 256;

/// `$Bitmap`, read through its runs, with a cache of pages.
#[derive(Debug)]
pub struct ClusterBitmap {
    map: ExtentMap,
    clusters: u64,
    cluster_size: u64,
    capacity: usize,
    pages: Vec<(u64, u64, Vec<u8>)>,
    clock: u64,
}

impl ClusterBitmap {
    /// `$Bitmap` of `volume`.
    ///
    /// # Errors
    /// If record 6 cannot be read or holds no non-resident `$DATA`.
    pub fn open<S: ReadAt>(volume: &Volume<S>) -> Result<Self, Error> {
        let mut buf = Vec::new();
        let record = volume.record(BITMAP, &mut buf)?;
        let list = volume.attribute_list(&record)?;
        let (map, sizes) = volume
            .runs(BITMAP, &record, list.as_deref(), types::DATA, &[])?
            .ok_or(Error::Missing {
                what: "$Bitmap's non-resident $DATA",
            })?;
        let bytes = sizes.data.min(sizes.initialized);
        Ok(Self {
            map,
            clusters: (volume.size() / volume.cluster_size()).min(bytes.saturating_mul(8)),
            cluster_size: volume.cluster_size(),
            capacity: PAGES,
            pages: Vec::new(),
            clock: 0,
        })
    }

    /// Clusters the bitmap vouches for.
    #[must_use]
    pub const fn clusters(&self) -> u64 {
        self.clusters
    }

    /// Forgets every cached page: the next look reads the bitmap as it is now.
    pub fn refresh(&mut self) {
        self.pages.clear();
    }

    fn page<S: ReadAt + ?Sized>(&mut self, source: &S, index: u64, fresh: bool) -> &[u8] {
        self.clock += 1;
        let clock = self.clock;
        if let Some(at) = self.pages.iter().position(|(page, ..)| *page == index) {
            if fresh {
                self.pages.swap_remove(at);
            } else if let Some(entry) = self.pages.get_mut(at) {
                entry.1 = clock;
                return self
                    .pages
                    .get(at)
                    .map_or(&[][..], |(_, _, bits)| bits.as_slice());
            }
        }
        let start = index * PAGE;
        // Unknown bits read as in use.
        let mut bits = vec![0xFF_u8; PAGE as usize];
        for (offset, len, place) in self.map.pieces(start..start + PAGE) {
            let at = usize::try_from(offset - start).unwrap_or(usize::MAX);
            let Some(slot) = bits.get_mut(at..at.saturating_add(len as usize)) else {
                continue;
            };
            if let Place::Volume(position) = place
                && crate::source::read_at(source, position, slot).is_err()
            {
                slot.fill(0xFF);
            }
        }
        if self.pages.len() >= self.capacity
            && let Some(oldest) = self
                .pages
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, used, _))| *used)
                .map(|(at, _)| at)
        {
            self.pages.swap_remove(oldest);
        }
        self.pages.push((index, clock, bits));
        self.pages
            .last()
            .map_or(&[], |(_, _, bits)| bits.as_slice())
    }

    /// The parts of the volume bytes `position..position + len` whose clusters are in use,
    /// relative to `position`, sorted and merged. `fresh` reads the pages again. Clusters past
    /// what the bitmap vouches for are in use. With `first_only`, stops at the first found.
    ///
    /// # Errors
    /// Never, today; reserved for sources that fail loudly.
    pub fn in_use<S: ReadAt + ?Sized>(
        &mut self,
        source: &S,
        position: u64,
        len: u64,
        fresh: bool,
        first_only: bool,
    ) -> Result<Vec<Range<u64>>, Error> {
        let size = self.cluster_size;
        let end = position.saturating_add(len);
        let first = position / size;
        let last = end.div_ceil(size);
        let known = last.min(self.clusters);
        let relative = |from: u64, to: u64| {
            from.saturating_mul(size).max(position) - position
                ..to.saturating_mul(size).min(end) - position
        };
        let mut used: Vec<Range<u64>> = Vec::new();
        let mut cluster = first;
        while cluster < known {
            let page = cluster / PAGE_CLUSTERS;
            let page_start = page * PAGE_CLUSTERS;
            let page_end = (page_start + PAGE_CLUSTERS).min(known);
            let bits = self.page(source, page, fresh).to_vec();
            while cluster < page_end {
                let within = cluster - page_start;
                let byte = bits.get((within / 8) as usize).copied().unwrap_or(0xFF);
                let bit = within % 8;
                if bit == 0 && cluster + 8 <= page_end && (byte == 0 || byte == 0xFF) {
                    if byte == 0xFF {
                        add(&mut used, relative(cluster, cluster + 8));
                    }
                    cluster += 8;
                } else {
                    if (byte >> bit) & 1 == 1 {
                        add(&mut used, relative(cluster, cluster + 1));
                    }
                    cluster += 1;
                }
                if first_only && !used.is_empty() {
                    return Ok(used);
                }
            }
        }
        if last > known {
            add(&mut used, relative(known.max(first), last));
        }
        Ok(used)
    }
}

/// Adds `range` to the sorted, merged `used`.
fn add(used: &mut Vec<Range<u64>>, range: Range<u64>) {
    match used.last_mut() {
        Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
        _ => used.push(range),
    }
}
