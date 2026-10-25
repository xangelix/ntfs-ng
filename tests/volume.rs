//! Volumes end to end: synthetic images read back through `Volume`, `Mft`, the stream reader
//! and `$Bitmap`.

#![cfg(feature = "testing")]
#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::expect_used
)]

use ntfs_ng::{
    FileReference, Record,
    files::{FileOptions, Files},
    index::PathPolicy,
    scan::{Entry, Scan},
    testing::{
        Rng,
        image::{Content, FileSpec, Image, ImageBuilder, ImageConfig, ROOT},
    },
    types,
    volume::{
        Volume,
        bitmap::ClusterBitmap,
        stream::{HoleKind, Piece, StreamReader},
    },
};

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

fn text(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| b"the quick brown fox jumps over "[i % 31])
        .collect()
}

/// Every stream kind, in a volume with `$MFT` in `fragments` extents.
fn image(fragments: usize, in_extensions: bool) -> (Image, Vec<FileSpec>) {
    let mut builder = ImageBuilder::new(ImageConfig {
        mft_fragments: fragments,
        mft_in_extensions: in_extensions,
        ..ImageConfig::default()
    });
    let docs = builder.add(FileSpec::directory("docs", ROOT));
    let gone = builder.add(FileSpec {
        deleted: true,
        ..FileSpec::directory("gone", docs)
    });
    let specs = vec![
        FileSpec::file("resident.txt", docs, Content::Resident(b"small".to_vec())),
        FileSpec::file(
            "contiguous.bin",
            docs,
            Content::Contiguous(noise(50_000, 1)),
        ),
        FileSpec::file(
            "fragmented.bin",
            docs,
            Content::Fragmented(noise(70_000, 2), 4),
        ),
        FileSpec::file(
            "sparse.bin",
            ROOT,
            Content::Sparse {
                len: 200_000,
                pieces: vec![(8192, noise(5000, 3)), (100 * 1024, noise(4096, 4))],
            },
        ),
        FileSpec::file("compressed.txt", ROOT, Content::Compressed(text(150_000))),
        FileSpec::file(
            "incompressible.bin",
            ROOT,
            Content::Compressed(noise(70_000, 5)),
        ),
        FileSpec::file(
            "uninitialized.bin",
            ROOT,
            Content::Uninitialized {
                data: noise(20_000, 6),
                initialized: 9000,
            },
        ),
        FileSpec {
            extension: true,
            ..FileSpec::file("extended.bin", docs, Content::Contiguous(noise(30_000, 7)))
        },
        FileSpec {
            deleted: true,
            ..FileSpec::file("deleted.bin", gone, Content::Contiguous(noise(12_288, 8)))
        },
        FileSpec {
            streams: vec![(
                "Zone.Identifier".to_owned(),
                Content::Resident(b"[ZoneTransfer]".to_vec()),
            )],
            ..FileSpec::file("with-ads.txt", docs, Content::Resident(b"main".to_vec()))
        },
        FileSpec::file("empty.txt", ROOT, Content::Empty),
    ];
    for spec in &specs {
        builder.add(spec.clone());
    }
    // A deleted file whose clusters a new file took since.
    let freed = builder.add(FileSpec {
        deleted: true,
        ..FileSpec::file("freed.bin", docs, Content::Contiguous(noise(8192, 9)))
    });
    builder.add(FileSpec {
        takes_clusters_of: Some(freed),
        ..FileSpec::file("taker.bin", docs, Content::Contiguous(noise(8192, 10)))
    });
    (builder.build(), specs)
}

/// Reads stream `name` (`""` for the unnamed one) of record `number` whole.
fn read<S: ntfs_ng::source::ReadAt>(
    volume: &Volume<S>,
    number: u64,
    name: &str,
    bitmap: Option<&mut ClusterBitmap>,
) -> (Vec<u8>, Vec<(u64, u64, HoleKind)>) {
    let units: Vec<u16> = name.encode_utf16().collect();
    let mut buf = Vec::new();
    let record = volume.record(number, &mut buf).unwrap();
    let list = volume.attribute_list(&record).unwrap();
    // Resident first.
    for attribute in record.attributes() {
        let attribute = attribute.unwrap();
        if attribute.type_code() == types::DATA
            && attribute.name().unwrap().eq_units(&units)
            && attribute.is_resident()
        {
            return (attribute.value().unwrap().to_vec(), Vec::new());
        }
    }
    let (map, sizes) = volume
        .runs(number, &record, list.as_deref(), types::DATA, &units)
        .unwrap()
        .unwrap_or_else(|| panic!("record {number} has no stream {name:?}"));
    let mut reader = StreamReader::new(volume, map, sizes);
    let mut out = vec![0u8; sizes.data as usize];
    let mut holes = Vec::new();
    reader
        .read(0..u64::MAX, bitmap, &mut |offset,
                                         piece|
         -> Result<(), ()> {
            match piece {
                Piece::Data(bytes) => {
                    out[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes)
                }
                Piece::Hole(len, kind) => holes.push((offset, len, kind)),
            }
            Ok(())
        })
        .unwrap();
    (out, holes)
}

