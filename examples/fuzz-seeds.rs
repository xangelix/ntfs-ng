//! Writes seed inputs for the fuzz targets (`fuzz/corpus/<target>/`), from the synthetic
//! corpus and image builders: valid structures the fuzzer mutates from.
//!
//! fuzz-seeds <corpus-dir>

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::{fs, path::Path};

use ntfs_ng::testing::{
    corpus::{CorpusConfig, mft},
    image::{Content, FileSpec, ImageBuilder, ImageConfig, ROOT},
    lznt1_compress,
};

fn write(dir: &Path, target: &str, name: &str, bytes: &[u8]) {
    let dir = dir.join(target);
    fs::create_dir_all(&dir).expect("create the corpus directory");
    fs::write(dir.join(name), bytes).expect("write a seed");
}

fn main() {
    let dir = std::env::args().nth(1).expect("corpus directory");
    let dir = Path::new(&dir);
    let mut config = CorpusConfig::realistic(512);
    config.seed = 99;
    let (bytes, _) = mft(&config);
    // Records, one per seed (every kind the generator writes, damaged ones included).
    for (at, record) in bytes.chunks(1024).enumerate().step_by(3) {
        write(dir, "record", &format!("record-{at:04}"), record);
    }
    // Values: each attribute value of a few records.
    for (at, record) in bytes.chunks(1024).enumerate().take(64) {
        let mut buf = record.to_vec();
        if let Ok(parsed) = ntfs_ng::Record::fixup(&mut buf) {
            for (index, attribute) in parsed.attributes().map_while(Result::ok).enumerate() {
                if let Ok(value) = attribute.value() {
                    write(dir, "values", &format!("value-{at:04}-{index}"), value);
                }
            }
        }
    }
    // LZNT1: compressed units.
    let text: Vec<u8> = (0..20_000)
        .map(|i| b"seed text for lznt1 "[i % 20])
        .collect();
    write(dir, "lznt1", "text", &lznt1_compress(&text));
    write(dir, "lznt1", "short", &lznt1_compress(b"abcabcabcabc"));
    // $MFT dumps for the scan.
    for (at, chunk) in bytes.chunks(16 * 1024).enumerate().take(8) {
        write(dir, "mft_scan", &format!("mft-{at}"), chunk);
    }
    // Volumes: the target prepends a boot sector placing $MFT at cluster 1 of 512-byte
    // clusters, so a seed is $MFT's records followed by data.
    let mut builder = ImageBuilder::new(ImageConfig {
        cluster_size: 512,
        clusters: 1024,
        mft_records: 32,
        ..ImageConfig::default()
    });
    builder.add(FileSpec::file(
        "a",
        ROOT,
        Content::Resident(b"hello".to_vec()),
    ));
    builder.add(FileSpec::file(
        "b",
        ROOT,
        Content::Contiguous(vec![7; 3000]),
    ));
    let image = builder.build();
    let (lcn, clusters) = image.mft_extents[0];
    let start = (lcn * 512) as usize;
    write(
        dir,
        "volume",
        "mft",
        &image.bytes[start..start + (clusters * 512) as usize],
    );
    write(
        dir,
        "pages",
        "record",
        bytes.get(..4096).unwrap_or_default(),
    );
    println!("seeds written to {}", dir.display());
}
