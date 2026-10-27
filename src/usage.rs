//! Disk usage: every name of every live file, with what a disk-usage view shows of it, and no
//! more.
//!
//! A [`Usage`] holds one [`Entry`] per name, 48 bytes each: the file's record, the parent the
//! name records, the file's size and allocated size, its creation and modification times (Unix
//! seconds), and a few flags; the names themselves, UTF-8, sit in each chunk's arena. That is
//! about a third of what a [`crate::files::FileTable`] holds per name, which keeps every time
//! and attribute.
//!
//! What is counted is what [`crate::files::FileOptions::LIVE`] keeps: records in use from
//! record 16 on (and the root), every long name, and a DOS name only when the file has no long
//! name in the same directory. A file whose record overflowed is completed from its extension
//! records in use (its first `$DATA` extent and names may live there), as the file table does.
//!
//! Build one from a scan with [`Usage::scan`]; [`Usage::parents`] then places every name in
//! its directory, and [`crate::index::group`] lists each directory's names.

use std::ops::ControlFlow;

use crate::{
    Error, FileReference,
    index::{DirId, DirectoryBatch, DirectoryIndex, IndexBuilder, NONE, PathPolicy, best_name},
    record::Record,
    records,
    scan::{Chunk, Entry as ScanEntry, Projection, Scan, join_all},
    source::ReadAt,
    time::unix_seconds_u32,
    types,
    value::{FileName, StandardInformation, tags},
};

/// What an [`Entry`]'s flags say.
pub mod usage_flags {
    /// The file is a directory.
    pub const DIRECTORY: u8 = 0x01;
    /// The file is a symbolic link or a junction.
    pub const LINK: u8 = 0x02;
    /// No unnamed `$DATA` was found: the size is the one a `$FILE_NAME` recorded (often stale),
    /// or 0.
    pub const SIZE_FROM_NAME: u8 = 0x04;
    /// The file has a reparse point (of any kind: cloud placeholders included).
    pub const REPARSE_POINT: u8 = 0x08;
    /// The unnamed `$DATA` is compressed.
    pub const COMPRESSED: u8 = 0x10;
    /// The unnamed `$DATA` is sparse.
    pub const SPARSE: u8 = 0x20;
    /// The name is in the DOS namespace alone.
    pub const DOS: u8 = 0x40;
    /// The name is a DOS alias of a long name of the file in the same directory: not counted.
    pub const ALIAS: u8 = 0x80;
}

use usage_flags as flags;

/// One name of one live file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// The file's record.
    pub record: u64,
    /// The parent the name records, as NTFS stores it (sequence in the high 16 bits).
    parent: u64,
    /// Bytes of data.
    pub size: u64,
    /// Bytes allocated (for compressed and sparse files, those actually allocated).
    pub allocated: u64,
    /// Created, in Unix seconds (0 before 1970, `u32::MAX` past 2106).
    pub created: u32,
    /// Last modified, in Unix seconds.
    pub modified: u32,
    /// The name, in its batch's arena.
    name_start: u32,
    name_len: u16,
    /// [`usage_flags`].
    pub flags: u8,
}

impl Entry {
    /// The parent the name records.
    #[must_use]
    pub const fn parent(&self) -> FileReference {
        FileReference::from_u64(self.parent)
    }

    /// Whether the file is a directory.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.flags & flags::DIRECTORY != 0
    }

    /// Whether the name is a DOS alias, not counted.
    #[must_use]
    pub const fn is_alias(&self) -> bool {
        self.flags & flags::ALIAS != 0
    }

    /// Whether the size may be wrong: no unnamed `$DATA` was found (the size is the one a
    /// `$FILE_NAME` recorded), or a reparse point left the file empty (a cloud placeholder).
    #[must_use]
    pub const fn size_untrusted(&self) -> bool {
        self.flags & flags::SIZE_FROM_NAME != 0
            || (self.size == 0 && self.flags & flags::REPARSE_POINT != 0)
    }

    fn name<'a>(&self, names: &'a str) -> &'a str {
        let start = self.name_start as usize;
        names
            .get(start..start + usize::from(self.name_len))
            .unwrap_or_default()
    }
}

/// What a file is, whichever name it is seen by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Info {
    size: u64,
    allocated: u64,
    created: u32,
    modified: u32,
    flags: u8,
}

