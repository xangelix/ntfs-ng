//! Files, not records: every base record with its names and sizes, extension records folded in.
//!
//! A file whose record overflowed keeps attributes in extension records: names (`$FILE_NAME`)
//! and the first extent of its `$DATA` may live there, the base record holding only an
//! `$ATTRIBUTE_LIST`. A [`FileTable`] is what a scan of `$MFT` means at the level of files:
//! one [`File`] per base record (its standard times, sizes and flags), one [`Link`] per name
//! (DOS aliases included and marked), and both completed from the file's extension records —
//! names held there become links, a `$DATA` held there gives the size.
//!
//! An extension record is folded in only when it is in use and names, as its base, a base
//! record in use at its current sequence: a freed extension record still names its base but no
//! longer holds what the file has, and a deleted file's extension records may belong to another
//! file by now (deleted files report what their base record holds).
//!
//! [`FileProjection`] builds the table's pieces in a parallel scan (with a [`DirectoryBatch`]
//! of the directories, for a [`DirectoryIndex`]); [`FileTable::build`] assembles them in
//! O(files + links). Names are decoded once, to UTF-8 (U+FFFD for unpaired surrogates).

use std::ops::ControlFlow;

use crate::{
    Error,
    index::{DirId, DirectoryBatch, DirectoryIndex, IndexBuilder, NONE, PathPolicy, best_name},
    record::{FileReference, Record},
    records,
    scan::{Chunk, Entry, Projection, Scan, join_all},
    source::ReadAt,
    types,
    value::{FileName, Namespace, StandardInformation, Times, tags},
};

/// [`File::flags`].
pub mod file_flags {
    /// The record is in use.
    pub const IN_USE: u16 = 0x0001;
    /// The record is a directory.
    pub const DIRECTORY: u16 = 0x0002;
    /// The record has an `$ATTRIBUTE_LIST`.
    pub const ATTRIBUTE_LIST: u16 = 0x0004;
    /// The record has a reparse point.
    pub const REPARSE_POINT: u16 = 0x0008;
    /// The reparse point is a symbolic link or a junction.
    pub const LINK: u16 = 0x0010;
    /// No unnamed `$DATA` was found: the size is the one a `$FILE_NAME` recorded (often
    /// stale), or 0.
    pub const SIZE_FROM_NAME: u16 = 0x0020;
    /// The unnamed `$DATA` is non-resident.
    pub const NON_RESIDENT: u16 = 0x0040;
    /// The unnamed `$DATA` is compressed.
    pub const COMPRESSED: u16 = 0x0080;
    /// The unnamed `$DATA` is sparse.
    pub const SPARSE: u16 = 0x0100;
    /// Something came from an extension record.
    pub const EXTENDED: u16 = 0x0200;
    /// The record has named `$DATA` streams.
    pub const STREAMS: u16 = 0x0400;
}

/// One file: a base record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct File {
    /// The record.
    pub record: u64,
    /// Its sequence.
    pub sequence: u16,
    /// [`file_flags`].
    pub flags: u16,
    /// `$STANDARD_INFORMATION`'s file attributes.
    pub attributes: u32,
    /// The reparse tag (0 for none).
    pub reparse_tag: u32,
    /// `$STANDARD_INFORMATION`'s times.
    pub times: Times,
    /// The unnamed `$DATA`'s size (see [`file_flags::SIZE_FROM_NAME`]).
    pub size: u64,
    /// Bytes allocated to it (for compressed and sparse streams, those actually allocated).
    pub allocated: u64,
}

impl File {
    /// The file's reference.
    #[must_use]
    pub const fn reference(&self) -> FileReference {
        FileReference::new(self.record, self.sequence)
    }

    /// Whether the record is in use.
    #[must_use]
    pub const fn in_use(&self) -> bool {
        self.flags & file_flags::IN_USE != 0
    }

    /// Whether the file is a directory.
    #[must_use]
    pub const fn is_directory(&self) -> bool {
        self.flags & file_flags::DIRECTORY != 0
    }
}

