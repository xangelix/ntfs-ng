//! Micro-benchmarks of the structure parsers: what one record, attribute, value or name costs.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use ntfs_ng::{
    BootSector, FileReference, MappingPairs, Name, Record, lznt1,
    testing::{
        Rng, lznt1_compress,
        record::{NonResidentSpec, RecordBuilder, mapping_pairs, utf16},
    },
    types, usa,
    value::{AttributeList, FileName, Namespace, StandardInformation, Times},
};

fn times() -> Times {
    Times {
        created: 0x01d9_0000_0000_0000,
        modified: 0x01d9_0000_0001_0000,
        changed: 0x01d9_0000_0002_0000,
        accessed: 0x01d9_0000_0003_0000,
    }
}

/// A typical file record: `$SI`, a long name and its 8.3 alias, a non-resident `$DATA`.
fn typical(size: usize) -> Vec<u8> {
    let parent = FileReference::new(1234, 3);
    RecordBuilder::new(77)
        .size(size)
        .standard_information(times(), 0x20)
        .file_name(
            parent,
            Namespace::Win32,
            "Quarterly Report (final) v2.xlsx",
            times(),
            52_311,
        )
        .file_name(parent, Namespace::Dos, "QUARTE~1.XLS", times(), 52_311)
        .non_resident(
            types::DATA,
            "",
            &NonResidentSpec {
                runs: vec![(13, Some(912_345)), (2, Some(1_000_001))],
                allocated: 15 * 4096,
                data: 52_311,
                initialized: 52_311,
                ..NonResidentSpec::default()
            },
        )
        .build()
}

