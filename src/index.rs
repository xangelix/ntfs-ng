//! Directories and paths.
//!
//! A name's `$FILE_NAME` records its parent directory by record number *and* sequence number;
//! a path is the chain of parents up to the root, record 5. Callers disagree on which links
//! count, so the rules are a [`PathPolicy`]:
//!
//! - [`ParentRule`]: a parent counts when its sequence matches exactly; or, for a deleted
//!   parent, also when it is one more (NTFS increments a record's sequence when it frees it);
//!   or always.
//! - Whether deleted directories may be path components at all.
//! - The deepest path allowed, and whether the root must be exactly what NTFS makes it.
//!
//! A [`DirectoryIndex`] holds every directory record (in dense arrays indexed by record
//! number), resolves every directory's chain once, in O(directories), and then answers each
//! name's question — *where is my parent?* ([`DirectoryIndex::locate`]) — with two array
//! lookups, and writes paths from a UTF-8 name arena without hashing anything. Files never
//! enter the index: its memory is proportional to the directories, not the records.
//!
//! Build one from a scan with [`DirectoryProjection`] (directories only, cheap: other records
//! are dropped after their header), or feed [`DirectoryBatch`]es from a projection of your own.

use std::{fmt::Write as _, ops::ControlFlow};

use crate::{
    Error,
    name::Name,
    record::{FileReference, Record, RecordHeader, next_sequence},
    records::ROOT,
    scan::{Chunk, Entry, Projection, Scan},
    source::ReadAt,
    types,
    value::{FileName, Namespace},
};

/// A directory's position in a [`DirectoryIndex`].
pub type DirId = u32;

/// No directory.
pub const NONE: DirId = DirId::MAX;

/// Which parent references count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentRule {
    /// The parent's sequence must be the one recorded.
    Exact,
    /// As [`Self::Exact`], or for a deleted parent one more (it was freed after the name was
    /// recorded).
    ExactOrNext,
    /// Any record at the number counts (no sequence check).
    Any,
}

/// How paths are resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathPolicy {
    /// Which parent references count.
    pub parent: ParentRule,
    /// Whether deleted directories may be parents (and path components).
    pub deleted_directories: bool,
    /// The most components a placed name's path may have, its own name included.
    pub max_depth: usize,
    /// Whether record 5 must be in use, a directory, named `.` and its own parent to be the
    /// root.
    pub strict_root: bool,
}

impl PathPolicy {
    /// Names of files in use: exact links between directories in use.
    pub const LIVE: Self = Self {
        parent: ParentRule::Exact,
        deleted_directories: false,
        max_depth: 4096,
        strict_root: false,
    };
    /// Strict: exact links between directories in use, and the root exactly as NTFS makes it.
    pub const STRICT: Self = Self {
        parent: ParentRule::Exact,
        deleted_directories: false,
        max_depth: 65_535,
        strict_root: true,
    };
    /// Deleted names too: deleted directories place their names, under parents in use with the
    /// recorded sequence or deleted with it or the next.
    pub const RECOVERY: Self = Self {
        parent: ParentRule::ExactOrNext,
        deleted_directories: true,
        max_depth: 4096,
        strict_root: false,
    };

    /// Whether a parent at `sequence`, in use or not, is the one a name recorded with
    /// `recorded`.
    #[must_use]
    pub const fn links(&self, sequence: u16, in_use: bool, recorded: u16) -> bool {
        if !in_use && !self.deleted_directories {
            return false;
        }
        match self.parent {
            ParentRule::Exact => sequence == recorded,
            ParentRule::ExactOrNext => {
                sequence == recorded || (!in_use && sequence == next_sequence(recorded))
            }
            ParentRule::Any => true,
        }
    }
}

/// Where a directory's chain leads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DirectoryState {
    /// The root itself.
    Root = 1,
    /// Linked to the root through valid parents.
    Linked = 2,
    /// Its own parent reference does not count (missing, reused, or deleted when deleted
    /// directories are not parents).
    Orphaned = 3,
    /// It has no usable name.
    Unnamed = 4,
    /// Its chain loops.
    Cycle = 5,
    /// Its path would be deeper than the policy allows.
    TooDeep = 6,
    /// An ancestor's chain is broken.
    Detached = 7,
}

impl DirectoryState {
    /// Whether names in the directory have a path.
    #[must_use]
    pub const fn is_reachable(self) -> bool {
        matches!(self, Self::Root | Self::Linked)
    }
}

