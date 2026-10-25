//! Micro-benchmarks of the structure parsers: what records, attributes, values and names cost.
//!
//! Every benchmark parses a batch of different inputs per iteration (`common`), and names and
//! mapping pairs a different batch each time: its throughput is per record, name, value, run or
//! byte, so that the time per item stays comparable however large the batch.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

mod common;

use std::hint::black_box;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use ntfs_ng::{
    BootSector, MappingPairs, Name, Record, lznt1, types, usa,
    value::{AttributeList, FileName, StandardInformation},
};

/// Every attribute of `buf`'s record decoded as its type reads: what a full decode costs.
fn decode_all(buf: &mut [u8]) {
    let record = Record::fixup(buf).unwrap();
    for attribute in record.attributes().map_while(Result::ok) {
        match attribute.type_code() {
            types::STANDARD_INFORMATION => {
                black_box(StandardInformation::parse(attribute.value().unwrap()).unwrap());
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
}

fn records(c: &mut Criterion) {
    let mut group = c.benchmark_group("record");
    for size in [1024usize, 4096] {
        let stored = common::records(size);
        group.throughput(Throughput::Bytes((size * stored.len()) as u64));
        group.bench_function(format!("fixup/{size}"), |b| {
            b.iter_batched_ref(
                || stored.clone(),
                |batch| {
                    for buf in batch {
                        black_box(Record::fixup(black_box(buf)).is_ok());
                    }
                },
                BatchSize::SmallInput,
            );
        });
        group.bench_function(format!("fixup+walk/{size}"), |b| {
            b.iter_batched_ref(
                || stored.clone(),
                |batch| {
                    let mut count = 0;
                    for buf in batch {
                        let record = Record::fixup(buf).unwrap();
                        for attribute in record.attributes() {
                            count += black_box(attribute.unwrap().type_code()) as usize;
                        }
                    }
                    black_box(count);
                },
                BatchSize::SmallInput,
            );
        });
        group.bench_function(format!("fixup+decode-all/{size}"), |b| {
            b.iter_batched_ref(
                || stored.clone(),
                |batch| batch.iter_mut().for_each(|buf| decode_all(buf)),
                BatchSize::SmallInput,
            );
        });
        let fixed = common::fixed(size);
        group.bench_function(format!("raw_into/{size}"), |b| {
            let records: Vec<_> = fixed
                .iter()
                .map(|data| Record::from_fixed(data).unwrap())
                .collect();
            let mut out = Vec::with_capacity(size);
            b.iter(|| {
                for record in &records {
                    record.raw_into(&mut out);
                    black_box(&out);
                }
            });
        });
    }
    group.finish();
}

fn names(c: &mut Criterion) {
    let mut group = c.benchmark_group("name");
    for (label, _, _) in common::NAME_CLASSES {
        let pool = common::names(label);
        let names: Vec<(Name<'_>, &str)> = pool
            .iter()
            .map(|(units, text)| (Name::new(units), text.as_str()))
            .collect();
        group.throughput(Throughput::Elements(common::NAMES as u64));
        group.bench_function(format!("is_ascii/{label}"), |b| {
            let mut batches = common::Rotation::new(&names, common::NAMES);
            b.iter(|| {
                for (name, _) in batches.next_batch() {
                    black_box(black_box(*name).is_ascii());
                }
            });
        });
        group.bench_function(format!("push_lossy/{label}"), |b| {
            let mut batches = common::Rotation::new(&names, common::NAMES);
            let mut out = String::with_capacity(1024);
            b.iter(|| {
                for (name, _) in batches.next_batch() {
                    out.clear();
                    black_box(*name).push_lossy(&mut out);
                    black_box(&out);
                }
            });
        });
        group.bench_function(format!("push_wtf8/{label}"), |b| {
            let mut batches = common::Rotation::new(&names, common::NAMES);
            let mut out = Vec::with_capacity(1024);
            b.iter(|| {
                for (name, _) in batches.next_batch() {
                    out.clear();
                    black_box(*name).push_wtf8(&mut out);
                    black_box(&out);
                }
            });
        });
        group.bench_function(format!("eq_ignore_ascii_case/{label}"), |b| {
            let mut batches = common::Rotation::new(&names, common::NAMES);
            b.iter(|| {
                for (name, text) in batches.next_batch() {
                    black_box(black_box(*name).eq_ignore_ascii_case(text));
                }
            });
        });
    }
    group.finish();
}

fn values(c: &mut Criterion) {
    let mut group = c.benchmark_group("value");
    let file_names = common::file_names();
    group.throughput(Throughput::Elements(file_names.len() as u64));
    group.bench_function("file_name/parse", |b| {
        b.iter(|| {
            for value in &file_names {
                black_box(FileName::parse(black_box(value)).unwrap());
            }
        });
    });
    group.bench_function("file_name/link", |b| {
        b.iter(|| {
            for value in &file_names {
                black_box(FileName::link(black_box(value)).unwrap());
            }
        });
    });
    let infos = common::standard_information_values();
    group.throughput(Throughput::Elements(infos.len() as u64));
    group.bench_function("standard_information/parse", |b| {
        b.iter(|| {
            for value in &infos {
                black_box(StandardInformation::parse(black_box(value)).unwrap());
            }
        });
    });
    group.bench_function("standard_information/times_of", |b| {
        b.iter(|| {
            for value in &infos {
                black_box(StandardInformation::times_of(black_box(value)));
            }
        });
    });
    let lists = common::attribute_lists();
    group.throughput(Throughput::Elements(40 * lists.len() as u64));
    group.bench_function("attribute_list/40-entries", |b| {
        b.iter(|| {
            for list in &lists {
                black_box(
                    AttributeList::new(black_box(list))
                        .filter_map(Result::ok)
                        .count(),
                );
            }
        });
    });
    group.finish();
}

fn runs(c: &mut Criterion) {
    let mut group = c.benchmark_group("mapping_pairs");
    for count in [1usize, 16, 256] {
        let arrays = common::pair_arrays(count);
        group.throughput(Throughput::Elements(common::PAIRS as u64));
        group.bench_function(format!("decode/{count}"), |b| {
            let mut batches = common::Rotation::new(&arrays, common::PAIRS / count);
            b.iter(|| {
                let mut total = 0u64;
                for pairs in batches.next_batch() {
                    for run in MappingPairs::new(black_box(pairs), 0, 0) {
                        total += run.unwrap().length;
                    }
                }
                black_box(total);
            });
        });
    }
    group.finish();
}

fn compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("lznt1");
    for (label, packed) in common::compressed() {
        group.throughput(Throughput::Bytes(65_536));
        group.bench_function(format!("decompress-unit/{label}"), |b| {
            let mut out = vec![0u8; 65_536];
            b.iter(|| black_box(lznt1::decompress(black_box(&packed), &mut out).unwrap()));
        });
    }
    group.finish();
}

fn pages(c: &mut Criterion) {
    let mut group = c.benchmark_group("pages");
    let pages = common::pages();
    group.throughput(Throughput::Bytes((4096 * pages.len()) as u64));
    group.bench_function("usa/unprotect-4k", |b| {
        b.iter_batched_ref(
            || pages.clone(),
            |batch| {
                for page in batch {
                    usa::unprotect(page, 32).unwrap();
                }
            },
            BatchSize::SmallInput,
        );
    });
    let sectors = common::boot_sectors();
    group.throughput(Throughput::Elements(sectors.len() as u64));
    group.bench_function("boot_sector/parse", |b| {
        b.iter(|| {
            for sector in &sectors {
                black_box(BootSector::parse(black_box(sector)).unwrap());
            }
        });
    });
    group.finish();
}

criterion_group!(benches, records, names, values, runs, compression, pages);
criterion_main!(benches);