impl Info {
    const fn entry(&self, record: u64, parent: u64, name: (u32, u16), dos: bool) -> Entry {
        Entry {
            record,
            parent,
            size: self.size,
            allocated: self.allocated,
            created: self.created,
            modified: self.modified,
            name_start: name.0,
            name_len: name.1,
            flags: self.flags | if dos { flags::DOS } else { 0 },
        }
    }
}

/// A base record with an attribute list: extension records may complete it.
#[derive(Clone, Copy, Debug)]
struct Listed {
    record: u64,
    sequence: u16,
    info: Info,
    /// Its entries in the batch.
    first: u32,
    count: u32,
}

/// What an extension record adds to its base.
#[derive(Clone, Debug)]
struct Piece {
    base: FileReference,
    /// The unnamed `$DATA`'s sizes and flags, when this record holds its first extent (or the
    /// whole resident value).
    data: Option<(u64, u64, u8)>,
    /// Names: (parent, DOS alone, start, length) in the batch's names.
    names: Vec<(u64, bool, u32, u16)>,
}

/// One chunk's entries.
#[derive(Clone, Debug, Default)]
pub struct UsageBatch {
    first: u64,
    entries: Vec<Entry>,
    names: String,
    listed: Vec<Listed>,
    pieces: Vec<Piece>,
    directories: DirectoryBatch,
}

/// Builds [`UsageBatch`]es in a scan (see [`Usage::scan`]).
#[derive(Clone, Copy, Debug)]
pub struct UsageProjection;

/// A record's names, before its DOS aliases are known: (parent, DOS alone, start, length).
type Names = Vec<(u64, bool, u32, u16)>;

impl UsageProjection {
    /// A base record in use (`number`), at record 16 or later.
    fn file(batch: &mut UsageBatch, names: &mut Names, number: u64, record: &Record<'_>) {
        let header = record.header();
        let mut info = Info {
            flags: if header.is_directory() {
                flags::DIRECTORY
            } else {
                0
            },
            ..Info::default()
        };
        let mut data_found = false;
        let mut listed = false;
        let mut name_size = 0u64;
        names.clear();
        for attribute in record.attributes() {
            let Ok(attribute) = attribute else {
                break;
            };
            match attribute.type_code() {
                types::STANDARD_INFORMATION => {
                    if let Ok(value) = attribute.value()
                        && let Some(times) = StandardInformation::times_of(value)
                    {
                        info.created = unix_seconds_u32(times.created);
                        info.modified = unix_seconds_u32(times.modified);
                    }
                }
                types::FILE_NAME => {
                    let Ok(value) = attribute.value() else {
                        continue;
                    };
                    let Ok(name) = FileName::parse(value) else {
                        continue;
                    };
                    if name_size == 0 {
                        name_size = name.data_size;
                    }
                    let start = batch.names.len();
                    name.name.push_lossy(&mut batch.names);
                    let len = u16::try_from(batch.names.len() - start).unwrap_or(u16::MAX);
                    names.push((
                        name.parent.to_u64(),
                        name.namespace.is_dos_only(),
                        start as u32,
                        len,
                    ));
                }
                types::DATA if attribute.is_unnamed() => {
                    data_found |= data(&attribute, &mut info);
                }
                types::ATTRIBUTE_LIST => listed = true,
                types::REPARSE_POINT => {
                    info.flags |= flags::REPARSE_POINT;
                    if let Ok(value) = attribute.value()
                        && let Some(tag) =
                            value.first_chunk::<4>().map(|tag| u32::from_le_bytes(*tag))
                        && (tag == tags::SYMLINK || tag == tags::MOUNT_POINT)
                    {
                        info.flags |= flags::LINK;
                    }
                }
                _ => {}
            }
        }
        if !data_found {
            info.flags |= flags::SIZE_FROM_NAME;
            if info.flags & flags::DIRECTORY == 0 {
                info.size = name_size;
            }
        }
        let first = batch.entries.len() as u32;
        for &(parent, dos, start, len) in names.iter() {
            // A DOS name is an alias of a long name in the same directory: not kept.
            if dos
                && names
                    .iter()
                    .any(|&(other, other_dos, ..)| other == parent && !other_dos)
            {
                continue;
            }
            batch
                .entries
                .push(info.entry(number, parent, (start, len), dos));
        }
        if listed {
            batch.listed.push(Listed {
                record: number,
                sequence: header.sequence,
                info,
                first,
                count: batch.entries.len() as u32 - first,
            });
        }
    }