/// One name of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Link {
    /// The file's position in [`FileTable::files`].
    pub file: u32,
    /// The directory the name records.
    pub parent: FileReference,
    /// Where the name is in the table's names.
    name: (u32, u32),
    /// Its namespace.
    pub namespace: Namespace,
    /// Whether this is a DOS alias of a long name in the same directory.
    pub alias: bool,
}

/// What to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileOptions {
    /// Keep deleted records.
    pub deleted: bool,
    /// Keep the metadata files (records 0 to 15).
    pub metadata: bool,
    /// Keep DOS names that are another name's alias (marked [`Link::alias`]). Without them, a
    /// record's DOS name is kept only when it has no long name in the same directory.
    pub dos_aliases: bool,
    /// The policy of the directory index built alongside.
    pub policy: PathPolicy,
}

impl FileOptions {
    /// Live files only, for disk usage: no metadata files, no DOS aliases.
    pub const LIVE: Self = Self {
        deleted: false,
        metadata: false,
        dos_aliases: false,
        policy: PathPolicy::LIVE,
    };
}

/// What an extension record adds to its base.
#[derive(Clone, Debug)]
struct Piece {
    base: FileReference,
    /// The unnamed `$DATA`'s sizes (data, allocated, flags), when this record holds its first
    /// extent (or the whole resident value).
    data: Option<(u64, u64, u16)>,
    /// Names: (parent, namespace, name start, name length) in the batch's names.
    links: Vec<(FileReference, Namespace, u32, u32)>,
}

/// A chunk's files, links and extension pieces.
#[derive(Clone, Debug, Default)]
pub struct FileBatch {
    files: Vec<File>,
    /// Links grouped by file, `file` local to the batch.
    links: Vec<Link>,
    /// Each file's first link (and, last, the end).
    link_offsets: Vec<u32>,
    names: String,
    pieces: Vec<Piece>,
    /// The chunk's directories, for a [`DirectoryIndex`].
    pub directories: DirectoryBatch,
}

/// Builds [`FileBatch`]es in a scan.
#[derive(Clone, Copy, Debug)]
pub struct FileProjection {
    /// What to keep.
    pub options: FileOptions,
}

/// A chunk's files are gathered in the worker's own batch, which keeps its room from chunk to
/// chunk, and handed over in a batch of exactly their size: every batch lives until the table
/// is built, and room for every record of every chunk would be most of the table's memory.
impl Projection for FileProjection {
    type Output = FileBatch;
    type Scratch = FileBatch;

    fn scratch(&self) -> FileBatch {
        FileBatch::default()
    }

    fn begin(&self, batch: &mut FileBatch, chunk: Chunk) -> FileBatch {
        batch.files.clear();
        batch.files.reserve(chunk.records);
        batch.links.clear();
        batch.links.reserve(chunk.records);
        batch.link_offsets.clear();
        batch.link_offsets.reserve(chunk.records + 1);
        batch.link_offsets.push(0);
        batch.names.clear();
        batch.names.reserve(chunk.records * 20);
        batch.pieces.clear();
        FileBatch::default()
    }

    fn end(&self, output: &mut FileBatch, batch: &mut FileBatch) {
        output.files.extend_from_slice(&batch.files);
        output.links.extend_from_slice(&batch.links);
        output.link_offsets.extend_from_slice(&batch.link_offsets);
        output.names.push_str(&batch.names);
        output.pieces = core::mem::take(&mut batch.pieces);
        output.directories = core::mem::take(&mut batch.directories);
    }

