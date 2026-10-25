//! Engine benchmarks over a synthetic `$MFT` held in memory: the scan by projection, threads,
//! chunk size and delivery order; the directory index; the file table; parents and paths; and
//! streams of a volume image.
//!
//! `NTFS_NG_BENCH_RECORDS` sets the corpus size (default 200,000 records, about 200 MB).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use std::{hint::black_box, ops::ControlFlow, sync::OnceLock, time::Duration};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ntfs_ng::{
    files::{FileOptions, Files},
    index::{DirectoryIndex, DirectoryProjection, IndexBuilder, PathPolicy},
    mft::Mft,
    scan::{Chunk, Entry, Projection, Scan},
    testing::{
        corpus::{CorpusConfig, mft},
        image::{Content, FileSpec, ImageBuilder, ImageConfig, ROOT},
    },
    types,
    usage::Usage,
    volume::{Volume, stream::StreamReader},
};

fn records() -> u64 {
    std::env::var("NTFS_NG_BENCH_RECORDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200_000)
}

fn corpus(record_size: usize) -> &'static [u8] {
    static ONE_K: OnceLock<Vec<u8>> = OnceLock::new();
    static FOUR_K: OnceLock<Vec<u8>> = OnceLock::new();
    let cell = if record_size == 4096 { &FOUR_K } else { &ONE_K };
    cell.get_or_init(|| {
        let mut config = CorpusConfig::realistic(if record_size == 4096 {
            records() / 4
        } else {
            records()
        });
        config.record_size = record_size;
        mft(&config).0
    })
}

/// Counts records: the scan engine's own cost.
struct Count;

impl Projection for Count {
    type Output = u64;
    type Scratch = ();

    fn scratch(&self) {}

    fn begin(&self, (): &mut (), _: Chunk) -> u64 {
        0
    }

    fn entry(&self, out: &mut u64, (): &mut (), _: u64, entry: Entry<'_>) {
        if let Entry::Record(record) = entry {
            *out += u64::from(record.in_use());
        }
    }
}

/// Walks every attribute of every record: parsing without keeping anything.
struct Walk;

impl Projection for Walk {
    type Output = u64;
    type Scratch = ();

    fn scratch(&self) {}

    fn begin(&self, (): &mut (), _: Chunk) -> u64 {
        0
    }

    fn entry(&self, out: &mut u64, (): &mut (), _: u64, entry: Entry<'_>) {
        if let Entry::Record(record) = entry {
            for attribute in record.attributes().map_while(Result::ok) {
                *out += u64::from(attribute.type_code());
            }
        }
    }
}

fn sum<P: Projection<Output = u64>>(scan: &Scan<'_, &[u8]>, projection: &P) -> u64 {
    let mut total = 0;
    scan.run(projection, |_, count| {
        total += count;
        ControlFlow::Continue(())
    })
    .unwrap();
    total
}

fn scan(c: &mut Criterion) {
    let data = corpus(1024);
    let mft = Mft::from_file(data).unwrap();
    let mut group = c.benchmark_group("scan");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(8));
    let all = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    // Each count once: the machine's own may be one of the others.
    let mut counts: Vec<usize> = [1, 2, 4, 8, 16, all]
        .into_iter()
        .filter(|&threads| threads <= all)
        .collect();
    counts.sort_unstable();
    counts.dedup();
    for threads in counts {
        group.bench_function(format!("count/threads-{threads}"), |b| {
            b.iter(|| black_box(sum(&Scan::new(&mft).threads(threads), &Count)));
        });
    }
    for chunk in [256usize << 10, 1 << 20, 4 << 20, 16 << 20] {
        group.bench_function(format!("count/chunk-{}k", chunk >> 10), |b| {
            b.iter(|| black_box(sum(&Scan::new(&mft).chunk_bytes(chunk), &Count)));
        });
    }
    group.bench_function("count/unordered", |b| {
        b.iter(|| black_box(sum(&Scan::new(&mft).ordered(false), &Count)))
    });
    group.bench_function("walk/all-threads", |b| {
        b.iter(|| black_box(sum(&Scan::new(&mft), &Walk)))
    });
    group.bench_function("walk/1-thread", |b| {
        b.iter(|| black_box(sum(&Scan::new(&mft).threads(1), &Walk)))
    });
    let data4 = corpus(4096);
    let mft4 = Mft::from_file(data4).unwrap();
    group.throughput(Throughput::Bytes(data4.len() as u64));
    group.bench_function("walk/4k-records", |b| {
        b.iter(|| black_box(sum(&Scan::new(&mft4), &Walk)))
    });
    group.finish();
}