    /// An extension record in use.
    fn extension(batch: &mut UsageBatch, record: &Record<'_>) {
        let header = record.header();
        let mut piece = Piece {
            base: header.base_record,
            data: None,
            names: Vec::new(),
        };
        for attribute in record.attributes() {
            let Ok(attribute) = attribute else {
                break;
            };
            match attribute.type_code() {
                types::DATA if attribute.is_unnamed() => {
                    let mut info = Info::default();
                    if data(&attribute, &mut info) {
                        piece.data = Some((info.size, info.allocated, info.flags));
                    }
                }
                types::FILE_NAME => {
                    if let Ok(value) = attribute.value()
                        && let Ok((parent, namespace, name)) = FileName::link(value)
                    {
                        batch.directories.add_extension_name(
                            header.base_record,
                            parent,
                            namespace,
                            name,
                        );
                        let start = batch.names.len();
                        name.push_lossy(&mut batch.names);
                        let len = u16::try_from(batch.names.len() - start).unwrap_or(u16::MAX);
                        piece.names.push((
                            parent.to_u64(),
                            namespace.is_dos_only(),
                            start as u32,
                            len,
                        ));
                    }
                }
                _ => {}
            }
        }
        if piece.data.is_some() || !piece.names.is_empty() {
            batch.pieces.push(piece);
        }
    }
}

/// Reads an unnamed `$DATA` into `info`; whether it held the stream's sizes (resident, or a
/// first extent).
#[inline]
fn data(attribute: &crate::Attribute<'_>, info: &mut Info) -> bool {
    if attribute.is_resident() {
        if let Ok(size) = attribute.data_size() {
            info.size = size;
            info.allocated = size;
            return true;
        }
        return false;
    }
    let Ok(extent) = attribute.non_resident() else {
        return false;
    };
    if attribute.is_compressed() {
        info.flags |= flags::COMPRESSED;
    }
    if attribute.is_sparse() {
        info.flags |= flags::SPARSE;
    }
    if !extent.is_first() {
        return false;
    }
    info.size = extent.data_size;
    info.allocated = extent.total_allocated.unwrap_or(extent.allocated_size);
    true
}

/// A chunk's entries are gathered in the worker's own batch, which keeps its room from chunk to
/// chunk, and handed over in a batch of exactly their size.
impl Projection for UsageProjection {
    type Output = UsageBatch;
    type Scratch = (UsageBatch, Names);

    fn scratch(&self) -> (UsageBatch, Names) {
        (UsageBatch::default(), Vec::new())
    }

    fn begin(&self, (batch, _): &mut (UsageBatch, Names), chunk: Chunk) -> UsageBatch {
        batch.first = chunk.first;
        batch.entries.clear();
        batch.entries.reserve(chunk.records);
        batch.names.clear();
        batch.names.reserve(chunk.records * 16);
        batch.listed.clear();
        batch.pieces.clear();
        UsageBatch::default()
    }

    #[inline]
    fn entry(
        &self,
        _: &mut UsageBatch,
        (batch, names): &mut (UsageBatch, Names),
        number: u64,
        entry: ScanEntry<'_>,
    ) {
        let ScanEntry::Record(record) = entry else {
            return;
        };
        let header = record.header();
        if !header.in_use() {
            return;
        }
        if !header.is_base() {
            Self::extension(batch, &record);
            return;
        }
        let keep = number >= records::FIRST_USER;
        if number == records::ROOT || (header.is_directory() && keep) {
            batch.directories.add(number, header, best_name(&record));
        }
        if keep {
            Self::file(batch, names, number, &record);
        }
    }

    fn end(&self, output: &mut UsageBatch, (batch, _): &mut (UsageBatch, Names)) {
        output.first = batch.first;
        output.entries.extend_from_slice(&batch.entries);
        output.names.push_str(&batch.names);
        output.listed.extend_from_slice(&batch.listed);
        output.pieces = core::mem::take(&mut batch.pieces);
        output.directories = core::mem::take(&mut batch.directories);
    }
}

/// Every name of every live file, and the directory index; see the module documentation.
#[derive(Debug)]
pub struct Usage {
    batches: Vec<UsageBatch>,
    /// Each batch's first entry id, and last, the number of entries in batches.
    starts: Vec<u32>,
    /// Each entry's batch.
    batch_of: Vec<u32>,
    /// Names found in extension records, after every batch's.
    extra: UsageBatch,
    /// The directories.
    pub index: DirectoryIndex,
}