#[test]
fn every_stream_kind_reads_back() {
    for (fragments, in_extensions) in [(1, false), (2, false), (3, false), (2, true), (3, true)] {
        let (image, specs) = image(fragments, in_extensions);
        let volume = Volume::open(image.bytes.as_slice()).unwrap();
        assert!(
            volume.mft().problems().is_empty(),
            "{:?}",
            volume.mft().problems()
        );
        assert_eq!(volume.boot().record_size, 1024);
        assert_eq!(volume.label().unwrap(), "synthetic");
        assert_eq!(volume.information().unwrap().major, 3);
        assert_eq!(volume.mft().records(), image.config.mft_records);
        // The specs after the two directories.
        for (spec, reference) in specs.iter().zip(&image.records[2..]) {
            if spec.deleted {
                continue;
            }
            let (bytes, holes) = read(&volume, reference.record, "", None);
            assert_eq!(
                bytes,
                spec.content.expected(),
                "{} ({fragments}, {in_extensions})",
                spec.name
            );
            for (offset, len, kind) in &holes {
                assert!(
                    matches!(kind, HoleKind::Sparse | HoleKind::Uninitialized),
                    "{}: {kind:?} at {offset}+{len}",
                    spec.name
                );
            }
            for (name, content) in &spec.streams {
                assert_eq!(
                    read(&volume, reference.record, name, None).0,
                    content.expected()
                );
            }
        }
    }
}

#[test]
fn holes_are_reported_not_invented() {
    let (image, specs) = image(1, false);
    let volume = Volume::open(image.bytes.as_slice()).unwrap();
    let sparse = specs
        .iter()
        .position(|spec| spec.name == "sparse.bin")
        .unwrap();
    let (_, holes) = read(&volume, image.records[2 + sparse].record, "", None);
    let sparse_bytes: u64 = holes
        .iter()
        .filter(|(.., kind)| *kind == HoleKind::Sparse)
        .map(|(_, len, _)| len)
        .sum();
    assert_eq!(
        sparse_bytes,
        200_000 - 5000_u64.next_multiple_of(4096) - 4096
    );
    let uninitialized = specs
        .iter()
        .position(|spec| spec.name == "uninitialized.bin")
        .unwrap();
    let (_, holes) = read(&volume, image.records[2 + uninitialized].record, "", None);
    assert_eq!(holes, vec![(9000, 11_000, HoleKind::Uninitialized)]);
}