    #[inline]
    fn entry(&self, _: &mut FileBatch, batch: &mut FileBatch, number: u64, entry: Entry<'_>) {
        let Entry::Record(record) = entry else {
            return;
        };
        let header = record.header();
        if !header.in_use() && !self.options.deleted {
            return;
        }
        if !header.is_base() {
            self.extension(batch, &record);
            return;
        }
        let keep = self.options.metadata || number >= records::FIRST_USER;
        if number == records::ROOT || (header.is_directory() && keep) {
            batch.directories.add(number, header, best_name(&record));
        }
        if !keep {
            return;
        }
        let mut file = File {
            record: number,
            sequence: header.sequence,
            flags: if header.in_use() {
                file_flags::IN_USE
            } else {
                0
            },
            ..File::default()
        };
        if header.is_directory() {
            file.flags |= file_flags::DIRECTORY;
        }
        let mut data_found = false;
        let mut name_size = 0u64;
        let index = batch.files.len() as u32;
        let first = batch.links.len();
        for attribute in record.attributes() {
            let Ok(attribute) = attribute else {
                break;
            };
            match attribute.type_code() {
                types::STANDARD_INFORMATION => {
                    if let Ok(value) = attribute.value() {
                        if let Some(times) = StandardInformation::times_of(value) {
                            file.times = times;
                        }
                        if let Some(attributes) = value.get(0x20..0x24) {
                            file.attributes = crate::bytes::le32(attributes, 0);
                        }
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
                    let len = batch.names.len() - start;
                    batch.links.push(Link {
                        file: index,
                        parent: name.parent,
                        name: (start as u32, len as u32),
                        namespace: name.namespace,
                        alias: false,
                    });
                }
                types::DATA => {
                    if attribute.is_unnamed() {
                        data_found |= data(&attribute, &mut file);
                    } else {
                        file.flags |= file_flags::STREAMS;
                    }
                }
                types::ATTRIBUTE_LIST => file.flags |= file_flags::ATTRIBUTE_LIST,
                types::REPARSE_POINT => {
                    file.flags |= file_flags::REPARSE_POINT;
                    if let Ok(value) = attribute.value()
                        && let Some(tag) =
                            value.first_chunk::<4>().map(|tag| u32::from_le_bytes(*tag))
                    {
                        file.reparse_tag = tag;
                        if tag == tags::SYMLINK || tag == tags::MOUNT_POINT {
                            file.flags |= file_flags::LINK;
                        }
                    }
                }
                _ => {}
            }
        }
        if !data_found {
            file.flags |= file_flags::SIZE_FROM_NAME;
            if !file.is_directory() {
                file.size = name_size;
            }
        }
        // A DOS name is an alias of a long name of the file in the same directory.
        let names = batch.links.get_mut(first..).unwrap_or_default();
        for at in 0..names.len() {
            let Some(&Link {
                parent, namespace, ..
            }) = names.get(at)
            else {
                break;
            };
            let alias = namespace.is_dos_only()
                && names
                    .iter()
                    .any(|other| other.parent == parent && !other.namespace.is_dos_only());
            if let Some(link) = names.get_mut(at) {
                link.alias = alias;
            }
        }
        if !self.options.dos_aliases {
            let mut kept = first;
            for at in first..batch.links.len() {
                if let Some(&link) = batch.links.get(at)
                    && !link.alias
                {
                    if let Some(slot) = batch.links.get_mut(kept) {
                        *slot = link;
                    }
                    kept += 1;
                }
            }
            batch.links.truncate(kept);
        }
        batch.files.push(file);
        batch.link_offsets.push(batch.links.len() as u32);
    }
}

/// Reads an unnamed `$DATA` into `file`; whether it held the stream's sizes (resident, or a
/// first extent).
#[inline]
fn data(attribute: &crate::Attribute<'_>, file: &mut File) -> bool {
    if attribute.is_resident() {
        if let Ok(size) = attribute.data_size() {
            file.size = size;
            file.allocated = size;
            return true;
        }
        return false;
    }
    let Ok(extent) = attribute.non_resident() else {
        return false;
    };
    file.flags |= file_flags::NON_RESIDENT;
    if attribute.is_compressed() {
        file.flags |= file_flags::COMPRESSED;
    }
    if attribute.is_sparse() {
        file.flags |= file_flags::SPARSE;
    }
    if !extent.is_first() {
        return false;
    }
    file.size = extent.data_size;
    file.allocated = extent.total_allocated.unwrap_or(extent.allocated_size);
    true
}

impl FileProjection {
    fn extension(&self, batch: &mut FileBatch, record: &Record<'_>) {
        let header = record.header();
        // A freed extension record still names its base, but what it holds is no longer the
        // file's.
        if !header.in_use() {
            return;
        }
        let mut piece = Piece {
            base: header.base_record,
            data: None,
            links: Vec::new(),
        };
        for attribute in record.attributes() {
            let Ok(attribute) = attribute else {
                break;
            };
            match attribute.type_code() {
                types::DATA if attribute.is_unnamed() => {
                    let mut file = File::default();
                    if data(&attribute, &mut file) {
                        piece.data = Some((file.size, file.allocated, file.flags));
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
                        piece.links.push((
                            parent,
                            namespace,
                            start as u32,
                            (batch.names.len() - start) as u32,
                        ));
                    }
                }
                _ => {}
            }
        }
        if piece.data.is_some() || !piece.links.is_empty() {
            batch.pieces.push(piece);
        }
    }
}

/// A file's position in a [`FileTable`].
pub type FileId = u32;
/// A link's position in a [`FileTable`].
pub type LinkId = u32;

/// A name of a file, as a [`FileTable`] hands it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkRef<'a> {
    /// The file.
    pub file: FileId,
    /// The directory the name records.
    pub parent: FileReference,
    /// Its namespace.
    pub namespace: Namespace,
    /// Whether this is a DOS alias of a long name in the same directory.
    pub alias: bool,
    /// The name, as UTF-8.
    pub name: &'a str,
}