impl Usage {
    /// Scans `scan`'s `$MFT` (`records` records): the entries, and the directory index under
    /// `policy`, built at once.
    ///
    /// # Errors
    /// As [`Scan::run`].
    pub fn scan<S: ReadAt>(
        scan: &Scan<'_, S>,
        records: u64,
        policy: PathPolicy,
    ) -> Result<Self, Error> {
        Self::scan_observed(scan, records, policy, &mut |_| {})
    }

    /// As [`Self::scan`], calling `observe` with each chunk as it is done (in no particular
    /// order): progress.
    ///
    /// # Errors
    /// As [`Scan::run`].
    pub fn scan_observed<S: ReadAt>(
        scan: &Scan<'_, S>,
        records: u64,
        policy: PathPolicy,
        observe: &mut dyn FnMut(Chunk),
    ) -> Result<Self, Error> {
        let mut batches = Vec::new();
        let mut builder = IndexBuilder::new();
        scan.ordered(false)
            .run(&UsageProjection, |chunk, mut batch| {
                builder.push(core::mem::take(&mut batch.directories));
                batches.push(batch);
                observe(chunk);
                ControlFlow::Continue(())
            })?;
        // The index and the table at once.
        let (index, usage) = std::thread::scope(|scope| {
            let index = scope.spawn(|| builder.build(records, policy));
            let usage = Self::assemble(batches);
            (index.join(), usage)
        });
        let index = index.map_err(|_| Error::Missing {
            what: "the directory index",
        })?;
        Ok(usage(index))
    }

    /// Orders the batches, numbers the entries, and folds the extension records in; the index
    /// comes last.
    fn assemble(mut batches: Vec<UsageBatch>) -> impl FnOnce(DirectoryIndex) -> Self {
        batches.retain(|batch| {
            !batch.entries.is_empty() || !batch.pieces.is_empty() || !batch.listed.is_empty()
        });
        batches.sort_by_key(|batch| batch.first);
        let mut starts = Vec::with_capacity(batches.len() + 1);
        let mut total = 0u32;
        for batch in &batches {
            starts.push(total);
            total += batch.entries.len() as u32;
        }
        starts.push(total);
        let mut batch_of = Vec::with_capacity(total as usize);
        for (at, batch) in batches.iter().enumerate() {
            batch_of.extend(std::iter::repeat_n(at as u32, batch.entries.len()));
        }
        let mut extra = UsageBatch::default();
        fold(&mut batches, &mut extra);
        move |index| Self {
            batches,
            starts,
            batch_of,
            extra,
            index,
        }
    }

    /// Entries in the table, aliases included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.batch_of.len() + self.extra.entries.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entry `id` and its name.
    #[must_use]
    #[inline]
    pub fn entry(&self, id: u32) -> Option<(&Entry, &str)> {
        let (batch, local) = match self.batch_of.get(id as usize) {
            Some(&batch) => {
                let batch_at = batch as usize;
                let local = id - self.starts.get(batch_at)?;
                (self.batches.get(batch_at)?, local)
            }
            None => (&self.extra, id - self.batch_of.len() as u32),
        };
        let entry = batch.entries.get(local as usize)?;
        Some((entry, entry.name(&batch.names)))
    }

    /// Every entry with its id and name, in record order (names from extension records last).
    pub fn entries(&self) -> impl Iterator<Item = (u32, &Entry, &str)> + '_ {
        self.batches
            .iter()
            .chain(std::iter::once(&self.extra))
            .flat_map(|batch| {
                batch
                    .entries
                    .iter()
                    .map(move |entry| (entry, entry.name(&batch.names)))
            })
            .enumerate()
            .map(|(id, (entry, name))| (id as u32, entry, name))
    }

    /// Each entry's directory: `parents()[id]` is entry `id`'s, or [`NONE`] when its parent
    /// does not count ([`DirectoryIndex::locate`]); aliases are [`NONE`]. Computed on
    /// `threads` threads.
    #[must_use]
    pub fn parents(&self, threads: usize) -> Vec<DirId> {
        let mut out = vec![NONE; self.len()];
        let batched = self.batch_of.len().min(out.len());
        let (mut rest, extra) = out.split_at_mut(batched);
        let per = self.batches.len().div_ceil(threads.max(1)).max(1);
        let locate = |slot: &mut DirId, entry: &Entry| {
            if !entry.is_alias() {
                *slot = self.index.locate(entry.parent()).unwrap_or(NONE);
            }
        };
        std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for group in self.batches.chunks(per) {
                let len: usize = group.iter().map(|batch| batch.entries.len()).sum();
                let available = rest.len();
                let (slots, more) = core::mem::take(&mut rest).split_at_mut(len.min(available));
                rest = more;
                workers.push(scope.spawn(move || {
                    let entries = group.iter().flat_map(|batch| batch.entries.iter());
                    for (slot, entry) in slots.iter_mut().zip(entries) {
                        locate(slot, entry);
                    }
                }));
            }
            for (slot, entry) in extra.iter_mut().zip(&self.extra.entries) {
                locate(slot, entry);
            }
            join_all(workers);
        });
        out
    }
}

