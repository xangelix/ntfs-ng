//! Live NTFS volumes on Windows, read raw and checked against what Windows reports of the same
//! files. The `Windows live volumes` workflow prepares a volume with
//! `scripts/windows-live-fixtures.ps1` and runs these with `--ignored`: they need an
//! administrator and that volume, so they are ignored elsewhere.
//!
//! - `NTFS_NG_LIVE_VOLUME`: the prepared volume (`N:`), its fixtures in `N:\fixtures`.
//! - `NTFS_NG_LIVE_RECORD_SIZE`: the file record size it was formatted with.
//! - `NTFS_NG_LIVE_SHADOW`: a shadow copy of it (`\\?\GLOBALROOT\Device\…`), taken before
//!   `fixtures\snapshot` changed.

#![cfg(windows)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::print_stdout
)]

use std::{
    collections::HashMap,
    os::windows::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Instant,
};

use ntfs_ng::{
    files::{FileOptions, Files, file_flags},
    index::{NONE, PathPolicy},
    scan::Scan,
    source::{Device, DeviceOptions, ReadAt},
    types,
    usage::Usage,
    volume::{
        Volume,
        stream::{HoleKind, Piece, StreamReader},
    },
};

const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;
const FILE_ATTRIBUTE_COMPRESSED: u32 = 0x800;
/// Hard links of `fixtures\links\many\many.bin`: too many names for one record.
const MANY_LINKS: usize = 300;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set: see the module docs"))
}

/// The prepared volume's letter, `N:`.
fn letter() -> String {
    env("NTFS_NG_LIVE_VOLUME").trim_end_matches('\\').to_owned()
}

/// `path`, a volume device, opened raw.
fn open(path: &str, unbuffered: bool) -> Volume<Device> {
    let options = DeviceOptions {
        unbuffered,
        ..DeviceOptions::default()
    };
    let device = Device::open(path, options).unwrap_or_else(|error| panic!("{path}: {error}"));
    Volume::open(device).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The files of a volume, and each name's path below its root (`\`-separated, without a
/// leading `\`), to its file's id: the names of files in use, or of deleted ones.
struct Names {
    files: Files,
    in_use: HashMap<String, u32>,
    deleted: HashMap<String, u32>,
}

fn names<S: ReadAt>(volume: &Volume<S>) -> Names {
    let scan = Scan::new(volume.mft());
    let options = FileOptions {
        deleted: true,
        metadata: false,
        dos_aliases: false,
        policy: PathPolicy::RECOVERY,
    };
    let files = Files::scan(&scan, volume.mft().records(), options).unwrap();
    let parents = files.parents(4);
    let (mut in_use, mut deleted) = (HashMap::new(), HashMap::new());
    for (id, link) in files.table.links() {
        let dir = parents[id as usize];
        if dir == NONE {
            continue;
        }
        let mut path = files.index.path(dir, '\\');
        if !path.is_empty() {
            path.push('\\');
        }
        path.push_str(link.name);
        let file = files.table.file(link.file).unwrap();
        if file.in_use() {
            in_use.insert(path, link.file);
        } else {
            deleted.insert(path, link.file);
        }
    }
    Names {
        files,
        in_use,
        deleted,
    }
}

/// Stream `name` (`""` for the unnamed one) of record `number`, whole, and the kinds of its
/// holes (read as zeros).
fn read<S: ReadAt>(volume: &Volume<S>, number: u64, name: &str) -> (Vec<u8>, Vec<HoleKind>) {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut buf = Vec::new();
    let record = volume.record(number, &mut buf).unwrap();
    for attribute in record.attributes() {
        let attribute = attribute.unwrap();
        if attribute.type_code() == types::DATA
            && attribute.name().unwrap().eq_units(&units)
            && attribute.is_resident()
        {
            return (attribute.value().unwrap().to_vec(), Vec::new());
        }
    }
    let list = volume.attribute_list(&record).unwrap();
    let (map, sizes) = volume
        .runs(number, &record, list.as_deref(), types::DATA, &units)
        .unwrap()
        .unwrap_or_else(|| panic!("record {number} has no stream {name:?}"));
    let mut out = vec![0u8; usize::try_from(sizes.data).unwrap()];
    let mut holes = Vec::new();
    StreamReader::new(volume, map, sizes)
        .read(0..u64::MAX, None, &mut |offset, piece| -> Result<(), ()> {
            match piece {
                Piece::Data(bytes) => {
                    let at = usize::try_from(offset).unwrap();
                    out[at..at + bytes.len()].copy_from_slice(bytes);
                }
                Piece::Hole(_, kind) => holes.push(kind),
            }
            Ok(())
        })
        .unwrap();
    (out, holes)
}

/// Every entry below `dir`, symbolic links and junctions not followed.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        out.push(path.clone());
        if meta.is_dir() && !meta.file_type().is_symlink() {
            walk(&path, out);
        }
    }
}