/// Every file, with its names: see the module documentation.
///
/// The table keeps the scan's batches as they are (nothing is copied together): files and
/// links have dense ids, mapped to their batch by small arrays, so any file or link is found in
/// O(1). Links held in extension records live beside the batches.
#[derive(Clone, Debug, Default)]
pub struct FileTable {
    batches: Vec<FileBatch>,
    /// The id of each batch's first file, then the total.
    file_starts: Vec<u32>,
    /// The id of each batch's first link, then the total of batched links.
    link_starts: Vec<u32>,
    /// Each file's batch.
    file_batch: Vec<u32>,
    /// Each batched link's batch.
    link_batch: Vec<u32>,
    /// Links from extension records, by file (`file` is a [`FileId`]).
    extra: Vec<Link>,
    extra_names: String,
}

/// A [`FileTable`] and the [`DirectoryIndex`] of its volume, from one scan.
#[derive(Clone, Debug)]
pub struct Files {
    /// The files.
    pub table: FileTable,
    /// The directories.
    pub index: DirectoryIndex,
}

impl Files {
    /// Scans `scan`'s `$MFT` once for both.
    ///
    /// # Errors
    /// As [`Scan::run`].
    pub fn scan<S: ReadAt>(
        scan: &Scan<'_, S>,
        records: u64,
        options: FileOptions,
    ) -> Result<Self, Error> {
        Self::scan_observed(scan, records, options, &mut |_| {})
    }

    /// As [`Self::scan`], calling `observe` on the calling thread as each chunk is done (for
    /// progress reports).
    ///
    /// # Errors
    /// As [`Scan::run`].
    pub fn scan_observed<S: ReadAt>(
        scan: &Scan<'_, S>,
        records: u64,
        options: FileOptions,
        observe: &mut dyn FnMut(Chunk),
    ) -> Result<Self, Error> {
        let mut batches = Vec::new();
        let mut builder = IndexBuilder::new();
        scan.ordered(false)
            .run(&FileProjection { options }, |chunk, mut batch| {
                builder.push(std::mem::take(&mut batch.directories));
                batches.push(batch);
                observe(chunk);
                ControlFlow::Continue(())
            })?;
        // The index and the table at once.
        let (index, table) = std::thread::scope(|scope| {
            let index = scope.spawn(|| builder.build(records, options.policy));
            let table = FileTable::build(batches);
            (index.join(), table)
        });
        let index = index.map_err(|_| Error::Missing {
            what: "the directory index",
        })?;
        Ok(Self { table, index })
    }