const IN_USE: u8 = 1;
const NAMED: u8 = 2;
const EXTENSION: u8 = 4;

/// One directory record (or one name of a directory found in an extension record).
#[derive(Clone, Copy, Debug)]
struct Raw {
    record: u64,
    sequence: u16,
    flags: u8,
    rank: u8,
    parent: FileReference,
    /// The name, as UTF-16LE bytes in the batch's arena.
    name: (u32, u16),
}

/// Directories found in a chunk of records: what a projection hands to an [`IndexBuilder`].
#[derive(Clone, Debug, Default)]
pub struct DirectoryBatch {
    dirs: Vec<Raw>,
    names: Vec<u8>,
}

impl DirectoryBatch {
    /// An empty batch.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            dirs: Vec::new(),
            names: Vec::new(),
        }
    }

    /// Directories in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    /// Whether the batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    fn push_name(&mut self, name: Name<'_>) -> (u32, u16) {
        let start = u32::try_from(self.names.len()).unwrap_or(u32::MAX);
        self.names.extend_from_slice(name.bytes());
        (start, u16::try_from(name.len()).unwrap_or(u16::MAX))
    }

    /// Adds the directory record `number` whose header is `header`, with its best name (see
    /// [`best_name`]).
    pub fn add(
        &mut self,
        number: u64,
        header: &RecordHeader,
        name: Option<(FileReference, Namespace, Name<'_>)>,
    ) {
        self.push(number, header.sequence, header.in_use(), name);
    }

    /// Adds the directory record `number` at `sequence`, in use or not, with the name its
    /// paths use and the parent that name records (`None` when it has no usable name): for
    /// callers that parse records themselves and choose names by rules of their own.
    pub fn push(
        &mut self,
        number: u64,
        sequence: u16,
        in_use: bool,
        name: Option<(FileReference, Namespace, Name<'_>)>,
    ) {
        let mut flags = if in_use { IN_USE } else { 0 };
        let (parent, rank, name) = match name {
            Some((parent, namespace, name)) => {
                flags |= NAMED;
                (parent, namespace.rank(), self.push_name(name))
            }
            None => (FileReference::default(), u8::MAX, (0, 0)),
        };
        self.dirs.push(Raw {
            record: number,
            sequence,
            flags,
            rank,
            parent,
            name,
        });
    }

    /// Adds a name found in an extension record of the base record `base`: used for the base
    /// directory if it has no better name of its own.
    pub fn add_extension_name(
        &mut self,
        base: FileReference,
        parent: FileReference,
        namespace: Namespace,
        name: Name<'_>,
    ) {
        let name = self.push_name(name);
        self.dirs.push(Raw {
            record: base.record,
            sequence: base.sequence,
            flags: EXTENSION | NAMED,
            rank: namespace.rank(),
            parent,
            name,
        });
    }
}

/// The best name of a record for paths: its first `$FILE_NAME` of the best rank ([`Namespace::rank`]:
/// Win32 first, then POSIX, then DOS). `None` when it has none readable.
#[must_use]
pub fn best_name<'a>(record: &Record<'a>) -> Option<(FileReference, Namespace, Name<'a>)> {
    let mut best: Option<(FileReference, Namespace, Name<'a>)> = None;
    for attribute in record.attributes() {
        let Ok(attribute) = attribute else {
            break;
        };
        if attribute.type_code() != types::FILE_NAME {
            continue;
        }
        let Ok(value) = attribute.value() else {
            continue;
        };
        let Ok(link) = FileName::link(value) else {
            continue;
        };
        if best.is_none_or(|(_, namespace, _)| link.1.rank() < namespace.rank()) {
            let rank = link.1.rank();
            best = Some(link);
            if rank == 0 {
                break;
            }
        }
    }
    best
}

/// A projection that collects directories (in use, and deleted ones when the policy places
/// names under them), for a [`DirectoryIndex`]. Other records are dropped after their header.
#[derive(Clone, Copy, Debug)]
pub struct DirectoryProjection {
    /// The policy the index will use.
    pub policy: PathPolicy,
}

impl Projection for DirectoryProjection {
    type Output = DirectoryBatch;
    type Scratch = ();

    fn scratch(&self) {}

    fn begin(&self, (): &mut (), _: Chunk) -> DirectoryBatch {
        DirectoryBatch::new()
    }

    #[inline]
    fn entry(&self, batch: &mut DirectoryBatch, (): &mut (), number: u64, entry: Entry<'_>) {
        let Entry::Record(record) = entry else {
            return;
        };
        let header = record.header();
        if !header.is_base() {
            // A directory's name may sit in an extension record (rarely: when its base record
            // overflowed); keep it in case the base needs it. Only an extension record in use
            // holds what its base has (see `IndexBuilder::build`).
            if header.in_use()
                && let Some((parent, namespace, name)) = best_name(&record)
            {
                batch.add_extension_name(header.base_record, parent, namespace, name);
            }
            return;
        }
        if number != ROOT
            && (!header.is_directory() || !(header.in_use() || self.policy.deleted_directories))
        {
            return;
        }
        batch.add(number, header, best_name(&record));
    }
}

/// Collects [`DirectoryBatch`]es into a [`DirectoryIndex`].
#[derive(Debug, Default)]
pub struct IndexBuilder {
    batches: Vec<DirectoryBatch>,
}

impl IndexBuilder {
    /// An empty builder.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            batches: Vec::new(),
        }
    }

    /// Adds a batch.
    pub fn push(&mut self, batch: DirectoryBatch) {
        if !batch.is_empty() {
            self.batches.push(batch);
        }
    }

    /// The index of `records` records, resolved with `policy`.
    #[must_use]
    pub fn build(self, records: u64, policy: PathPolicy) -> DirectoryIndex {
        DirectoryIndex::build(self.batches, records, policy)
    }
}