/// Every fixture, as Windows reports it, is what the volume holds: each name placed, each size,
/// each flag, each stream's bytes; hard links, extension records, compressed, sparse and
/// fragmented streams, alternate streams, reparse points and deleted files included. Read
/// through the cache and around it.
#[test]
#[ignore = "needs an administrator and a volume prepared by scripts/windows-live-fixtures.ps1"]
fn fixtures_read_back() {
    let letter = letter();
    let root = PathBuf::from(format!("{letter}\\"));
    let mut entries = Vec::new();
    walk(&root.join("fixtures"), &mut entries);
    assert!(entries.len() > MANY_LINKS, "{} fixtures", entries.len());
    for unbuffered in [false, true] {
        let volume = open(&format!(r"\\.\{letter}"), unbuffered);
        let record_size: u32 = env("NTFS_NG_LIVE_RECORD_SIZE").parse().unwrap();
        assert_eq!(volume.mft().record_size(), record_size);
        let names = names(&volume);
        let table = &names.files.table;
        let (mut compressed, mut sparse, mut listed) = (0, 0, 0);
        for path in &entries {
            let relative = path.strip_prefix(&root).unwrap().to_str().unwrap();
            let &id = names
                .in_use
                .get(relative)
                .unwrap_or_else(|| panic!("{relative} was not found"));
            let file = table.file(id).unwrap();
            let meta = std::fs::symlink_metadata(path).unwrap();
            if meta.file_type().is_symlink() {
                assert_ne!(file.flags & file_flags::LINK, 0, "{relative}");
                continue;
            }
            if meta.is_dir() {
                assert!(file.is_directory(), "{relative}");
                continue;
            }
            assert_eq!(file.size, meta.len(), "{relative}");
            let attributes = meta.file_attributes();
            assert_eq!(
                attributes & FILE_ATTRIBUTE_COMPRESSED != 0,
                file.flags & file_flags::COMPRESSED != 0,
                "{relative}"
            );
            assert_eq!(
                attributes & FILE_ATTRIBUTE_SPARSE_FILE != 0,
                file.flags & file_flags::SPARSE != 0,
                "{relative}"
            );
            let (bytes, holes) = read(&volume, file.record, "");
            assert!(bytes == std::fs::read(path).unwrap(), "{relative}: bytes");
            if file.flags & file_flags::COMPRESSED != 0 {
                compressed += 1;
                assert!(!holes.contains(&HoleKind::Undecodable), "{relative}");
            }
            if file.flags & file_flags::SPARSE != 0 {
                sparse += 1;
                assert!(holes.contains(&HoleKind::Sparse), "{relative}: {holes:?}");
            }
            listed += usize::from(file.flags & file_flags::ATTRIBUTE_LIST != 0);
        }
        assert!(
            compressed >= 2 && sparse >= 1,
            "{compressed} compressed, {sparse} sparse"
        );
        assert!(
            listed >= MANY_LINKS,
            "{listed} names of files with an attribute list"
        );

        // Alternate data streams, resident and not.
        let ads = root.join(r"fixtures\streams\ads.txt");
        let id = names.in_use[r"fixtures\streams\ads.txt"];
        let file = table.file(id).unwrap();
        assert_ne!(file.flags & file_flags::STREAMS, 0);
        for stream in ["note", "big"] {
            let expected = std::fs::read(format!("{}:{stream}", ads.display())).unwrap();
            assert!(read(&volume, file.record, stream).0 == expected, "{stream}");
        }

        // Every hard link of one file, held in extension records.
        let many: Vec<_> = names
            .in_use
            .iter()
            .filter(|(path, _)| path.starts_with(r"fixtures\links\many\"))
            .map(|(_, &id)| id)
            .collect();
        assert_eq!(many.len(), MANY_LINKS + 1);
        assert!(many.iter().all(|&id| id == many[0]));
        let file = table.file(many[0]).unwrap();
        assert_ne!(file.flags & file_flags::ATTRIBUTE_LIST, 0);
        assert_eq!(table.links_of(many[0]).count(), MANY_LINKS + 1);

        // A stream in more extents than its record holds: runs in extension records.
        let id = names.in_use[r"fixtures\fragmented\big.bin"];
        let file = table.file(id).unwrap();
        assert_ne!(file.flags & file_flags::ATTRIBUTE_LIST, 0);

        // Deleted files keep their names until their records are used again.
        let gone = names
            .deleted
            .keys()
            .filter(|path| path.starts_with(r"fixtures\deleted\gone-"))
            .count();
        assert!(
            gone > 0,
            "no deleted fixture was found among {} deleted names, such as {:?}",
            names.deleted.len(),
            names.deleted.keys().take(5).collect::<Vec<_>>()
        );
        assert!(
            !names
                .in_use
                .keys()
                .any(|path| path.starts_with(r"fixtures\deleted\gone-"))
        );

        // The disk-usage table sizes every live name as the file table does.
        let usage = Usage::scan(
            &Scan::new(volume.mft()),
            volume.mft().records(),
            PathPolicy::LIVE,
        )
        .unwrap();
        let parents = usage.parents(4);
        let mut checked = 0;
        for (id, entry, name) in usage.entries() {
            let dir = parents[id as usize];
            if dir == NONE || entry.is_alias() || entry.is_directory() {
                continue;
            }
            let mut path = usage.index.path(dir, '\\');
            path.push('\\');
            path.push_str(name);
            if let Some(&file) = names.in_use.get(&path) {
                assert_eq!(entry.size, table.file(file).unwrap().size, "{path}");
                checked += 1;
            }
        }
        assert!(checked > MANY_LINKS, "{checked} names sized");
    }
}

/// A shadow copy holds the volume as it was: a file changed since reads as it was, and a file
/// deleted since is still there.
#[test]
#[ignore = "needs an administrator and a shadow copy of a prepared volume"]
fn shadow_copies_hold_the_volume_as_it_was() {
    let letter = letter();
    let shadow = open(&env("NTFS_NG_LIVE_SHADOW"), false);
    let live = open(&format!(r"\\.\{letter}"), false);
    for (volume, expected, removed_in_use) in [
        (&shadow, &b"before the snapshot"[..], true),
        (&live, &b"after the snapshot"[..], false),
    ] {
        let names = names(volume);
        let id = names.in_use[r"fixtures\snapshot\changed.txt"];
        let file = names.files.table.file(id).unwrap();
        assert_eq!(read(volume, file.record, "").0, expected);
        assert_eq!(
            names.in_use.contains_key(r"fixtures\snapshot\removed.txt"),
            removed_in_use
        );
    }
}

/// The system volume reads whole, buffered and not: every name in use placed, a known file
/// sized as Windows sizes it, and the time each part takes printed.
#[test]
#[ignore = "needs an administrator"]
fn the_system_volume_reads_whole() {
    let system = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_owned());
    let kernel = PathBuf::from(format!(r"{system}\Windows\System32\kernel32.dll"));
    let expected = std::fs::metadata(&kernel).unwrap().len();
    for unbuffered in [false, true] {
        let volume = open(&format!(r"\\.\{system}"), unbuffered);
        let started = Instant::now();
        let names = names(&volume);
        let files = started.elapsed();
        let started = Instant::now();
        let usage = Usage::scan(
            &Scan::new(volume.mft()),
            volume.mft().records(),
            PathPolicy::LIVE,
        )
        .unwrap();
        let du = started.elapsed();
        println!(
            "{system} unbuffered={unbuffered}: {} records of {} bytes, {} names in use; \
             files {files:?}, disk usage {du:?} ({} names)",
            volume.mft().records(),
            volume.mft().record_size(),
            names.in_use.len(),
            usage.len(),
        );
        assert!(names.in_use.len() > 50_000, "{} names", names.in_use.len());
        let (_, &id) = names
            .in_use
            .iter()
            .find(|(path, _)| path.eq_ignore_ascii_case(r"Windows\System32\kernel32.dll"))
            .expect("kernel32.dll was not found");
        assert_eq!(names.files.table.file(id).unwrap().size, expected);
    }
}