/// Folds extension records into their bases (in use, at the sequence a piece names, with an
/// attribute list): a piece's `$DATA` sizes replace the base's, its names join the base's
/// (into `extra`), and DOS aliases are marked across both.
fn fold(batches: &mut [UsageBatch], extra: &mut UsageBatch) {
    let mut listed: Vec<(u64, usize, usize)> = Vec::new();
    for (at, batch) in batches.iter().enumerate() {
        for (index, file) in batch.listed.iter().enumerate() {
            listed.push((file.record, at, index));
        }
    }
    if listed.is_empty() {
        return;
    }
    listed.sort_unstable_by_key(|&(record, ..)| record);
    // The pieces of each listed file, data first.
    let mut found: Vec<(usize, usize, usize)> = Vec::new();
    for (piece_batch, batch) in batches.iter().enumerate() {
        for (piece_at, piece) in batch.pieces.iter().enumerate() {
            let at = listed.partition_point(|&(record, ..)| record < piece.base.record);
            let Some(&(record, file_batch, file_at)) = listed.get(at) else {
                continue;
            };
            let sequence = batches
                .get(file_batch)
                .and_then(|batch| batch.listed.get(file_at))
                .map(|file| file.sequence);
            if record == piece.base.record && sequence == Some(piece.base.sequence) {
                found.push((at, piece_batch, piece_at));
            }
        }
    }
    found.sort_unstable_by_key(|&(at, ..)| at);
    let mut start = 0;
    while start < found.len() {
        let at = found.get(start).map_or(0, |found| found.0);
        let end = start
            + found
                .get(start..)
                .unwrap_or_default()
                .iter()
                .take_while(|found| found.0 == at)
                .count();
        let Some(&(_, file_batch, file_at)) = listed.get(at) else {
            break;
        };
        complete(
            batches,
            extra,
            (file_batch, file_at),
            found.get(start..end).unwrap_or_default(),
        );
        start = end;
    }
}