    /// Each link's directory: `parents()[id]` is link `id`'s, or [`NONE`] when its parent does
    /// not count ([`DirectoryIndex::locate`]). Aliases are [`NONE`]. Computed on `threads`
    /// threads.
    #[must_use]
    pub fn parents(&self, threads: usize) -> Vec<DirId> {
        let table = &self.table;
        let mut out = vec![NONE; table.link_count()];
        let batched = table.link_batch.len().min(out.len());
        let (mut rest, extra) = out.split_at_mut(batched);
        let per = table.batches.len().div_ceil(threads.max(1)).max(1);
        std::thread::scope(|scope| {
            let mut workers = Vec::new();
            for group in table.batches.chunks(per) {
                let len: usize = group.iter().map(|batch| batch.links.len()).sum();
                let available = rest.len();
                let (slots, more) = std::mem::take(&mut rest).split_at_mut(len.min(available));
                rest = more;
                workers.push(scope.spawn(move || {
                    let links = group.iter().flat_map(|batch| batch.links.iter());
                    for (slot, link) in slots.iter_mut().zip(links) {
                        if !link.alias {
                            *slot = self.index.locate(link.parent).unwrap_or(NONE);
                        }
                    }
                }));
            }
            for (slot, link) in extra.iter_mut().zip(&table.extra) {
                if !link.alias {
                    *slot = self.index.locate(link.parent).unwrap_or(NONE);
                }
            }
            join_all(workers);
        });
        out
    }
}

impl FileTable {
    /// Assembles batches from a scan (in any order): sorts them, numbers files and links, and
    /// folds extension records in. Nothing is copied but the extension records' names.
    #[must_use]
    pub fn build(mut batches: Vec<FileBatch>) -> Self {
        batches.retain(|batch| !batch.files.is_empty() || !batch.pieces.is_empty());
        batches.sort_by_key(|batch| batch.files.first().map_or(u64::MAX, |file| file.record));
        let mut file_starts = Vec::with_capacity(batches.len() + 1);
        let mut link_starts = Vec::with_capacity(batches.len() + 1);
        let (mut files, mut links) = (0u32, 0u32);
        for batch in &batches {
            file_starts.push(files);
            link_starts.push(links);
            files += batch.files.len() as u32;
            links += batch.links.len() as u32;
        }
        file_starts.push(files);
        link_starts.push(links);
        let mut file_batch = Vec::with_capacity(files as usize);
        let mut link_batch = Vec::with_capacity(links as usize);
        for (at, batch) in batches.iter().enumerate() {
            file_batch.extend(std::iter::repeat_n(at as u32, batch.files.len()));
            link_batch.extend(std::iter::repeat_n(at as u32, batch.links.len()));
        }
        let mut table = Self {
            batches,
            file_starts,
            link_starts,
            file_batch,
            link_batch,
            extra: Vec::new(),
            extra_names: String::new(),
        };
        table.fold();
        table
    }

    /// Folds extension records into their bases: sizes into the files, names into the extra
    /// links (sorted by file), DOS aliases marked across both.
    fn fold(&mut self) {
        let mut pieces = Vec::new();
        for (at, batch) in self.batches.iter_mut().enumerate() {
            for piece in std::mem::take(&mut batch.pieces) {
                pieces.push((at, piece));
            }
        }
        let mut extended = Vec::new();
        for (batch_at, piece) in pieces {
            let Some(id) = self.find(piece.base.record) else {
                continue;
            };
            let Some(file) = self.file_mut(id) else {
                continue;
            };
            // Pieces come from extension records in use only, and belong only to a base in use:
            // a deleted file's extension records may belong to another file by now.
            if file.sequence != piece.base.sequence || !file.in_use() {
                continue;
            }
            file.flags |= file_flags::EXTENDED;
            if let Some((size, allocated, flags)) = piece.data {
                file.size = size;
                file.allocated = allocated;
                file.flags = (file.flags & !file_flags::SIZE_FROM_NAME) | flags;
            }
            if piece.links.is_empty() {
                continue;
            }
            extended.push(id);
            let names = self
                .batches
                .get(batch_at)
                .map_or("", |batch| batch.names.as_str());
            for (parent, namespace, start, len) in piece.links {
                let name = names
                    .get(start as usize..(start + len) as usize)
                    .unwrap_or_default();
                let at = self.extra_names.len() as u32;
                self.extra_names.push_str(name);
                self.extra.push(Link {
                    file: id,
                    parent,
                    name: (at, name.len() as u32),
                    namespace,
                    alias: false,
                });
            }
        }
        self.extra.sort_by_key(|link| link.file);
        extended.sort_unstable();
        extended.dedup();
        for id in extended {
            self.mark_aliases(id);
        }
    }

