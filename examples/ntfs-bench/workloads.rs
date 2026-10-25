//! Workloads that read `$MFT` at three depths.
//!
//! - [`du`]: a disk-usage scan: live records only; names, sizes and two times;
//!   extension records folded in; the tree built and walked depth-first from the root.
//! - [`paths`]: every record, checked: every slot, a strict directory index, and a
//!   full path string for every name of every record, built in parallel and delivered in
//!   record order.
//! - [`inventory`]: everything, deleted included: every record in use or deleted, an object
//!   with every name (exact UTF-16), its placements (deleted directories included) and its
//!   streams.

use std::ops::ControlFlow;

use ntfs_ng::{
    FileReference,
    files::{FileOptions, Files},
    index::{DirectoryIndex, NONE, PathPolicy, group},
    mft::Mft,
    record::RecordHeader,
    records,
    scan::{Chunk, Entry, Projection, Scan},
    source::ReadAt,
    types,
    usage::Usage,
    value::{FileName, Namespace},
};

/// What a workload produced: counts to check and print.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Totals {
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub names: u64,
    pub resolved: u64,
    pub orphans: u64,
    pub path_bytes: u64,
}

// ---------------------------------------------------------------------------------------------
// du

/// The disk-usage workload.
pub(crate) fn du<S: ReadAt>(mft: &Mft<S>, threads: usize, chunk: usize) -> Totals {
    du_walk(mft, threads, chunk, None)
}

/// The disk-usage tree as sorted lines (`D <path>`, `F <path>\t<size>`), for parity checks.
pub(crate) fn du_dump<S: ReadAt>(mft: &Mft<S>, threads: usize, chunk: usize) -> Vec<String> {
    let mut lines = Vec::new();
    du_walk(mft, threads, chunk, Some(&mut lines));
    lines.sort();
    lines
}

fn du_walk<S: ReadAt>(
    mft: &Mft<S>,
    threads: usize,
    chunk: usize,
    mut lines: Option<&mut Vec<String>>,
) -> Totals {
    let trace = std::env::var_os("NTFS_BENCH_TRACE").is_some();
    let started = std::time::Instant::now();
    let phase = |name: &str| {
        if trace {
            eprintln!(
                "  {name:>8}: {:.1} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
    };
    let scan = Scan::new(mft).threads(threads).chunk_bytes(chunk);
    let Ok(usage) = Usage::scan(&scan, mft.records(), PathPolicy::LIVE) else {
        return Totals::default();
    };
    phase("usage");
    let workers = if threads == 0 {
        std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get)
    } else {
        threads
    };
    let parents = usage.parents(workers);
    phase("parents");
    let (offsets, members) = group(&parents, usage.index.len());
    phase("group");
    let index = &usage.index;
    let mut totals = Totals::default();
    let Some(root) = index.directory(records::ROOT) else {
        return totals;
    };
    let mut visited = vec![false; index.len()];
    let mut dir_paths: Vec<String> = if lines.is_some() {
        vec![String::new(); index.len()]
    } else {
        Vec::new()
    };
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let prefix = dir_paths.get(dir as usize).cloned().unwrap_or_default();
        let join = |name: &str| {
            if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}\\{name}")
            }
        };
        let start = offsets.get(dir as usize).copied().unwrap_or(0) as usize;
        let end = offsets.get(dir as usize + 1).copied().unwrap_or(0) as usize;
        for &member in members.get(start..end).unwrap_or_default() {
            let Some((file, name)) = usage.entry(member) else {
                continue;
            };
            totals.names += 1;
            totals.path_bytes += name.len() as u64;
            if file.is_directory() {
                if let Some(child) = index.directory(file.record)
                    && let Some(seen) = visited.get_mut(child as usize)
                    && !*seen
                {
                    *seen = true;
                    totals.directories += 1;
                    if let Some(lines) = lines.as_deref_mut() {
                        let full = join(name);
                        lines.push(format!("D {full}"));
                        if let Some(slot) = dir_paths.get_mut(child as usize) {
                            *slot = full;
                        }
                    }
                    stack.push(child);
                }
            } else {
                totals.files += 1;
                totals.bytes += file.size;
                totals.resolved += u64::from(file.created > 0);
                if let Some(lines) = lines.as_deref_mut() {
                    lines.push(format!("F {}\t{}", join(name), file.size));
                }
            }
        }
    }
    phase("walk");
    totals.orphans = parents.iter().filter(|parent| **parent == NONE).count() as u64;
    totals
}

// ---------------------------------------------------------------------------------------------
// paths

/// A row: a record's name and its full path.
#[derive(Debug)]
pub(crate) struct Row {
    #[expect(
        dead_code,
        reason = "a row carries its record; the workload builds rows whole, and keeps only \
                  counts"
    )]
    pub record: u64,
    pub path: Option<String>,
    pub raw: Vec<u8>,
}

struct PathsProjection<'a> {
    index: &'a DirectoryIndex,
    raw: bool,
}