/// One directory of the index.
#[derive(Clone, Copy, Debug)]
struct Dir {
    record: u64,
    sequence: u16,
    flags: u8,
    rank: u8,
    state: DirectoryState,
    depth: u16,
    parent_ref: FileReference,
    parent: DirId,
    name16: (u32, u16),
    name8: (u32, u32),
}

/// Every directory, its chain resolved; see the module documentation.
#[derive(Clone, Debug)]
pub struct DirectoryIndex {
    slots: Vec<DirId>,
    dirs: Vec<Dir>,
    names16: Vec<u8>,
    names8: String,
    policy: PathPolicy,
}

impl DirectoryIndex {
    /// Scans `scan`'s `$MFT` for directories and resolves them with `policy`.
    ///
    /// # Errors
    /// As [`Scan::run`].
    pub fn scan<S: ReadAt>(
        scan: &Scan<'_, S>,
        records: u64,
        policy: PathPolicy,
    ) -> Result<Self, Error> {
        let mut builder = IndexBuilder::new();
        scan.ordered(false)
            .run(&DirectoryProjection { policy }, |_, batch| {
                builder.push(batch);
                ControlFlow::Continue(())
            })?;
        Ok(builder.build(records, policy))
    }

    fn build(batches: Vec<DirectoryBatch>, records: u64, policy: PathPolicy) -> Self {
        let slot_count = usize::try_from(records).unwrap_or(usize::MAX);
        let mut slots = vec![NONE; slot_count];
        let total: usize = batches.iter().map(DirectoryBatch::len).sum();
        let mut dirs: Vec<Dir> = Vec::with_capacity(total);
        let mut names16 = Vec::with_capacity(batches.iter().map(|batch| batch.names.len()).sum());
        let mut extension_names = Vec::new();
        for batch in &batches {
            let base = u32::try_from(names16.len()).unwrap_or(u32::MAX);
            names16.extend_from_slice(&batch.names);
            for raw in &batch.dirs {
                let name16 = (base.saturating_add(raw.name.0), raw.name.1);
                if raw.flags & EXTENSION != 0 {
                    extension_names.push((*raw, name16));
                    continue;
                }
                let Some(slot) = usize::try_from(raw.record)
                    .ok()
                    .and_then(|at| slots.get_mut(at))
                else {
                    continue;
                };
                if *slot != NONE {
                    continue;
                }
                *slot = DirId::try_from(dirs.len()).unwrap_or(NONE);
                dirs.push(Dir {
                    record: raw.record,
                    sequence: raw.sequence,
                    flags: raw.flags,
                    rank: raw.rank,
                    state: DirectoryState::Detached,
                    depth: 0,
                    parent_ref: raw.parent,
                    parent: NONE,
                    name16,
                    name8: (0, 0),
                });
            }
        }
        drop(batches);
        // Names from extension records (in use) serve a base directory in use, of the same
        // sequence, that has no better name: a deleted directory's extension records may
        // belong to another file by now, so it has only the names its own record holds.
        for (raw, name16) in extension_names {
            let Some(dir) = usize::try_from(raw.record)
                .ok()
                .and_then(|at| slots.get(at))
                .and_then(|&id| dirs.get_mut(id as usize))
            else {
                continue;
            };
            if dir.flags & IN_USE != 0 && dir.sequence == raw.sequence && raw.rank < dir.rank {
                dir.flags |= NAMED;
                dir.rank = raw.rank;
                dir.parent_ref = raw.parent;
                dir.name16 = name16;
            }
        }
        let mut index = Self {
            slots,
            dirs,
            names16,
            names8: String::new(),
            policy,
        };
        index.resolve();
        index.decode_names();
        index
    }