fn records(c: &mut Criterion) {
    let mut group = c.benchmark_group("record");
    for size in [1024usize, 4096] {
        let raw = typical(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_function(format!("fixup/{size}"), |b| {
            b.iter_batched_ref(
                || raw.clone(),
                |buf| {
                    black_box(Record::fixup(black_box(buf)).is_ok());
                },
                BatchSize::SmallInput,
            );
        });
        group.bench_function(format!("fixup+walk/{size}"), |b| {
            b.iter_batched_ref(
                || raw.clone(),
                |buf| {
                    let record = Record::fixup(buf).unwrap();
                    let mut count = 0;
                    for attribute in record.attributes() {
                        count += black_box(attribute.unwrap().type_code()) as usize;
                    }
                    black_box(count);
                },
                BatchSize::SmallInput,
            );
        });
        group.bench_function(format!("fixup+decode-all/{size}"), |b| {
            b.iter_batched_ref(
                || raw.clone(),
                |buf| {
                    let record = Record::fixup(buf).unwrap();
                    for attribute in record.attributes().map_while(Result::ok) {
                        match attribute.type_code() {
                            types::STANDARD_INFORMATION => {
                                black_box(
                                    StandardInformation::parse(attribute.value().unwrap()).unwrap(),
                                );
                            }
                            types::FILE_NAME => {
                                let name = FileName::parse(attribute.value().unwrap()).unwrap();
                                black_box(name.name.to_string_lossy());
                            }
                            types::DATA => {
                                let extent = attribute.non_resident().unwrap();
                                for run in extent.runs() {
                                    black_box(run.unwrap());
                                }
                            }
                            _ => {}
                        }
                    }
                },
                BatchSize::SmallInput,
            );
        });
        let mut fixed = raw.clone();
        Record::fixup(&mut fixed).unwrap();
        group.bench_function(format!("raw_into/{size}"), |b| {
            let record = Record::from_fixed(&fixed).unwrap();
            let mut out = Vec::with_capacity(size);
            b.iter(|| {
                record.raw_into(&mut out);
                black_box(&out);
            });
        });
    }
    group.finish();
}

fn names(c: &mut Criterion) {
    let mut group = c.benchmark_group("name");
    for (label, text) in [
        ("ascii-12", "Readme.txt.x"),
        (
            "ascii-64",
            "a-rather-long-file-name-of-sixty-four-characters-exactly-yes.dat",
        ),
        ("ascii-255", &"x".repeat(255)[..]),
        ("bmp-12", "日本語のファイル名です"),
        ("mixed-40", "données_Übersicht_ñandú_русский_Ελληνικά"),
        ("surrogates-16", "😀😁😂🤣😃😄😅😆"),
    ] {
        let bytes = utf16(text);
        let name = Name::new(&bytes);
        group.throughput(Throughput::Elements(1));
        group.bench_function(format!("is_ascii/{label}"), |b| {
            b.iter(|| black_box(black_box(name).is_ascii()))
        });
        group.bench_function(format!("push_lossy/{label}"), |b| {
            let mut out = String::with_capacity(512);
            b.iter(|| {
                out.clear();
                black_box(name).push_lossy(&mut out);
                black_box(&out);
            });
        });
        group.bench_function(format!("push_wtf8/{label}"), |b| {
            let mut out = Vec::with_capacity(1024);
            b.iter(|| {
                out.clear();
                black_box(name).push_wtf8(&mut out);
                black_box(&out);
            });
        });
        group.bench_function(format!("eq_ignore_ascii_case/{label}"), |b| {
            b.iter(|| black_box(black_box(name).eq_ignore_ascii_case(text)));
        });
    }
    group.finish();
}

fn values(c: &mut Criterion) {
    let mut group = c.benchmark_group("value");
    let raw = typical(1024);
    let mut fixed = raw.clone();
    let record = Record::fixup(&mut fixed).unwrap();
    let name_value = record
        .attributes()
        .map_while(Result::ok)
        .find(|attribute| attribute.type_code() == types::FILE_NAME)
        .unwrap()
        .value()
        .unwrap()
        .to_vec();
    group.bench_function("file_name/parse", |b| {
        b.iter(|| black_box(FileName::parse(black_box(&name_value)).unwrap()))
    });
    group.bench_function("file_name/link", |b| {
        b.iter(|| black_box(FileName::link(black_box(&name_value)).unwrap()))
    });
    let si = ntfs_ng::testing::record::standard_information(times(), 0x20);
    group.bench_function("standard_information/parse", |b| {
        b.iter(|| black_box(StandardInformation::parse(black_box(&si)).unwrap()));
    });
    group.bench_function("standard_information/times_of", |b| {
        b.iter(|| black_box(StandardInformation::times_of(black_box(&si))));
    });
    let mut list = Vec::new();
    for at in 0..40u64 {
        list.extend(ntfs_ng::testing::record::attribute_list_entry(
            types::DATA,
            &[],
            at * 16,
            FileReference::new(9000 + at, 1),
            u16::try_from(at).unwrap(),
        ));
    }
    group.throughput(Throughput::Elements(40));
    group.bench_function("attribute_list/40-entries", |b| {
        b.iter(|| {
            black_box(
                AttributeList::new(black_box(&list))
                    .filter_map(Result::ok)
                    .count(),
            )
        });
    });
    group.finish();
}

fn runs(c: &mut Criterion) {
    let mut group = c.benchmark_group("mapping_pairs");
    let mut rng = Rng::new(7);
    for count in [1usize, 16, 256] {
        let runs: Vec<(u64, Option<u64>)> = (0..count)
            .map(|_| {
                let length = 1 + rng.below(5000);
                if rng.chance(100_000) {
                    (length, None)
                } else {
                    (length, Some(rng.below(1 << 36)))
                }
            })
            .collect();
        let pairs = mapping_pairs(&runs);
        group.throughput(Throughput::Elements(count as u64));
        group.bench_function(format!("decode/{count}"), |b| {
            b.iter(|| {
                let mut total = 0u64;
                for run in MappingPairs::new(black_box(&pairs), 0, 0) {
                    total += run.unwrap().length;
                }
                black_box(total);
            });
        });
    }
    group.finish();
}

fn compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("lznt1");
    let mut rng = Rng::new(11);
    let text: Vec<u8> = (0..65_536)
        .map(|i| b"lorem ipsum dolor sit amet, "[i % 28])
        .collect();
    let mixed: Vec<u8> = (0..65_536)
        .map(|i| {
            if rng.chance(200_000) {
                rng.next_u64() as u8
            } else {
                (i / 9 % 41) as u8
            }
        })
        .collect();
    let noise: Vec<u8> = (0..65_536).map(|_| rng.next_u64() as u8).collect();
    for (label, input) in [("text", &text), ("mixed", &mixed), ("noise", &noise)] {
        let packed = lznt1_compress(input);
        group.throughput(Throughput::Bytes(input.len() as u64));
        group.bench_function(format!("decompress-unit/{label}"), |b| {
            let mut out = vec![0u8; 65_536];
            b.iter(|| black_box(lznt1::decompress(black_box(&packed), &mut out).unwrap()));
        });
    }
    group.finish();
}

fn pages(c: &mut Criterion) {
    let mut group = c.benchmark_group("pages");
    let mut page = vec![0u8; 4096];
    page[0..4].copy_from_slice(b"RCRD");
    page[4..6].copy_from_slice(&40u16.to_le_bytes());
    page[6..8].copy_from_slice(&9u16.to_le_bytes());
    ntfs_ng::testing::record::protect(&mut page, 40, 0x0707);
    group.throughput(Throughput::Bytes(4096));
    group.bench_function("usa/unprotect-4k", |b| {
        b.iter_batched_ref(
            || page.clone(),
            |buf| usa::unprotect(buf, 32).unwrap(),
            BatchSize::SmallInput,
        );
    });
    let mut boot = vec![0u8; 512];
    boot[3..11].copy_from_slice(b"NTFS    ");
    boot[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
    boot[0x0d] = 8;
    boot[0x28..0x30].copy_from_slice(&(1u64 << 30).to_le_bytes());
    boot[0x30..0x38].copy_from_slice(&4u64.to_le_bytes());
    boot[0x40] = 0xf6;
    boot[0x44] = 1;
    boot[0x1fe] = 0x55;
    boot[0x1ff] = 0xaa;
    group.bench_function("boot_sector/parse", |b| {
        b.iter(|| black_box(BootSector::parse(black_box(&boot)).unwrap()))
    });
    group.finish();
}

criterion_group!(benches, records, names, values, runs, compression, pages);
criterion_main!(benches);