    /// Marks file `id`'s DOS names that alias a long name in the same directory.
    fn mark_aliases(&mut self, id: FileId) {
        let parents: Vec<(FileReference, bool)> = self
            .links_of(id)
            .map(|link| (link.parent, link.namespace.is_dos_only()))
            .collect();
        let alias = |parent: FileReference, dos: bool| {
            dos && parents
                .iter()
                .any(|(other, other_dos)| *other == parent && !other_dos)
        };
        let range = self.extra_range(id);
        if let Some(extra) = self.extra.get_mut(range) {
            for link in extra {
                link.alias = alias(link.parent, link.namespace.is_dos_only());
            }
        }
        if let Some((batch, local)) = self.locate_file(id)
            && let Some(batch) = self.batches.get_mut(batch)
        {
            let start = batch.link_offsets.get(local).copied().unwrap_or(0) as usize;
            let end = batch.link_offsets.get(local + 1).copied().unwrap_or(0) as usize;
            if let Some(links) = batch.links.get_mut(start..end) {
                for link in links {
                    link.alias = alias(link.parent, link.namespace.is_dos_only());
                }
            }
        }
    }

    fn extra_range(&self, id: FileId) -> std::ops::Range<usize> {
        let start = self.extra.partition_point(|link| link.file < id);
        let end = self.extra.partition_point(|link| link.file <= id);
        start..end
    }

    /// The batch and position within it of file `id`.
    #[inline]
    fn locate_file(&self, id: FileId) -> Option<(usize, usize)> {
        let batch = *self.file_batch.get(id as usize)? as usize;
        let start = *self.file_starts.get(batch)?;
        Some((batch, (id - start) as usize))
    }

    fn file_mut(&mut self, id: FileId) -> Option<&mut File> {
        let (batch, local) = self.locate_file(id)?;
        self.batches.get_mut(batch)?.files.get_mut(local)
    }

    /// Files in the table.
    #[must_use]
    pub fn len(&self) -> usize {
        self.file_batch.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.file_batch.is_empty()
    }

    /// Links in the table.
    #[must_use]
    pub fn link_count(&self) -> usize {
        self.link_batch.len() + self.extra.len()
    }

    /// File `id`.
    #[must_use]
    #[inline]
    pub fn file(&self, id: FileId) -> Option<&File> {
        let (batch, local) = self.locate_file(id)?;
        self.batches.get(batch)?.files.get(local)
    }