/// Completes the listed file `file` (batch, position) with its pieces `found`.
fn complete(
    batches: &mut [UsageBatch],
    extra: &mut UsageBatch,
    file: (usize, usize),
    found: &[(usize, usize, usize)],
) {
    let Some(mut listed) = batches
        .get(file.0)
        .and_then(|batch| batch.listed.get(file.1))
        .copied()
    else {
        return;
    };
    // Sizes first: names from any piece carry the final ones.
    for &(_, piece_batch, piece_at) in found {
        if let Some((size, allocated, data_flags)) = batches
            .get(piece_batch)
            .and_then(|batch| batch.pieces.get(piece_at))
            .and_then(|piece| piece.data)
        {
            listed.info.size = size;
            listed.info.allocated = allocated;
            listed.info.flags = (listed.info.flags & !flags::SIZE_FROM_NAME) | data_flags;
        }
    }
    let extra_first = extra.entries.len();
    for &(_, piece_batch, piece_at) in found {
        let Some(batch) = batches.get(piece_batch) else {
            continue;
        };
        let Some(piece) = batch.pieces.get(piece_at) else {
            continue;
        };
        for &(parent, dos, start, len) in &piece.names {
            let name = batch
                .names
                .get(start as usize..start as usize + usize::from(len))
                .unwrap_or_default();
            let at = extra.names.len() as u32;
            extra.names.push_str(name);
            extra.entries.push(listed.info.entry(
                listed.record,
                parent,
                (at, u16::try_from(name.len()).unwrap_or(u16::MAX)),
                dos,
            ));
        }
    }
    // The base's own entries take the final sizes; then DOS aliases across both.
    let Some(batch) = batches.get_mut(file.0) else {
        return;
    };
    let own = batch
        .entries
        .get_mut(listed.first as usize..(listed.first + listed.count) as usize)
        .unwrap_or_default();
    for entry in own.iter_mut() {
        let name = (entry.name_start, entry.name_len);
        let dos = entry.flags & flags::DOS != 0;
        *entry = listed.info.entry(listed.record, entry.parent, name, dos);
    }
    let added = extra.entries.get_mut(extra_first..).unwrap_or_default();
    let long: Vec<u64> = own
        .iter()
        .chain(added.iter())
        .filter(|entry| entry.flags & flags::DOS == 0)
        .map(|entry| entry.parent)
        .collect();
    for entry in own.iter_mut().chain(added.iter_mut()) {
        if entry.flags & flags::DOS != 0 && long.contains(&entry.parent) {
            entry.flags |= flags::ALIAS;
        }
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        files::{FileOptions, Files, file_flags},
        mft::Mft,
        testing::corpus::{CorpusConfig, mft},
    };

    /// What one name shows, as either table sees it: its directory as that directory's
    /// reference (directory ids are numbered in the order a scan delivers them).
    type Seen = (
        u64,
        u64,
        String,
        u64,
        u64,
        u32,
        u32,
        u8,
        Option<FileReference>,
    );

    /// Every counted name of the table, with its directory.
    fn usage_names(usage: &Usage, threads: usize) -> BTreeMap<(u64, u64, String), Seen> {
        let parents = usage.parents(threads);
        let mut out = BTreeMap::new();
        for (id, entry, name) in usage.entries() {
            if entry.is_alias() {
                continue;
            }
            let flags = entry.flags & !(flags::DOS | flags::ALIAS);
            out.insert(
                (entry.record, entry.parent, name.to_owned()),
                (
                    entry.record,
                    entry.parent,
                    name.to_owned(),
                    entry.size,
                    entry.allocated,
                    entry.created,
                    entry.modified,
                    flags,
                    usage.index.reference(parents[id as usize]),
                ),
            );
        }
        out
    }

    fn table_names(files: &Files, threads: usize) -> BTreeMap<(u64, u64, String), Seen> {
        let parents = files.parents(threads);
        let mut out = BTreeMap::new();
        for (id, link) in files.table.links() {
            if link.alias {
                continue;
            }
            let file = files.table.file(link.file).unwrap();
            let mut flags = 0;
            for (from, to) in [
                (file_flags::DIRECTORY, flags::DIRECTORY),
                (file_flags::LINK, flags::LINK),
                (file_flags::SIZE_FROM_NAME, flags::SIZE_FROM_NAME),
                (file_flags::REPARSE_POINT, flags::REPARSE_POINT),
                (file_flags::COMPRESSED, flags::COMPRESSED),
                (file_flags::SPARSE, flags::SPARSE),
            ] {
                if file.flags & from != 0 {
                    flags |= to;
                }
            }
            let parent = link.parent.to_u64();
            out.insert(
                (file.record, parent, link.name.to_owned()),
                (
                    file.record,
                    parent,
                    link.name.to_owned(),
                    file.size,
                    file.allocated,
                    unix_seconds_u32(file.times.created),
                    unix_seconds_u32(file.times.modified),
                    flags,
                    files.index.reference(parents[id as usize]),
                ),
            );
        }
        out
    }

    /// Every name the file table counts, with the same sizes, times, flags and directory:
    /// extension records, hard links and DOS aliases included.
    #[test]
    fn usage_matches_the_file_table() {
        let config = CorpusConfig::realistic(30_000);
        let (bytes, _) = mft(&config);
        let mft = Mft::from_file(bytes.as_slice()).unwrap();
        for threads in [1, 4] {
            let scan = Scan::new(&mft).threads(threads).chunk_bytes(64 << 10);
            let usage = Usage::scan(&scan, mft.records(), PathPolicy::LIVE).unwrap();
            let files = Files::scan(&scan, mft.records(), FileOptions::LIVE).unwrap();
            let expected = table_names(&files, threads);
            let found = usage_names(&usage, threads);
            assert!(expected.len() > 10_000, "{}", expected.len());
            assert_eq!(found.len(), expected.len());
            for (key, seen) in &expected {
                assert_eq!(found.get(key), Some(seen), "{key:?}");
            }
            // Entries are reached by id as the iteration numbers them.
            for (id, entry, name) in usage.entries().step_by(97) {
                assert_eq!(usage.entry(id), Some((entry, name)));
            }
        }
    }
}