fn index(c: &mut Criterion) {
    let data = corpus(1024);
    let mft = Mft::from_file(data).unwrap();
    let mut group = c.benchmark_group("index");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(8));
    group.throughput(Throughput::Elements(mft.records()));
    for (label, policy) in [
        ("live", PathPolicy::LIVE),
        ("strict", PathPolicy::STRICT),
        ("recovery", PathPolicy::RECOVERY),
    ] {
        group.bench_function(format!("scan+build/{label}"), |b| {
            b.iter(|| {
                black_box(
                    DirectoryIndex::scan(&Scan::new(&mft), mft.records(), policy)
                        .unwrap()
                        .len(),
                )
            });
        });
    }
    // The build alone, from batches collected once.
    let mut batches = Vec::new();
    Scan::new(&mft)
        .run(
            &DirectoryProjection {
                policy: PathPolicy::RECOVERY,
            },
            |_, batch| {
                batches.push(batch);
                ControlFlow::Continue(())
            },
        )
        .unwrap();
    group.bench_function("build-only/recovery", |b| {
        b.iter(|| {
            let mut builder = IndexBuilder::new();
            for batch in &batches {
                builder.push(batch.clone());
            }
            black_box(builder.build(mft.records(), PathPolicy::RECOVERY).len())
        });
    });
    let index = DirectoryIndex::scan(&Scan::new(&mft), mft.records(), PathPolicy::LIVE).unwrap();
    let dirs: Vec<_> = (0..index.len() as u32).collect();
    group.throughput(Throughput::Elements(dirs.len() as u64));
    group.bench_function("write_path/every-directory", |b| {
        let mut out = String::with_capacity(4096);
        b.iter(|| {
            let mut bytes = 0;
            for &dir in &dirs {
                out.clear();
                index.write_path(dir, '\\', &mut out);
                bytes += out.len();
            }
            black_box(bytes)
        });
    });
    group.finish();
}

fn files(c: &mut Criterion) {
    let data = corpus(1024);
    let mft = Mft::from_file(data).unwrap();
    let mut group = c.benchmark_group("files");
    group
        .sample_size(20)
        .measurement_time(Duration::from_secs(8));
    group.throughput(Throughput::Elements(mft.records()));
    let all = FileOptions {
        deleted: true,
        metadata: true,
        dos_aliases: true,
        policy: PathPolicy::RECOVERY,
    };
    for (label, options) in [("live", FileOptions::LIVE), ("everything", all)] {
        group.bench_function(format!("scan/{label}"), |b| {
            b.iter(|| {
                black_box(
                    Files::scan(&Scan::new(&mft), mft.records(), options)
                        .unwrap()
                        .table
                        .len(),
                )
            });
        });
    }
    // The disk-usage table: the same live names with a third of the memory.
    group.bench_function("scan/usage", |b| {
        b.iter(|| {
            black_box(
                Usage::scan(&Scan::new(&mft), mft.records(), PathPolicy::LIVE)
                    .unwrap()
                    .len(),
            )
        });
    });
    let files = Files::scan(&Scan::new(&mft), mft.records(), FileOptions::LIVE).unwrap();
    let threads = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
    group.throughput(Throughput::Elements(files.table.link_count() as u64));
    group.bench_function("parents", |b| {
        b.iter(|| black_box(files.parents(threads).len()))
    });
    group.bench_function("full-paths", |b| {
        let parents = files.parents(threads);
        let mut out = String::with_capacity(4096);
        b.iter(|| {
            let mut bytes = 0;
            for (id, link) in files.table.links() {
                let Some(&dir) = parents.get(id as usize) else {
                    continue;
                };
                if dir == ntfs_ng::index::NONE {
                    continue;
                }
                out.clear();
                files.index.write_path(dir, '\\', &mut out);
                out.push('\\');
                out.push_str(link.name);
                bytes += out.len();
            }
            black_box(bytes)
        });
    });
    group.finish();
}

fn streams(c: &mut Criterion) {
    let mut builder = ImageBuilder::new(ImageConfig {
        clusters: 16_384,
        ..ImageConfig::default()
    });
    let mut rng = ntfs_ng::testing::Rng::new(5);
    let noise: Vec<u8> = (0..8 << 20).map(|_| rng.next_u64() as u8).collect();
    let text: Vec<u8> = (0..8 << 20)
        .map(|i| b"compressible text, compressible text "[i % 37])
        .collect();
    builder.add(FileSpec::file(
        "contiguous",
        ROOT,
        Content::Contiguous(noise.clone()),
    ));
    builder.add(FileSpec::file(
        "fragmented",
        ROOT,
        Content::Fragmented(noise.clone(), 64),
    ));
    builder.add(FileSpec::file(
        "compressed",
        ROOT,
        Content::Compressed(text),
    ));
    let image = builder.build();
    let volume = Volume::open(image.bytes.as_slice()).unwrap();
    let mut group = c.benchmark_group("stream");
    group.sample_size(20);
    for (label, at) in [("contiguous", 0usize), ("fragmented", 1), ("compressed", 2)] {
        let number = image.records[at].record;
        let mut buf = Vec::new();
        let record = volume.record(number, &mut buf).unwrap();
        let (map, sizes) = volume
            .runs(number, &record, None, types::DATA, &[])
            .unwrap()
            .unwrap();
        group.throughput(Throughput::Bytes(sizes.data));
        group.bench_function(format!("read/{label}"), |b| {
            b.iter(|| {
                let mut reader = StreamReader::new(&volume, map.clone(), sizes);
                let mut total = 0u64;
                reader
                    .read(0..u64::MAX, None, &mut |_, piece| -> Result<(), ()> {
                        if let ntfs_ng::volume::stream::Piece::Data(bytes) = piece {
                            total += bytes.len() as u64;
                        }
                        Ok(())
                    })
                    .unwrap();
                black_box(total)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, scan, index, files, streams);
criterion_main!(benches);