    /// Resolves every directory's chain, each once.
    fn resolve(&mut self) {
        const UNKNOWN: u8 = 0;
        const VISITING: u8 = 1;
        const DONE: u8 = 2;
        let mut mark = vec![UNKNOWN; self.dirs.len()];
        let mut stack: Vec<usize> = Vec::new();
        for start in 0..self.dirs.len() {
            if mark.get(start) != Some(&UNKNOWN) {
                continue;
            }
            stack.clear();
            let mut current = start;
            // Walk up until a resolved directory or a break.
            let (mut state, mut depth) = loop {
                match mark.get(current).copied() {
                    Some(DONE) => {
                        let dir = self.dirs.get(current);
                        break (
                            dir.map_or(DirectoryState::Detached, |dir| dir.state),
                            dir.map_or(0, |dir| dir.depth),
                        );
                    }
                    Some(VISITING) => break (DirectoryState::Cycle, 0),
                    _ => {}
                }
                let Some(&dir) = self.dirs.get(current) else {
                    break (DirectoryState::Detached, 0);
                };
                let own = self.own_state(&dir);
                if let Some(own) = own {
                    if let Some(slot) = self.dirs.get_mut(current) {
                        slot.state = own;
                        slot.depth = 0;
                    }
                    if let Some(slot) = mark.get_mut(current) {
                        *slot = DONE;
                    }
                    break (own, 0);
                }
                let parent = self.slot(dir.parent_ref.record);
                if let Some(slot) = self.dirs.get_mut(current) {
                    slot.parent = parent;
                }
                if let Some(slot) = mark.get_mut(current) {
                    *slot = VISITING;
                }
                stack.push(current);
                current = parent as usize;
            };
            // Walk back down, extending the chain one component per level.
            let broken = !state.is_reachable();
            while let Some(at) = stack.pop() {
                if broken {
                    state = if state == DirectoryState::Cycle {
                        DirectoryState::Cycle
                    } else {
                        DirectoryState::Detached
                    };
                } else {
                    depth = depth.saturating_add(1);
                    state = if usize::from(depth) >= self.policy.max_depth {
                        DirectoryState::TooDeep
                    } else {
                        DirectoryState::Linked
                    };
                }
                if let Some(dir) = self.dirs.get_mut(at) {
                    dir.state = state;
                    dir.depth = depth;
                }
                if let Some(slot) = mark.get_mut(at) {
                    *slot = DONE;
                }
                if state == DirectoryState::TooDeep {
                    // Everything below is too deep as well, whatever it is linked to.
                    while let Some(below) = stack.pop() {
                        if let Some(dir) = self.dirs.get_mut(below) {
                            dir.state = DirectoryState::TooDeep;
                            dir.depth = u16::MAX;
                        }
                        if let Some(slot) = mark.get_mut(below) {
                            *slot = DONE;
                        }
                    }
                }
            }
        }
    }

    /// A directory's state from itself and its parent link alone: `Some` when the chain ends
    /// here (the root, or a break), `None` when it continues to a valid parent.
    fn own_state(&self, dir: &Dir) -> Option<DirectoryState> {
        if dir.record == ROOT {
            let root = !self.policy.strict_root
                || (dir.flags & IN_USE != 0
                    && dir.flags & NAMED != 0
                    && dir.parent_ref == FileReference::new(ROOT, dir.sequence)
                    && self.name16(dir).eq_str("."));
            return Some(if root {
                DirectoryState::Root
            } else {
                DirectoryState::Orphaned
            });
        }
        if dir.flags & NAMED == 0 {
            return Some(DirectoryState::Unnamed);
        }
        let parent = self.slot(dir.parent_ref.record);
        let linked = self.dirs.get(parent as usize).is_some_and(|parent| {
            self.policy.links(
                parent.sequence,
                parent.flags & IN_USE != 0,
                dir.parent_ref.sequence,
            )
        });
        if linked {
            None
        } else {
            Some(DirectoryState::Orphaned)
        }
    }