    /// Link `id`.
    #[must_use]
    #[inline]
    pub fn link(&self, id: LinkId) -> Option<LinkRef<'_>> {
        let batched = self.link_batch.len();
        if (id as usize) < batched {
            let batch_at = *self.link_batch.get(id as usize)? as usize;
            let batch = self.batches.get(batch_at)?;
            let local = (id - *self.link_starts.get(batch_at)?) as usize;
            let link = batch.links.get(local)?;
            Some(LinkRef {
                file: self.file_starts.get(batch_at)? + link.file,
                parent: link.parent,
                namespace: link.namespace,
                alias: link.alias,
                name: batch
                    .names
                    .get(link.name.0 as usize..(link.name.0 + link.name.1) as usize)?,
            })
        } else {
            let link = self.extra.get(id as usize - batched)?;
            Some(LinkRef {
                file: link.file,
                parent: link.parent,
                namespace: link.namespace,
                alias: link.alias,
                name: self
                    .extra_names
                    .get(link.name.0 as usize..(link.name.0 + link.name.1) as usize)?,
            })
        }
    }

    /// Link `id` and its file, found through one batch.
    #[must_use]
    #[inline]
    pub fn link_and_file(&self, id: LinkId) -> Option<(LinkRef<'_>, &File)> {
        let batched = self.link_batch.len();
        if (id as usize) < batched {
            let batch_at = *self.link_batch.get(id as usize)? as usize;
            let batch = self.batches.get(batch_at)?;
            let local = (id - *self.link_starts.get(batch_at)?) as usize;
            let link = batch.links.get(local)?;
            let file = batch.files.get(link.file as usize)?;
            Some((
                LinkRef {
                    file: self.file_starts.get(batch_at)? + link.file,
                    parent: link.parent,
                    namespace: link.namespace,
                    alias: link.alias,
                    name: batch
                        .names
                        .get(link.name.0 as usize..(link.name.0 + link.name.1) as usize)?,
                },
                file,
            ))
        } else {
            let link = self.link(id)?;
            Some((link, self.file(link.file)?))
        }
    }

    /// The links of file `id`: those in its base record, then those from its extension
    /// records.
    pub fn links_of(&self, id: FileId) -> impl Iterator<Item = LinkRef<'_>> + '_ {
        let base = self.locate_file(id).and_then(|(batch_at, local)| {
            let batch = self.batches.get(batch_at)?;
            let start = *batch.link_offsets.get(local)?;
            let end = *batch.link_offsets.get(local + 1)?;
            let first = *self.link_starts.get(batch_at)?;
            Some(first + start..first + end)
        });
        let extended = self
            .file(id)
            .is_some_and(|file| file.flags & file_flags::EXTENDED != 0);
        let extra = if extended {
            let range = self.extra_range(id);
            let batched = self.link_batch.len() as u32;
            range.start as u32 + batched..range.end as u32 + batched
        } else {
            0..0
        };
        base.into_iter()
            .flatten()
            .chain(extra)
            .filter_map(|link| self.link(link))
    }

    /// The file at `record`, if the table has one.
    #[must_use]
    pub fn find(&self, record: u64) -> Option<FileId> {
        let after = self.batches.partition_point(|batch| {
            batch
                .files
                .first()
                .is_some_and(|file| file.record <= record)
        });
        let batch_at = after.checked_sub(1)?;
        let batch = self.batches.get(batch_at)?;
        let local = batch
            .files
            .binary_search_by_key(&record, |file| file.record)
            .ok()?;
        Some(self.file_starts.get(batch_at)? + local as u32)
    }

    /// Every file with its id, in record order.
    pub fn files(&self) -> impl Iterator<Item = (FileId, &File)> + '_ {
        self.batches
            .iter()
            .zip(&self.file_starts)
            .flat_map(|(batch, &start)| {
                batch
                    .files
                    .iter()
                    .enumerate()
                    .map(move |(local, file)| (start + local as u32, file))
            })
    }

    /// Every link with its id.
    pub fn links(&self) -> impl Iterator<Item = (LinkId, LinkRef<'_>)> + '_ {
        (0..self.link_count() as u32).filter_map(|id| Some((id, self.link(id)?)))
    }
}