impl Projection for PathsProjection<'_> {
    type Output = Vec<Row>;
    type Scratch = ();

    fn scratch(&self) {}

    fn begin(&self, (): &mut (), chunk: Chunk) -> Vec<Row> {
        Vec::with_capacity(chunk.records)
    }

    #[inline]
    fn entry(&self, rows: &mut Vec<Row>, (): &mut (), number: u64, entry: Entry<'_>) {
        let raw = match &entry {
            Entry::Record(record) if self.raw => {
                let mut raw = Vec::with_capacity(record.data().len());
                record.raw_into(&mut raw);
                raw
            }
            Entry::Damaged { raw, .. } if self.raw => raw.to_vec(),
            _ => Vec::new(),
        };
        let Entry::Record(record) = entry else {
            rows.push(Row {
                record: number,
                path: None,
                raw,
            });
            return;
        };
        // Every name of the record (DOS aliases aside), each with its path.
        let mut path = None;
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
            let Ok((parent, namespace, name)) = FileName::link(value) else {
                continue;
            };
            if namespace.is_dos_only() || path.is_some() {
                continue;
            }
            if let Some(dir) = self.index.locate(parent) {
                let mut text = String::with_capacity(64);
                text.push_str(".\\");
                self.index.write_path(dir, '\\', &mut text);
                if text.len() > 2 {
                    text.push('\\');
                }
                name.push_lossy(&mut text);
                path = Some(text);
            }
        }
        rows.push(Row {
            record: number,
            path,
            raw,
        });
    }
}

/// Every record as a row with its path: two passes, rows in record order.
pub(crate) fn paths<S: ReadAt>(mft: &Mft<S>, threads: usize, chunk: usize, raw: bool) -> Totals {
    let scan = Scan::new(mft).threads(threads).chunk_bytes(chunk);
    let Ok(index) = DirectoryIndex::scan(&scan, mft.records(), PathPolicy::STRICT) else {
        return Totals::default();
    };
    let mut totals = Totals::default();
    let _ = scan.run(&PathsProjection { index: &index, raw }, |_, rows| {
        for row in rows {
            totals.names += 1;
            match &row.path {
                Some(path) => {
                    totals.resolved += 1;
                    totals.path_bytes += path.len() as u64;
                }
                None => totals.orphans += 1,
            }
            totals.bytes += row.raw.len() as u64;
            drop(row);
        }
        ControlFlow::Continue(())
    });
    totals.directories = index.len() as u64;
    totals
}

// ---------------------------------------------------------------------------------------------
// inventory

/// An object: a record in use or deleted, with every name and stream.
#[derive(Debug)]
pub(crate) struct Object {
    #[expect(
        dead_code,
        reason = "an object carries its identity; the workload builds objects whole, and \
                  keeps only counts"
    )]
    pub record: FileReference,
    pub in_use: bool,
    pub names: Vec<(Vec<u16>, FileReference, Namespace)>,
    pub placements: Vec<Option<String>>,
    pub streams: Vec<(u32, Vec<u16>, u64)>,
}

struct InventoryProjection<'a> {
    index: &'a DirectoryIndex,
}

impl Projection for InventoryProjection<'_> {
    type Output = Vec<Object>;
    type Scratch = String;

    fn scratch(&self) -> String {
        String::with_capacity(512)
    }

    fn begin(&self, _: &mut String, chunk: Chunk) -> Vec<Object> {
        Vec::with_capacity(chunk.records)
    }

    #[inline]
    fn entry(&self, objects: &mut Vec<Object>, path: &mut String, number: u64, entry: Entry<'_>) {
        let Entry::Record(record) = entry else {
            return;
        };
        let header: &RecordHeader = record.header();
        if !header.is_base() {
            return;
        }
        let mut object = Object {
            record: record.reference(number),
            in_use: header.in_use(),
            names: Vec::new(),
            placements: Vec::new(),
            streams: Vec::new(),
        };
        for attribute in record.attributes() {
            let Ok(attribute) = attribute else {
                break;
            };
            let type_code = attribute.type_code();
            if type_code == types::FILE_NAME
                && let Ok(value) = attribute.value()
                && let Ok((parent, namespace, name)) = FileName::link(value)
            {
                if !namespace.is_dos_only() {
                    let placement = self.index.locate(parent).map(|dir| {
                        path.clear();
                        self.index.write_path(dir, '\\', path);
                        if !path.is_empty() {
                            path.push('\\');
                        }
                        name.push_lossy(path);
                        path.clone()
                    });
                    object.placements.push(placement);
                }
                object
                    .names
                    .push((name.units().collect(), parent, namespace));
            }
            let size = attribute.data_size().unwrap_or(0);
            let name: Vec<u16> = attribute
                .name()
                .map(|name| name.units().collect())
                .unwrap_or_default();
            object.streams.push((type_code, name, size));
        }
        objects.push(object);
    }
}