    fn name16(&self, dir: &Dir) -> Name<'_> {
        let start = dir.name16.0 as usize;
        let end = start + usize::from(dir.name16.1) * 2;
        Name::new(self.names16.get(start..end).unwrap_or_default())
    }

    /// Decodes every directory name to UTF-8 once (U+FFFD for unpaired surrogates).
    fn decode_names(&mut self) {
        let mut names8 = String::with_capacity(self.names16.len() / 2);
        for at in 0..self.dirs.len() {
            let Some(dir) = self.dirs.get(at).copied() else {
                continue;
            };
            let start = names8.len();
            if dir.flags & NAMED != 0 {
                self.name16(&dir).push_lossy(&mut names8);
            }
            if let Some(dir) = self.dirs.get_mut(at) {
                dir.name8 = (
                    u32::try_from(start).unwrap_or(u32::MAX),
                    u32::try_from(names8.len() - start).unwrap_or(0),
                );
            }
        }
        self.names8 = names8;
    }

    fn slot(&self, record: u64) -> DirId {
        usize::try_from(record)
            .ok()
            .and_then(|at| self.slots.get(at))
            .copied()
            .unwrap_or(NONE)
    }

    fn dir(&self, id: DirId) -> Option<&Dir> {
        self.dirs.get(id as usize)
    }

    /// The policy.
    #[must_use]
    pub const fn policy(&self) -> &PathPolicy {
        &self.policy
    }

    /// Directories in the index.
    #[must_use]
    pub fn len(&self) -> usize {
        self.dirs.len()
    }

    /// Whether the index is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.dirs.is_empty()
    }

    /// The directory record `record` holds, if it holds one (whatever its state).
    #[must_use]
    pub fn directory(&self, record: u64) -> Option<DirId> {
        Some(self.slot(record)).filter(|&id| id != NONE)
    }

    /// The directory a name recorded with the parent reference `parent` is in: `None` when
    /// the reference does not count under the policy, the directory's chain is broken, or the
    /// name's path would be too deep.
    #[must_use]
    #[inline]
    pub fn locate(&self, parent: FileReference) -> Option<DirId> {
        let id = self.slot(parent.record);
        let dir = self.dir(id)?;
        (dir.state.is_reachable()
            && usize::from(dir.depth) + 1 < self.policy.max_depth.saturating_add(1)
            && self
                .policy
                .links(dir.sequence, dir.flags & IN_USE != 0, parent.sequence))
        .then_some(id)
    }

    /// Where `id`'s chain leads.
    #[must_use]
    pub fn state(&self, id: DirId) -> Option<DirectoryState> {
        self.dir(id).map(|dir| dir.state)
    }

    /// Components of `id`'s path (0 for the root).
    #[must_use]
    pub fn depth(&self, id: DirId) -> usize {
        self.dir(id).map_or(0, |dir| usize::from(dir.depth))
    }

    /// The directory's record and sequence.
    #[must_use]
    pub fn reference(&self, id: DirId) -> Option<FileReference> {
        self.dir(id)
            .map(|dir| FileReference::new(dir.record, dir.sequence))
    }

    /// Whether the directory's record is in use.
    #[must_use]
    pub fn in_use(&self, id: DirId) -> bool {
        self.dir(id).is_some_and(|dir| dir.flags & IN_USE != 0)
    }

    /// The parent reference the directory's name records.
    #[must_use]
    pub fn parent_reference(&self, id: DirId) -> Option<FileReference> {
        self.dir(id)
            .filter(|dir| dir.flags & NAMED != 0)
            .map(|dir| dir.parent_ref)
    }

    /// The directory's resolved parent, for a linked directory.
    #[must_use]
    pub fn parent(&self, id: DirId) -> Option<DirId> {
        self.dir(id)
            .filter(|dir| dir.state == DirectoryState::Linked)
            .map(|dir| dir.parent)
    }

    /// The directory's name, exactly as stored (empty when unnamed).
    #[must_use]
    pub fn name(&self, id: DirId) -> Name<'_> {
        self.dir(id).map_or(Name::default(), |dir| self.name16(dir))
    }

    /// The directory's name as UTF-8 (U+FFFD for unpaired surrogates).
    #[must_use]
    pub fn name_str(&self, id: DirId) -> &str {
        self.dir(id).map_or("", |dir| {
            let start = dir.name8.0 as usize;
            self.names8
                .get(start..start + dir.name8.1 as usize)
                .unwrap_or_default()
        })
    }

    /// The directories from the root's child down to `id` (empty for the root), into `out`
    /// (cleared first). Empty, too, when `id`'s chain is broken.
    pub fn ancestors(&self, id: DirId, out: &mut Vec<DirId>) {
        out.clear();
        let Some(dir) = self.dir(id) else {
            return;
        };
        if dir.state != DirectoryState::Linked {
            return;
        }
        let mut at = id;
        while let Some(dir) = self.dir(at) {
            if dir.state != DirectoryState::Linked || out.len() > usize::from(u16::MAX) {
                break;
            }
            out.push(at);
            at = dir.parent;
        }
        out.reverse();
    }

    /// Appends `id`'s path to `out`: its components from the root's child down, joined by
    /// `separator`, without a leading one (nothing for the root). Nothing when `id`'s chain is
    /// broken.
    pub fn write_path(&self, id: DirId, separator: char, out: &mut String) {
        let mut stack = [NONE; 64];
        let mut depth = 0usize;
        let mut at = id;
        while let Some(dir) = self.dir(at) {
            if dir.state != DirectoryState::Linked {
                break;
            }
            if let Some(slot) = stack.get_mut(depth) {
                *slot = at;
                depth += 1;
                at = dir.parent;
            } else {
                // Deeper than the stack: the general route.
                let mut ancestors = Vec::new();
                self.ancestors(id, &mut ancestors);
                for (index, &component) in ancestors.iter().enumerate() {
                    if index > 0 {
                        out.push(separator);
                    }
                    out.push_str(self.name_str(component));
                }
                return;
            }
        }
        for (index, &component) in stack
            .get(..depth)
            .unwrap_or_default()
            .iter()
            .rev()
            .enumerate()
        {
            if index > 0 {
                out.push(separator);
            }
            out.push_str(self.name_str(component));
        }
    }

    /// `id`'s path as a string (see [`Self::write_path`]).
    #[must_use]
    pub fn path(&self, id: DirId, separator: char) -> String {
        let mut out = String::new();
        self.write_path(id, separator, &mut out);
        out
    }

    /// Counts of directories by state, for diagnostics.
    #[must_use]
    pub fn states(&self) -> [(DirectoryState, usize); 7] {
        let mut counts = [
            (DirectoryState::Root, 0),
            (DirectoryState::Linked, 0),
            (DirectoryState::Orphaned, 0),
            (DirectoryState::Unnamed, 0),
            (DirectoryState::Cycle, 0),
            (DirectoryState::TooDeep, 0),
            (DirectoryState::Detached, 0),
        ];
        for dir in &self.dirs {
            if let Some(count) = counts.iter_mut().find(|(state, _)| *state == dir.state) {
                count.1 += 1;
            }
        }
        counts
    }

    /// A one-line summary of the states, for diagnostics.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut out = String::new();
        for (state, count) in self.states() {
            let _ = write!(out, "{state:?}={count} ");
        }
        out.trim_end().to_owned()
    }
}