#[cfg(all(test, feature = "testing"))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]

    use std::collections::{HashMap, HashSet};

    use super::*;
    use crate::{
        mft::Mft,
        testing::corpus::{CorpusConfig, mft},
    };

    /// A record of more names than most hold (4 KiB records hold dozens): every name is kept,
    /// in attribute order, and a DOS alias is known for one wherever it falls among them.
    #[test]
    fn every_name_of_a_record_is_judged() {
        use crate::testing::record::RecordBuilder;

        let root = FileReference::new(records::ROOT, 5);
        let other = FileReference::new(40, 1);
        let mut file = RecordBuilder::new(16).size(4096);
        for at in 0..20 {
            file = file.file_name(
                root,
                Namespace::Win32,
                &format!("link {at:02}"),
                Times::default(),
                7,
            );
        }
        // Past the twentieth name: an alias in the root, and a DOS name alone in another
        // directory.
        file = file
            .file_name(root, Namespace::Dos, "LINK~1", Times::default(), 7)
            .file_name(other, Namespace::Dos, "ONLY~1", Times::default(), 7);
        let mut bytes = Vec::new();
        for number in 0..17 {
            bytes.extend(match number {
                16 => file.build(),
                5 => RecordBuilder::new(5)
                    .size(4096)
                    .sequence(5)
                    .directory(true)
                    .file_name(root, Namespace::Win32AndDos, ".", Times::default(), 0)
                    .build(),
                _ => RecordBuilder::new(number).size(4096).build(),
            });
        }
        let mft = Mft::from_file(bytes.as_slice()).unwrap();
        for dos_aliases in [false, true] {
            let options = FileOptions {
                dos_aliases,
                ..FileOptions::LIVE
            };
            let files = Files::scan(&Scan::new(&mft), mft.records(), options).unwrap();
            let id = files.table.find(16).unwrap();
            let names: Vec<_> = files
                .table
                .links_of(id)
                .map(|link| (link.name.to_owned(), link.alias))
                .collect();
            let mut expected: Vec<_> = (0..20).map(|at| (format!("link {at:02}"), false)).collect();
            if dos_aliases {
                expected.push(("LINK~1".to_owned(), true));
            }
            expected.push(("ONLY~1".to_owned(), false));
            assert_eq!(names, expected, "DOS aliases kept: {dos_aliases}");
        }
    }

    #[test]
    fn files_match_the_corpus_truth() {
        let config = CorpusConfig::realistic(20_000);
        let (bytes, truth) = mft(&config);
        let mft = Mft::from_file(bytes.as_slice()).unwrap();
        for threads in [1, 3, 8] {
            let scan = Scan::new(&mft).threads(threads).chunk_bytes(64 << 10);
            let files = Files::scan(&scan, mft.records(), FileOptions::LIVE).unwrap();
            let table = &files.table;
            let expected: HashMap<u64, &crate::testing::corpus::TruthFile> = truth
                .files
                .iter()
                .filter(|file| file.in_use && file.reference.record >= 16)
                .map(|file| (file.reference.record, file))
                .collect();
            let mut seen = HashSet::new();
            for (id, file) in table.files() {
                if file.is_directory() {
                    continue;
                }
                let truth = expected
                    .get(&file.record)
                    .unwrap_or_else(|| panic!("{}", file.record));
                seen.insert(file.record);
                assert_eq!(file.size, truth.size, "record {}", file.record);
                let mut names: Vec<String> = table
                    .links_of(id)
                    .filter(|link| !link.alias)
                    .map(|link| link.name.to_owned())
                    .collect();
                let mut wanted: Vec<String> = truth
                    .names
                    .iter()
                    .filter(|name| {
                        !(name.namespace.is_dos_only()
                            && truth.names.iter().any(|other| {
                                other.parent == name.parent && !other.namespace.is_dos_only()
                            }))
                    })
                    .map(|name| name.name.clone())
                    .collect();
                names.sort();
                wanted.sort();
                assert_eq!(names, wanted, "record {}", file.record);
            }
            assert_eq!(seen.len(), expected.len());
            // Every link of a live file in a live directory is placed.
            let parents = files.parents(threads);
            assert_eq!(parents.len(), table.link_count());
            for (id, link) in table.links() {
                if link.alias {
                    assert_eq!(parents[id as usize], NONE);
                }
            }
            let placed = parents.iter().filter(|parent| **parent != NONE).count();
            assert!(placed > table.len() / 2, "{placed}");
        }
    }
}
