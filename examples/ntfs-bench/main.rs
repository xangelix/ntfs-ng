//! `ntfs-bench`: ntfs-ng's caller-shaped workloads over a `$MFT` dump or a volume image, and a
//! generator of synthetic corpora to run them on.
//!
//! ```text
//! ntfs-bench <du|paths|paths-raw|inventory|dirs> <$MFT|image> [--image] [--threads N]
//!            [--iterations N] [--chunk BYTES]
//! ntfs-bench generate <path> --records N [--record-size BYTES] [--seed N] [--image]
//! ```
//!
//! Each iteration prints one JSON line: the workload, its wall time and its counts.

#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    clippy::expect_used
)]

mod workloads;

use std::{
    io::{BufWriter, Write},
    path::Path,
    time::Instant,
};

use ntfs_ng::{
    BootSector,
    index::PathPolicy,
    mft::Mft,
    source::{FileSource, ReadAt},
    testing::corpus::{CorpusConfig, write_mft},
};

fn arg<T: std::str::FromStr>(args: &[String], flag: &str) -> Option<T> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|at| args.get(at + 1))
        .and_then(|value| value.parse().ok())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(workload), Some(path)) = (args.first(), args.get(1)) else {
        eprintln!(
            "usage: ntfs-bench <du|paths|paths-raw|inventory|dirs|generate> <path> [options]"
        );
        std::process::exit(2);
    };
    if workload == "generate" {
        generate(Path::new(path), &args);
        return;
    }
    let threads: usize = arg(&args, "--threads").unwrap_or(0);
    let iterations: usize = arg(&args, "--iterations").unwrap_or(1);
    let chunk: usize = arg(&args, "--chunk").unwrap_or(ntfs_ng::scan::DEFAULT_CHUNK_BYTES);
    let source = FileSource::open(path).expect("open the input");
    let mft = if args.iter().any(|a| a == "--image") {
        let mut sector = [0u8; 512];
        source
            .read_exact_at(0, &mut sector)
            .expect("read the boot sector");
        let boot = BootSector::parse(&sector).expect("an NTFS boot sector");
        Mft::from_volume(source, &boot).expect("map $MFT")
    } else {
        Mft::from_file(source).expect("a $MFT dump")
    };
    if workload == "inventory-dump" {
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for line in workloads::inventory_dump(&mft, threads, chunk) {
            writeln!(out, "{line}").expect("write");
        }
        return;
    }
    if workload == "paths-dump" {
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for line in workloads::paths_dump(&mft, threads, chunk) {
            writeln!(out, "{line}").expect("write");
        }
        return;
    }
    if workload == "du-dump" {
        let mut out = std::io::BufWriter::new(std::io::stdout().lock());
        for line in workloads::du_dump(&mft, threads, chunk) {
            writeln!(out, "{line}").expect("write");
        }
        return;
    }
    for iteration in 0..iterations {
        let started = Instant::now();
        let totals = match workload.as_str() {
            "du" => workloads::du(&mft, threads, chunk),
            "paths" => workloads::paths(&mft, threads, chunk, false),
            "paths-raw" => workloads::paths(&mft, threads, chunk, true),
            "inventory" => workloads::inventory(&mft, threads, chunk),
            "dirs" => workloads::directories(&mft, threads, chunk, PathPolicy::RECOVERY),
            other => {
                eprintln!("unknown workload {other}");
                std::process::exit(2);
            }
        };
        let wall = started.elapsed().as_secs_f64();
        println!(
            "{{\"workload\":\"{workload}\",\"iteration\":{iteration},\"records\":{},\"wall_s\":{wall:.4},\"records_per_s\":{:.0},\"files\":{},\"directories\":{},\"bytes\":{},\"names\":{},\"resolved\":{},\"orphans\":{},\"path_bytes\":{}}}",
            mft.records(),
            mft.records() as f64 / wall,
            totals.files,
            totals.directories,
            totals.bytes,
            totals.names,
            totals.resolved,
            totals.orphans,
            totals.path_bytes,
        );
    }
}

fn generate(path: &Path, args: &[String]) {
    let records: u64 = arg(args, "--records").unwrap_or(1_000_000);
    let mut config = CorpusConfig::realistic(records);
    if let Some(size) = arg(args, "--record-size") {
        config.record_size = size;
    }
    if let Some(seed) = arg(args, "--seed") {
        config.seed = seed;
    }
    let started = Instant::now();
    let file = std::fs::File::create(path).expect("create the output");
    let mut out = BufWriter::with_capacity(8 << 20, file);
    let stats = write_mft(&config, &mut out, None).expect("write the corpus");
    out.flush().expect("flush");
    eprintln!(
        "generated {} records ({} directories, {} files, {} deleted, {} extension, {} damaged, {} empty) in {:.2}s",
        stats.records,
        stats.directories,
        stats.files,
        stats.deleted,
        stats.extensions,
        stats.damaged,
        stats.empty,
        started.elapsed().as_secs_f64()
    );
}
