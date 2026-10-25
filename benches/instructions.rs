//! Instruction counts of the parser benchmarks (`parsers.rs`): the same batches (`common`), run
//! once each under Valgrind's Callgrind by [gungraun](https://docs.rs/gungraun). Counts are exact
//! and repeatable whatever the machine's load or where the code happens to be aligned, so that
//! parsers a change did not touch count the same before and after it.
//!
//! Each benchmark returns its inputs, and gets its output buffers from its setup: only the
//! parsing is counted, never an allocation or a free.
//!
//! Linux only, and not part of a plain `cargo bench`: it needs Valgrind and a `gungraun-runner`
//! of the same version as the `gungraun` dev-dependency.
//!
//! ```text
//! cargo install gungraun-runner --version <the dev-dependency's>
//! cargo bench --all-features --bench instructions
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    missing_docs
)]

#[cfg(target_os = "linux")]
mod common;

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod counted {
    use std::hint::black_box;

    use gungraun::prelude::*;
    use ntfs_ng::{
        BootSector, MappingPairs, Name, Record, lznt1, types, usa,
        value::{AttributeList, FileName, StandardInformation},
    };

    use super::common;

    type Names = Vec<(Vec<u8>, String)>;

    fn records_and_text(size: usize) -> (Vec<Vec<u8>>, String) {
        (common::records(size), String::with_capacity(1024))
    }

    fn fixed_and_bytes(size: usize) -> (Vec<Vec<u8>>, Vec<u8>) {
        (common::fixed(size), Vec::with_capacity(size))
    }

    /// The first batch of a pool of names: counts are exact, and need no other.
    fn names(label: &str) -> Names {
        let mut pool = common::names(label);
        pool.truncate(common::NAMES);
        pool
    }

    fn names_and_text(label: &str) -> (Names, String) {
        (names(label), String::with_capacity(1024))
    }

    fn names_and_bytes(label: &str) -> (Names, Vec<u8>) {
        (names(label), Vec::with_capacity(1024))
    }

    /// The first batch of a pool of mapping pairs arrays of `count` runs.
    fn pair_arrays(count: usize) -> Vec<Vec<u8>> {
        let mut pool = common::pair_arrays(count);
        pool.truncate(common::PAIRS / count);
        pool
    }

    #[library_benchmark]
    #[bench::size_1024(args = (1024), setup = common::records)]
    #[bench::size_4096(args = (4096), setup = common::records)]
    fn fixup(mut batch: Vec<Vec<u8>>) -> (usize, Vec<Vec<u8>>) {
        let fixed = batch
            .iter_mut()
            .map(|buf| usize::from(Record::fixup(black_box(buf)).is_ok()))
            .sum();
        (fixed, batch)
    }

    #[library_benchmark]
    #[bench::size_1024(args = (1024), setup = common::records)]
    #[bench::size_4096(args = (4096), setup = common::records)]
    fn fixup_walk(mut batch: Vec<Vec<u8>>) -> (usize, Vec<Vec<u8>>) {
        let mut count = 0;
        for buf in &mut batch {
            let record = Record::fixup(buf).unwrap();
            for attribute in record.attributes() {
                count += black_box(attribute.unwrap().type_code()) as usize;
            }
        }
        (count, batch)
    }

    #[library_benchmark]
    #[bench::size_1024(args = (1024), setup = records_and_text)]
    #[bench::size_4096(args = (4096), setup = records_and_text)]
    fn fixup_decode_all((mut batch, mut name): (Vec<Vec<u8>>, String)) -> (Vec<Vec<u8>>, String) {
        for buf in &mut batch {
            let record = Record::fixup(buf).unwrap();
            for attribute in record.attributes().map_while(Result::ok) {
                match attribute.type_code() {
                    types::STANDARD_INFORMATION => {
                        black_box(StandardInformation::parse(attribute.value().unwrap()).unwrap());
                    }
                    types::FILE_NAME => {
                        name.clear();
                        FileName::parse(attribute.value().unwrap())
                            .unwrap()
                            .name
                            .push_lossy(&mut name);
                        black_box(&name);
                    }
                    types::DATA => {
                        for run in attribute.non_resident().unwrap().runs() {
                            black_box(run.unwrap());
                        }
                    }
                    _ => {}
                }
            }
        }
        (batch, name)
    }

    #[library_benchmark]
    #[bench::size_1024(args = (1024), setup = fixed_and_bytes)]
    #[bench::size_4096(args = (4096), setup = fixed_and_bytes)]
    fn raw_into((batch, mut out): (Vec<Vec<u8>>, Vec<u8>)) -> (Vec<Vec<u8>>, Vec<u8>) {
        for data in &batch {
            Record::from_fixed(data).unwrap().raw_into(&mut out);
            black_box(&out);
        }
        (batch, out)
    }

    library_benchmark_group!(
        name = records,
        benchmarks = [fixup, fixup_walk, fixup_decode_all, raw_into]
    );

    #[library_benchmark]
    #[bench::ascii_12(args = ("ascii-12"), setup = names)]
    #[bench::ascii_64(args = ("ascii-64"), setup = names)]
    #[bench::ascii_255(args = ("ascii-255"), setup = names)]
    #[bench::bmp_12(args = ("bmp-12"), setup = names)]
    #[bench::mixed_40(args = ("mixed-40"), setup = names)]
    #[bench::surrogates_16(args = ("surrogates-16"), setup = names)]
    fn is_ascii(batch: Names) -> (usize, Names) {
        let count = batch
            .iter()
            .filter(|(units, _)| black_box(Name::new(units)).is_ascii())
            .count();
        (count, batch)
    }

    #[library_benchmark]
    #[bench::ascii_12(args = ("ascii-12"), setup = names_and_text)]
    #[bench::ascii_64(args = ("ascii-64"), setup = names_and_text)]
    #[bench::ascii_255(args = ("ascii-255"), setup = names_and_text)]
    #[bench::bmp_12(args = ("bmp-12"), setup = names_and_text)]
    #[bench::mixed_40(args = ("mixed-40"), setup = names_and_text)]
    #[bench::surrogates_16(args = ("surrogates-16"), setup = names_and_text)]
    fn push_lossy((batch, mut out): (Names, String)) -> (Names, String) {
        for (units, _) in &batch {
            out.clear();
            black_box(Name::new(units)).push_lossy(&mut out);
            black_box(&out);
        }
        (batch, out)
    }

    #[library_benchmark]
    #[bench::ascii_12(args = ("ascii-12"), setup = names_and_bytes)]
    #[bench::ascii_64(args = ("ascii-64"), setup = names_and_bytes)]
    #[bench::ascii_255(args = ("ascii-255"), setup = names_and_bytes)]
    #[bench::bmp_12(args = ("bmp-12"), setup = names_and_bytes)]
    #[bench::mixed_40(args = ("mixed-40"), setup = names_and_bytes)]
    #[bench::surrogates_16(args = ("surrogates-16"), setup = names_and_bytes)]
    fn push_wtf8((batch, mut out): (Names, Vec<u8>)) -> (Names, Vec<u8>) {
        for (units, _) in &batch {
            out.clear();
            black_box(Name::new(units)).push_wtf8(&mut out);
            black_box(&out);
        }
        (batch, out)
    }

    #[library_benchmark]
    #[bench::ascii_12(args = ("ascii-12"), setup = names)]
    #[bench::ascii_64(args = ("ascii-64"), setup = names)]
    #[bench::ascii_255(args = ("ascii-255"), setup = names)]
    #[bench::bmp_12(args = ("bmp-12"), setup = names)]
    #[bench::mixed_40(args = ("mixed-40"), setup = names)]
    #[bench::surrogates_16(args = ("surrogates-16"), setup = names)]
    fn eq_ignore_ascii_case(batch: Names) -> (usize, Names) {
        let equal = batch
            .iter()
            .filter(|(units, text)| black_box(Name::new(units)).eq_ignore_ascii_case(text))
            .count();
        (equal, batch)
    }

    library_benchmark_group!(
        name = names,
        benchmarks = [is_ascii, push_lossy, push_wtf8, eq_ignore_ascii_case]
    );

    #[library_benchmark]
    #[bench::batch(setup = common::file_names)]
    fn file_name_parse(batch: Vec<Vec<u8>>) -> (usize, Vec<Vec<u8>>) {
        let units = batch
            .iter()
            .map(|value| {
                black_box(FileName::parse(black_box(value)).unwrap())
                    .name
                    .len()
            })
            .sum();
        (units, batch)
    }

    #[library_benchmark]
    #[bench::batch(setup = common::file_names)]
    fn file_name_link(batch: Vec<Vec<u8>>) -> (usize, Vec<Vec<u8>>) {
        let units = batch
            .iter()
            .map(|value| black_box(FileName::link(black_box(value)).unwrap()).2.len())
            .sum();
        (units, batch)
    }

    #[library_benchmark]
    #[bench::batch(setup = common::standard_information_values)]
    fn standard_information_parse(batch: Vec<Vec<u8>>) -> (u32, Vec<Vec<u8>>) {
        let attributes = batch
            .iter()
            .map(|value| {
                black_box(StandardInformation::parse(black_box(value)).unwrap()).attributes
            })
            .fold(0, u32::wrapping_add);
        (attributes, batch)
    }

    #[library_benchmark]
    #[bench::batch(setup = common::standard_information_values)]
    fn standard_information_times_of(batch: Vec<Vec<u8>>) -> (usize, Vec<Vec<u8>>) {
        let read = batch
            .iter()
            .filter(|value| black_box(StandardInformation::times_of(black_box(value))).is_some())
            .count();
        (read, batch)
    }

    #[library_benchmark]
    #[bench::entries_40(setup = common::attribute_lists)]
    fn attribute_list(batch: Vec<Vec<u8>>) -> (usize, Vec<Vec<u8>>) {
        let entries = batch
            .iter()
            .map(|list| {
                AttributeList::new(black_box(list))
                    .filter_map(Result::ok)
                    .count()
            })
            .sum();
        (entries, batch)
    }

    library_benchmark_group!(
        name = values,
        benchmarks = [
            file_name_parse,
            file_name_link,
            standard_information_parse,
            standard_information_times_of,
            attribute_list
        ]
    );

    #[library_benchmark]
    #[bench::runs_1(args = (1), setup = pair_arrays)]
    #[bench::runs_16(args = (16), setup = pair_arrays)]
    #[bench::runs_256(args = (256), setup = pair_arrays)]
    fn decode(batch: Vec<Vec<u8>>) -> (u64, Vec<Vec<u8>>) {
        let mut total = 0;
        for pairs in &batch {
            for run in MappingPairs::new(black_box(pairs), 0, 0) {
                total += run.unwrap().length;
            }
        }
        (total, batch)
    }

    library_benchmark_group!(name = runs, benchmarks = [decode]);

    /// The compression unit `index` of [`common::compressed`], and room for it decompressed.
    fn unit(index: usize) -> (Vec<u8>, Vec<u8>) {
        let packed = Vec::from(common::compressed()).swap_remove(index).1;
        (packed, vec![0; 65_536])
    }

    #[library_benchmark]
    #[bench::text(args = (0), setup = unit)]
    #[bench::mixed(args = (1), setup = unit)]
    #[bench::noise(args = (2), setup = unit)]
    fn decompress_unit((packed, mut out): (Vec<u8>, Vec<u8>)) -> (usize, Vec<u8>, Vec<u8>) {
        let len = lznt1::decompress(black_box(&packed), &mut out).unwrap();
        (len, packed, out)
    }

    library_benchmark_group!(name = compression, benchmarks = [decompress_unit]);

    #[library_benchmark]
    #[bench::pages_4k(setup = common::pages)]
    fn usa_unprotect(mut batch: Vec<Vec<u8>>) -> (usize, Vec<Vec<u8>>) {
        let unprotected = batch
            .iter_mut()
            .map(|page| usize::from(usa::unprotect(black_box(page), 32).is_ok()))
            .sum();
        (unprotected, batch)
    }

    #[library_benchmark]
    #[bench::batch(setup = common::boot_sectors)]
    fn boot_sector_parse(batch: Vec<Vec<u8>>) -> (u64, Vec<Vec<u8>>) {
        let sectors = batch
            .iter()
            .map(|sector| black_box(BootSector::parse(black_box(sector)).unwrap()).total_sectors)
            .sum();
        (sectors, batch)
    }

    library_benchmark_group!(
        name = pages,
        benchmarks = [usa_unprotect, boot_sector_parse]
    );
}

#[cfg(target_os = "linux")]
use counted::{compression, names, pages, records, runs, values};

#[cfg(target_os = "linux")]
gungraun::main!(
    library_benchmark_groups = records,
    names,
    values,
    runs,
    compression,
    pages
);