/// Every record in use or deleted as an object: two passes, objects as they finish.
pub(crate) fn inventory<S: ReadAt>(mft: &Mft<S>, threads: usize, chunk: usize) -> Totals {
    let scan = Scan::new(mft).threads(threads).chunk_bytes(chunk);
    let Ok(index) = DirectoryIndex::scan(&scan, mft.records(), PathPolicy::RECOVERY) else {
        return Totals::default();
    };
    let mut totals = Totals::default();
    let _ = scan
        .ordered(false)
        .run(&InventoryProjection { index: &index }, |_, objects| {
            for object in objects {
                if object.in_use {
                    totals.files += 1;
                } else {
                    totals.bytes += 1;
                }
                totals.names += object.names.len() as u64;
                for placement in &object.placements {
                    match placement {
                        Some(path) => {
                            totals.resolved += 1;
                            totals.path_bytes += path.len() as u64;
                        }
                        None => totals.orphans += 1,
                    }
                }
            }
            ControlFlow::Continue(())
        });
    totals.directories = index.len() as u64;
    totals
}

/// Directories only: the first pass of [`paths`] and [`inventory`].
pub(crate) fn directories<S: ReadAt>(
    mft: &Mft<S>,
    threads: usize,
    chunk: usize,
    policy: PathPolicy,
) -> Totals {
    let scan = Scan::new(mft).threads(threads).chunk_bytes(chunk);
    let index = DirectoryIndex::scan(&scan, mft.records(), policy).ok();
    Totals {
        directories: index.as_ref().map_or(0, |index| index.len() as u64),
        ..Totals::default()
    }
}

// ---------------------------------------------------------------------------------------------
// parity dumps

struct PathsDump<'a> {
    index: &'a DirectoryIndex,
}

impl Projection for PathsDump<'_> {
    type Output = Vec<String>;
    type Scratch = ();

    fn scratch(&self) {}

    fn begin(&self, (): &mut (), chunk: Chunk) -> Vec<String> {
        Vec::with_capacity(chunk.records)
    }

    fn entry(&self, lines: &mut Vec<String>, (): &mut (), number: u64, entry: Entry<'_>) {
        let Entry::Record(record) = entry else {
            return;
        };
        let mut long: Vec<(FileReference, String)> = Vec::new();
        let mut first = None;
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
            let Ok((parent, namespace, name)) = FileName::link(value) else {
                continue;
            };
            if namespace.is_dos_only() {
                continue;
            }
            let text = name.to_string_lossy();
            if first.is_none() {
                first = Some((parent, text.clone()));
            }
            long.push((parent, text));
        }
        long.sort();
        long.dedup();
        let path = if number == records::ROOT {
            Some(String::new())
        } else {
            first.and_then(|(parent, name)| {
                let dir = self.index.locate(parent)?;
                let mut path = String::from("\\");
                self.index.write_path(dir, '\\', &mut path);
                if path.len() > 1 {
                    path.push('\\');
                }
                path.push_str(&name);
                Some(path)
            })
        };
        lines.push(format!(
            "{number}\t{}\t{}",
            long.len(),
            path.as_deref().unwrap_or("-")
        ));
    }
}

/// `record<TAB>long names<TAB>path` for every record, strict policy, in record order.
pub(crate) fn paths_dump<S: ReadAt>(mft: &Mft<S>, threads: usize, chunk: usize) -> Vec<String> {
    let scan = Scan::new(mft).threads(threads).chunk_bytes(chunk);
    let Ok(index) = DirectoryIndex::scan(&scan, mft.records(), PathPolicy::STRICT) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let _ = scan.run(&PathsDump { index: &index }, |_, mut chunk| {
        lines.append(&mut chunk);
        ControlFlow::Continue(())
    });
    lines
}

/// `record<TAB>state<TAB>path|ORPHAN` for every placed name of every record in use or deleted
/// (DOS names never placed), the recovery policy at a depth of 254 components, sorted.
pub(crate) fn inventory_dump<S: ReadAt>(mft: &Mft<S>, threads: usize, chunk: usize) -> Vec<String> {
    let scan = Scan::new(mft).threads(threads).chunk_bytes(chunk);
    let options = FileOptions {
        deleted: true,
        metadata: true,
        dos_aliases: true,
        policy: PathPolicy {
            max_depth: 254,
            ..PathPolicy::RECOVERY
        },
    };
    let Ok(files) = Files::scan(&scan, mft.records(), options) else {
        return Vec::new();
    };
    let mut lines = Vec::new();
    let mut path = String::new();
    for (id, file) in files.table.files() {
        let state = if file.in_use() { "in-use" } else { "deleted" };
        if file.record == records::ROOT {
            lines.push(format!("{}\t{state}\t", file.record));
            continue;
        }
        for link in files.table.links_of(id) {
            if link.namespace.is_dos_only() {
                continue;
            }
            match files.index.locate(link.parent) {
                Some(dir) => {
                    path.clear();
                    files.index.write_path(dir, '\\', &mut path);
                    if !path.is_empty() {
                        path.push('\\');
                    }
                    path.push_str(link.name);
                    lines.push(format!("{}\t{state}\t{path}", file.record));
                }
                None => lines.push(format!("{}\t{state}\tORPHAN", file.record)),
            }
        }
    }
    lines.sort();
    lines
}