/// Groups members by group: `groups[i]` is member `i`'s group (or [`NONE`]). Returns the
/// offsets of each group's members (`offsets[g]..offsets[g + 1]`) and the members, in order
/// within each group: a counting sort, O(members + groups).
#[must_use]
pub fn group(groups: &[DirId], count: usize) -> (Vec<u32>, Vec<u32>) {
    let mut offsets = vec![0u32; count + 1];
    for &g in groups {
        if let Some(slot) = (g as usize)
            .checked_add(1)
            .and_then(|at| offsets.get_mut(at))
        {
            *slot += 1;
        }
    }
    for at in 1..offsets.len() {
        let previous = offsets.get(at - 1).copied().unwrap_or(0);
        if let Some(slot) = offsets.get_mut(at) {
            *slot += previous;
        }
    }
    let total = offsets.last().copied().unwrap_or(0) as usize;
    let mut members = vec![0u32; total];
    let mut next = offsets.clone();
    for (member, &g) in groups.iter().enumerate() {
        if let Some(slot) = next.get_mut(g as usize)
            && let Some(place) = members.get_mut(*slot as usize)
        {
            *place = u32::try_from(member).unwrap_or(u32::MAX);
            *slot += 1;
        }
    }
    (offsets, members)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;

    /// A directory record header (only the fields the index reads).
    fn header(sequence: u16, in_use: bool) -> RecordHeader {
        RecordHeader {
            signature: *b"FILE",
            update_sequence_offset: 48,
            update_sequence_count: 3,
            log_sequence_number: 0,
            sequence,
            link_count: 1,
            first_attribute: 56,
            flags: if in_use { 3 } else { 2 },
            used_size: 64,
            allocated_size: 1024,
            base_record: FileReference::default(),
            next_attribute_id: 0,
            record_number: None,
        }
    }

    fn utf16(text: &str) -> Vec<u8> {
        text.encode_utf16().flat_map(u16::to_le_bytes).collect()
    }

    /// Adds directory `record` named `name` under `parent`.
    fn add(
        batch: &mut DirectoryBatch,
        record: u64,
        sequence: u16,
        in_use: bool,
        parent: (u64, u16),
        name: &str,
    ) {
        let bytes = utf16(name);
        batch.add(
            record,
            &header(sequence, in_use),
            Some((
                FileReference::new(parent.0, parent.1),
                Namespace::Win32,
                Name::new(&bytes),
            )),
        );
    }

    fn tree() -> DirectoryBatch {
        let mut batch = DirectoryBatch::new();
        add(&mut batch, 5, 5, true, (5, 5), ".");
        add(&mut batch, 20, 1, true, (5, 5), "Windows");
        add(&mut batch, 21, 3, true, (20, 1), "System32");
        // Deleted, freed after its child recorded it: sequence 7 now, 6 recorded.
        add(&mut batch, 22, 7, false, (21, 3), "gone");
        // Reused: the child recorded sequence 1, the record is at 2 and in use.
        add(&mut batch, 23, 2, true, (5, 5), "reused");
        // A cycle.
        add(&mut batch, 30, 1, true, (31, 1), "a");
        add(&mut batch, 31, 1, true, (30, 1), "b");
        // Parent missing.
        add(&mut batch, 40, 1, true, (99, 1), "lost");
        batch
    }

    fn index(policy: PathPolicy) -> DirectoryIndex {
        let mut builder = IndexBuilder::new();
        builder.push(tree());
        builder.build(100, policy)
    }

    #[test]
    fn live_paths_follow_exact_links_between_directories_in_use() {
        let index = index(PathPolicy::LIVE);
        let system32 = index.directory(21).unwrap();
        assert_eq!(index.state(system32), Some(DirectoryState::Linked));
        assert_eq!(index.path(system32, '\\'), "Windows\\System32");
        assert_eq!(index.depth(system32), 2);
        assert_eq!(index.locate(FileReference::new(21, 3)), Some(system32));
        assert_eq!(index.locate(FileReference::new(21, 2)), None);
        assert_eq!(index.locate(FileReference::new(5, 5)), index.directory(5));
        assert_eq!(index.path(index.directory(5).unwrap(), '\\'), "");
        // Deleted directories are not parents here.
        assert_eq!(index.locate(FileReference::new(22, 6)), None);
        assert_eq!(
            index.state(index.directory(30).unwrap()),
            Some(DirectoryState::Cycle)
        );
        assert_eq!(
            index.state(index.directory(40).unwrap()),
            Some(DirectoryState::Orphaned)
        );
        let mut ancestors = Vec::new();
        index.ancestors(system32, &mut ancestors);
        assert_eq!(ancestors.len(), 2);
        assert_eq!(index.name_str(ancestors[0]), "Windows");
    }

    #[test]
    fn recovery_places_names_under_deleted_directories() {
        let index = index(PathPolicy::RECOVERY);
        let gone = index.directory(22).unwrap();
        assert_eq!(index.state(gone), Some(DirectoryState::Linked));
        assert_eq!(index.path(gone, '/'), "Windows/System32/gone");
        // A name recorded at 6 under the deleted directory now at 7 (freed after): placed.
        assert_eq!(index.locate(FileReference::new(22, 6)), Some(gone));
        assert_eq!(index.locate(FileReference::new(22, 7)), Some(gone));
        assert_eq!(index.locate(FileReference::new(22, 5)), None);
        // In use and reused: only its own sequence.
        assert_eq!(index.locate(FileReference::new(23, 1)), None);
    }

    #[test]
    fn strict_roots_and_depth_limits() {
        let index = index(PathPolicy::STRICT);
        assert!(index.locate(FileReference::new(21, 3)).is_some());
        let mut batch = DirectoryBatch::new();
        add(&mut batch, 5, 5, true, (5, 5), "root?");
        add(&mut batch, 20, 1, true, (5, 5), "x");
        let mut builder = IndexBuilder::new();
        builder.push(batch);
        let strict = builder.build(100, PathPolicy::STRICT);
        assert_eq!(strict.locate(FileReference::new(20, 1)), None);

        let shallow = PathPolicy {
            max_depth: 2,
            ..PathPolicy::LIVE
        };
        let index = index_with(shallow);
        // Windows (depth 1) holds names (depth 2); System32 (depth 2) is too deep for more.
        assert!(index.locate(FileReference::new(20, 1)).is_some());
        assert_eq!(
            index.state(index.directory(21).unwrap()),
            Some(DirectoryState::TooDeep)
        );
        assert_eq!(index.locate(FileReference::new(21, 3)), None);
    }

    fn index_with(policy: PathPolicy) -> DirectoryIndex {
        index(policy)
    }

    #[test]
    fn extension_names_name_unnamed_directories() {
        let mut batch = DirectoryBatch::new();
        add(&mut batch, 5, 5, true, (5, 5), ".");
        batch.add(50, &header(4, true), None);
        let bytes = utf16("late");
        batch.add_extension_name(
            FileReference::new(50, 4),
            FileReference::new(5, 5),
            Namespace::Win32,
            Name::new(&bytes),
        );
        let mut builder = IndexBuilder::new();
        builder.push(batch);
        let index = builder.build(100, PathPolicy::LIVE);
        let dir = index.directory(50).unwrap();
        assert_eq!(index.path(dir, '\\'), "late");
        // A deleted directory has only the names its own record holds.
        let mut batch = DirectoryBatch::new();
        add(&mut batch, 5, 5, true, (5, 5), ".");
        batch.add(50, &header(4, false), None);
        batch.add_extension_name(
            FileReference::new(50, 4),
            FileReference::new(5, 5),
            Namespace::Win32,
            Name::new(&bytes),
        );
        let mut builder = IndexBuilder::new();
        builder.push(batch);
        let index = builder.build(100, PathPolicy::RECOVERY);
        let dir = index.directory(50).unwrap();
        assert_eq!(index.state(dir), Some(DirectoryState::Unnamed));
    }

    #[test]
    fn directories_parsed_elsewhere_resolve_alike() {
        let name = |text: &str| utf16(text);
        let (root, windows) = (name("."), name("Windows"));
        let mut batch = DirectoryBatch::new();
        batch.push(
            5,
            5,
            true,
            Some((
                FileReference::new(5, 5),
                Namespace::Win32AndDos,
                Name::new(&root),
            )),
        );
        batch.push(
            20,
            1,
            true,
            Some((
                FileReference::new(5, 5),
                Namespace::Win32,
                Name::new(&windows),
            )),
        );
        batch.push(21, 3, true, None);
        let mut builder = IndexBuilder::new();
        builder.push(batch);
        let index = builder.build(100, PathPolicy::STRICT);
        let dir = index.locate(FileReference::new(20, 1)).unwrap();
        assert_eq!(index.path(dir, '\\'), "Windows");
        assert_eq!(
            index.state(index.directory(21).unwrap()),
            Some(DirectoryState::Unnamed)
        );
    }

    #[test]
    fn groups_are_a_stable_counting_sort() {
        let (offsets, members) = group(&[2, 0, NONE, 2, 1, 0], 3);
        assert_eq!(offsets, [0, 2, 3, 5]);
        assert_eq!(members, [1, 5, 4, 0, 3]);
    }

    #[test]
    fn deep_chains_resolve_without_recursion() {
        let mut batch = DirectoryBatch::new();
        add(&mut batch, 5, 5, true, (5, 5), ".");
        let depth = 100_000u64;
        // Records 10.. each a child of the previous, inserted deepest first.
        for at in (0..depth).rev() {
            let parent = if at == 0 { (5, 5) } else { (9 + at, 1) };
            add(&mut batch, 10 + at, 1, true, parent, "d");
        }
        let mut builder = IndexBuilder::new();
        builder.push(batch);
        let index = builder.build(depth + 20, PathPolicy::STRICT);
        let deepest = index.directory(10 + depth - 1).unwrap();
        assert_eq!(index.depth(deepest), 65_535);
        assert_eq!(index.state(deepest), Some(DirectoryState::TooDeep));
        let shallow = index.directory(10 + 99).unwrap();
        assert_eq!(index.depth(shallow), 100);
        assert_eq!(index.path(shallow, '/').len(), 100 * 2 - 1);
    }
}