/// Sizes a header declares never cost work per byte or per compression unit: a stream of an
/// exabyte, its runs mapped past the volume, read with a small budget, costs a few pieces.
#[test]
fn hostile_sizes_cost_runs_not_units() {
    let (image, _) = image(1, false);
    let volume = Volume::open(image.bytes.as_slice()).unwrap();
    let cluster = volume.cluster_size();
    let huge = 1u64 << 60;
    let cases = [
        (true, 1, 2 * huge),
        (false, 1, 2 * huge),
        (true, 1 << 40, 2 * huge),
        (false, 1 << 40, 2 * huge),
        // Sizes that reach the end of the address space: the last unit ends there.
        (true, 1, u64::MAX),
        (false, 1, u64::MAX),
    ];
    for (compressed, lcn, size) in cases {
        let mut map = ntfs_ng::mft::ExtentMap::default();
        let runs = [
            ntfs_ng::Run {
                vcn: 0,
                length: huge / cluster,
                lcn: Some(lcn),
            },
            ntfs_ng::Run {
                vcn: huge / cluster,
                length: huge / cluster,
                lcn: None,
            },
        ];
        map.add_runs(runs.into_iter().map(Ok), cluster).unwrap();
        map.finish();
        let sizes = ntfs_ng::volume::StreamSizes {
            allocated: size,
            data: size,
            initialized: size,
            compression_unit: 4,
            compressed,
            sparse: false,
        };
        let mut reader = StreamReader::new(&volume, map, sizes).budget(1 << 20);
        let (mut pieces, mut covered) = (0u64, 0u64);
        reader
            .read(0..u64::MAX, None, &mut |offset, piece| -> Result<(), ()> {
                assert_eq!(offset, covered);
                covered += match piece {
                    Piece::Data(bytes) => bytes.len() as u64,
                    Piece::Hole(len, _) => len,
                };
                pieces += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(covered, size, "compressed {compressed}, lcn {lcn}");
        assert!(
            pieces < 16,
            "compressed {compressed}, lcn {lcn}: {pieces} pieces"
        );
    }
}

#[test]
fn deleted_streams_never_read_another_files_clusters() {
    let (image, specs) = image(1, false);
    let volume = Volume::open(image.bytes.as_slice()).unwrap();
    let mut bitmap = ClusterBitmap::open(&volume).unwrap();
    // `freed.bin` is the spec after the listed ones; `taker.bin` now owns its clusters.
    let freed = image.records[2 + specs.len()];
    let (bytes, holes) = read(&volume, freed.record, "", Some(&mut bitmap));
    assert_eq!(holes.len(), 1, "{holes:?}");
    assert_eq!(holes[0], (0, 8192, HoleKind::Reallocated));
    assert!(bytes.iter().all(|b| *b == 0));
    // A deleted file whose clusters nobody took reads whole, the bitmap checked.
    let deleted = specs
        .iter()
        .position(|spec| spec.name == "deleted.bin")
        .unwrap();
    let (bytes, holes) = read(
        &volume,
        image.records[2 + deleted].record,
        "",
        Some(&mut bitmap),
    );
    assert!(holes.is_empty(), "{holes:?}");
    assert_eq!(bytes, specs[deleted].content.expected());
}

#[test]
fn files_and_paths_of_a_volume() {
    let (image, specs) = image(3, true);
    let volume = Volume::open(image.bytes.as_slice()).unwrap();
    let scan = Scan::new(volume.mft()).threads(3).chunk_bytes(16 << 10);
    let files = Files::scan(
        &scan,
        volume.mft().records(),
        FileOptions {
            deleted: true,
            metadata: false,
            dos_aliases: true,
            policy: PathPolicy::RECOVERY,
        },
    )
    .unwrap();
    let table = &files.table;
    let parents = files.parents(2);
    for (spec, reference) in specs.iter().zip(&image.records[2..]) {
        let id = table.find(reference.record).unwrap();
        let file = table.file(id).unwrap();
        assert_eq!(file.in_use(), !spec.deleted, "{}", spec.name);
        assert_eq!(
            file.size,
            spec.content.expected().len() as u64,
            "{}",
            spec.name
        );
        let names: Vec<_> = table.links_of(id).map(|link| link.name).collect();
        assert_eq!(names, vec![spec.name.as_str()]);
        // Every name is placed, deleted ones under their deleted directory too.
        let (link, _) = table.links().find(|(_, link)| link.file == id).unwrap();
        let dir = parents[table.links().position(|(other, _)| other == link).unwrap()];
        let path = files.index.path(dir, '/');
        let expected = match spec.parent {
            ROOT => String::new(),
            0 => "docs".to_owned(),
            _ => "docs/gone".to_owned(),
        };
        assert_eq!(path, expected, "{}", spec.name);
    }
}

#[test]
fn records_round_trip_and_number_checks_hold() {
    let (image, _) = image(2, true);
    let volume = Volume::open(image.bytes.as_slice()).unwrap();
    let mut buf = Vec::new();
    let record = volume.record(5, &mut buf).unwrap();
    assert!(record.is_directory());
    assert_eq!(record.reference(5), FileReference::new(5, 5));
    // Read every slot through the scan: no damaged or unmapped slots in a clean image.
    let mut damaged = 0;
    let mut unmapped = 0;
    let projection = Counting;
    Scan::new(volume.mft())
        .threads(2)
        .chunk_bytes(8 << 10)
        .run(&projection, |_, (d, u)| {
            damaged += d;
            unmapped += u;
            std::ops::ControlFlow::Continue(())
        })
        .unwrap();
    assert_eq!((damaged, unmapped), (0, 0));
}

struct Counting;

impl ntfs_ng::scan::Projection for Counting {
    type Output = (usize, usize);
    type Scratch = ();

    fn scratch(&self) {}

    fn begin(&self, (): &mut (), _: ntfs_ng::scan::Chunk) -> (usize, usize) {
        (0, 0)
    }

    fn entry(&self, out: &mut (usize, usize), (): &mut (), _: u64, entry: Entry<'_>) {
        match entry {
            Entry::Damaged { .. } => out.0 += 1,
            Entry::Unmapped => out.1 += 1,
            Entry::Record(record) => {
                let mut raw = Vec::new();
                record.raw_into(&mut raw);
                let mut again = raw.clone();
                let fixed = Record::fixup(&mut again).unwrap();
                assert_eq!(fixed.data(), record.data());
            }
            _ => {}
        }
    }
}

#[test]
fn records_in_every_extent_of_a_fragmented_mft() {
    for (fragments, in_extensions) in [(3, false), (3, true)] {
        let mut builder = ImageBuilder::new(ImageConfig {
            mft_records: 384,
            mft_fragments: fragments,
            mft_in_extensions: in_extensions,
            ..ImageConfig::default()
        });
        let mut names = Vec::new();
        for at in 0..340 {
            let name = format!("file-{at:04}.txt");
            builder.add(FileSpec::file(
                &name,
                ROOT,
                Content::Resident(name.clone().into_bytes()),
            ));
            names.push(name);
        }
        let image = builder.build();
        assert_eq!(image.mft_extents.len(), 3);
        let volume = Volume::open(image.bytes.as_slice()).unwrap();
        assert!(
            volume.mft().problems().is_empty(),
            "{:?}",
            volume.mft().problems()
        );
        let last_extent_start = (image.mft_extents[0].1 + image.mft_extents[1].1) * 4096 / 1024;
        let mut in_last = 0;
        for (name, reference) in names.iter().zip(&image.records) {
            let (bytes, _) = read(&volume, reference.record, "", None);
            assert_eq!(bytes, name.as_bytes(), "record {}", reference.record);
            in_last += usize::from(reference.record >= last_extent_start);
        }
        assert!(in_last > 10, "{in_last} records in the last extent");
        // The scan maps them too.
        let scan = Scan::new(volume.mft()).threads(4).chunk_bytes(32 << 10);
        let files = Files::scan(&scan, volume.mft().records(), FileOptions::LIVE).unwrap();
        let extra: Vec<_> = files
            .table
            .files()
            .filter(|(id, _)| {
                files
                    .table
                    .links_of(*id)
                    .all(|link| !link.name.starts_with("file-"))
            })
            .map(|(id, file)| {
                (
                    file.record,
                    files
                        .table
                        .links_of(id)
                        .map(|link| link.name.to_owned())
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(files.table.len(), names.len(), "{extra:?}");
    }
}

/// Panics at chunk `at` (`Panicking::0`).
struct Panicking(usize);

impl ntfs_ng::scan::Projection for Panicking {
    type Output = ();
    type Scratch = ();

    fn scratch(&self) {}

    fn begin(&self, (): &mut (), chunk: ntfs_ng::scan::Chunk) {
        assert_ne!(chunk.index, self.0, "the projection failed");
    }

    fn entry(&self, (): &mut (), (): &mut (), _: u64, _: Entry<'_>) {}
}

/// A panic in a projection or in the sink ends the scan with that panic, in either order of
/// delivery, whether workers wait for the sink or not: the other workers never wait for a chunk,
/// or a release, that will not come.
#[test]
fn panics_end_the_scan() {
    let (image, _) = image(1, false);
    let bytes = image.bytes;
    for (ordered, wait) in [(true, false), (false, false), (true, true), (false, true)] {
        for in_sink in [false, true] {
            let bytes = bytes.clone();
            let (done, finished) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let outcome = std::panic::catch_unwind(|| {
                    let volume = Volume::open(bytes.as_slice()).unwrap();
                    let scan = Scan::new(volume.mft())
                        .threads(4)
                        .window(2)
                        .chunk_bytes(1 << 10)
                        .ordered(ordered)
                        .wait_for_sink(wait);
                    let projection = Panicking(if in_sink { usize::MAX } else { 3 });
                    let _ = scan.run(&projection, |chunk, ()| {
                        assert!(!in_sink || chunk.index != 3, "the sink failed");
                        std::ops::ControlFlow::Continue(())
                    });
                });
                let _ = done.send(outcome.is_err());
            });
            let panicked = finished
                .recv_timeout(std::time::Duration::from_secs(60))
                .expect("the scan hung");
            assert!(
                panicked,
                "ordered {ordered}, waiting {wait}, in the sink {in_sink}"
            );
        }
    }
}
