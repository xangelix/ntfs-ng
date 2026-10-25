//! The inputs of the parser benchmarks, shared by their wall-clock runs (`parsers.rs`) and their
//! instruction counts (`instructions.rs`). Each is a batch of different inputs of one kind, made
//! from a fixed seed: an iteration parses all of them, so that it measures a mix rather than one
//! input's best case, and lasts long enough that the harness's own cost and where the code
//! happens to be aligned weigh little.
//!
//! Names and mapping pairs, whose parsers branch on every character or pair, come as a pool of
//! [`ROTATION`] batches that wall-clock runs take in turn ([`Rotation`]): the same batch every
//! iteration lets a branch predictor learn its every branch, and measures a best case that no
//! caller sees, larger or smaller depending on where the code lies.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation,
    dead_code,
    unreachable_pub
)]

use ntfs_ng::{
    FileReference, Record,
    testing::{
        Rng, lznt1_compress,
        record::{
            NonResidentSpec, RecordBuilder, attribute_list_entry, file_name, mapping_pairs,
            protect, standard_information, utf16,
        },
    },
    types,
    value::{Namespace, Times},
};

/// Records per batch: 64 KiB of 1 KiB records, 256 KiB of 4 KiB ones.
pub const RECORDS: usize = 64;
/// Names, and values, per batch.
pub const NAMES: usize = 256;
/// Mapping pairs per batch, in arrays of 1, 16 or 256.
pub const PAIRS: usize = 1024;
/// Batches in a pool of names or mapping pairs.
pub const ROTATION: usize = 16;

/// The batches of a pool, in turn.
pub struct Rotation<'a, T> {
    pool: &'a [T],
    batch: usize,
    next: usize,
}

impl<'a, T> Rotation<'a, T> {
    /// The batches of `batch` items of `pool`.
    pub fn new(pool: &'a [T], batch: usize) -> Self {
        Self {
            pool,
            batch,
            next: 0,
        }
    }

    /// The next batch, after the last the first again.
    pub fn next_batch(&mut self) -> &'a [T] {
        let start = self.next * self.batch;
        self.next = (self.next + 1) % (self.pool.len() / self.batch);
        &self.pool[start..start + self.batch]
    }
}
/// `RCRD` pages, and boot sectors, per batch.
pub const PAGES: usize = 64;

fn times(rng: &mut Rng) -> Times {
    let base = 0x01d9_0000_0000_0000 + rng.below(1 << 40);
    Times {
        created: base,
        modified: base + rng.below(1 << 30),
        changed: base + rng.below(1 << 31),
        accessed: base + rng.below(1 << 32),
    }
}

/// A file name of `length` characters drawn from `alphabet`.
fn text(rng: &mut Rng, alphabet: &[char], length: usize) -> String {
    (0..length)
        .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
        .collect()
}

const ASCII: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ._-()~";

/// `RECORDS` typical file records of `size` bytes, as stored: `$STANDARD_INFORMATION`, a long
/// name and its 8.3 alias, and a non-resident `$DATA` of 1 to 8 runs, each different.
pub fn records(size: usize) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(size as u64);
    let alphabet: Vec<char> = ASCII.chars().collect();
    (0..RECORDS as u64)
        .map(|number| {
            let parent = FileReference::new(16 + rng.below(1 << 20), 1 + rng.below(9) as u16);
            let length = 8 + rng.below(40) as usize;
            let name = text(&mut rng, &alphabet, length) + ".dat";
            let runs: Vec<(u64, Option<u64>)> = (0..1 + rng.below(8))
                .map(|_| (1 + rng.below(512), Some(rng.below(1 << 30))))
                .collect();
            let clusters: u64 = runs.iter().map(|run| run.0).sum();
            let data = clusters * 4096 - rng.below(4096);
            let times = times(&mut rng);
            RecordBuilder::new(100 + number)
                .size(size)
                .standard_information(times, 0x20)
                .file_name(parent, Namespace::Win32, &name, times, data)
                .file_name(parent, Namespace::Dos, "FILE~1.DAT", times, data)
                .non_resident(
                    types::DATA,
                    "",
                    &NonResidentSpec {
                        runs,
                        allocated: clusters * 4096,
                        data,
                        initialized: data,
                        ..NonResidentSpec::default()
                    },
                )
                .build()
        })
        .collect()
}

/// The records of [`records`], with their update sequences applied.
pub fn fixed(size: usize) -> Vec<Vec<u8>> {
    records(size)
        .into_iter()
        .map(|mut record| {
            Record::fixup(&mut record).unwrap();
            record
        })
        .collect()
}

/// The name classes the name benchmarks run on: a label (characters, and UTF-16 code units),
/// and the characters names of that class are drawn from.
pub const NAME_CLASSES: [(&str, &str, usize); 6] = [
    ("ascii-12", ASCII, 12),
    ("ascii-64", ASCII, 64),
    ("ascii-255", ASCII, 255),
    ("bmp-12", "日本語のファイル名です資料報告書写真動画音楽", 12),
    ("mixed-40", "données_Übersicht_ñandú_русский_Ελληνικά", 40),
    ("surrogates-16", "😀😁😂🤣😃😄😅😆😇😈😉😊😋😌😍😎", 8),
];

/// `ROTATION` batches of `NAMES` names of class `label` ([`NAME_CLASSES`]), as UTF-16LE, each
/// with the same name in another ASCII case (for case-insensitive comparison).
pub fn names(label: &str) -> Vec<(Vec<u8>, String)> {
    let (_, alphabet, length) = NAME_CLASSES
        .iter()
        .find(|class| class.0 == label)
        .expect("a name class");
    let alphabet: Vec<char> = alphabet.chars().collect();
    let mut rng = Rng::new(label.len() as u64 + *length as u64);
    (0..NAMES * ROTATION)
        .map(|_| {
            let name = text(&mut rng, &alphabet, *length);
            let other_case: String = name
                .chars()
                .map(|c| {
                    if rng.chance(500_000) {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect();
            (utf16(&name), other_case)
        })
        .collect()
}

/// `NAMES` `$FILE_NAME` values, of names 1 to 64 characters long in every namespace.
pub fn file_names() -> Vec<Vec<u8>> {
    let mut rng = Rng::new(3);
    let alphabet: Vec<char> = ASCII.chars().collect();
    let namespaces = [
        Namespace::Win32,
        Namespace::Dos,
        Namespace::Win32AndDos,
        Namespace::Posix,
    ];
    (0..NAMES)
        .map(|at| {
            let length = 1 + rng.below(64) as usize;
            let name = text(&mut rng, &alphabet, length);
            let data = rng.below(1 << 32);
            file_name(
                FileReference::new(5 + rng.below(1 << 20), 1 + rng.below(9) as u16),
                namespaces[at % namespaces.len()],
                &name,
                times(&mut rng),
                data.next_multiple_of(4096),
                data,
                0x20,
            )
        })
        .collect()
}

/// `NAMES` `$STANDARD_INFORMATION` values.
pub fn standard_information_values() -> Vec<Vec<u8>> {
    let mut rng = Rng::new(4);
    (0..NAMES)
        .map(|_| standard_information(times(&mut rng), rng.below(1 << 16) as u32))
        .collect()
}

/// Attribute lists of 40 entries each, `NAMES` entries in all.
pub fn attribute_lists() -> Vec<Vec<u8>> {
    let mut rng = Rng::new(5);
    (0..NAMES / 40 + 1)
        .map(|_| {
            let mut list = Vec::new();
            for at in 0..40u64 {
                let name = if rng.chance(250_000) {
                    utf16("$I30")
                } else {
                    Vec::new()
                };
                list.extend(attribute_list_entry(
                    [types::DATA, types::FILE_NAME, types::INDEX_ALLOCATION][at as usize % 3],
                    &name,
                    at * 16,
                    FileReference::new(9000 + rng.below(1 << 20), 1),
                    at as u16,
                ));
            }
            list
        })
        .collect()
}

/// `ROTATION` batches of mapping pairs arrays of `count` runs each, `PAIRS` runs to a batch:
/// lengths of 1 to 5,000 clusters, one run in ten sparse, the rest anywhere on a 256 TiB volume.
pub fn pair_arrays(count: usize) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(7 + count as u64);
    (0..PAIRS / count * ROTATION)
        .map(|_| {
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
            mapping_pairs(&runs)
        })
        .collect()
}

/// A compression unit (64 KiB) of each kind, LZNT1-compressed: text, text with noise, noise.
pub fn compressed() -> [(&'static str, Vec<u8>); 3] {
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
    [
        ("text", lznt1_compress(&text)),
        ("mixed", lznt1_compress(&mixed)),
        ("noise", lznt1_compress(&noise)),
    ]
}

/// `PAGES` 4 KiB `RCRD` pages, as stored, with different contents and update sequence
/// numbers.
pub fn pages() -> Vec<Vec<u8>> {
    let mut rng = Rng::new(13);
    (0..PAGES)
        .map(|_| {
            let mut page: Vec<u8> = (0..4096).map(|_| rng.next_u64() as u8).collect();
            page[0..4].copy_from_slice(b"RCRD");
            page[4..6].copy_from_slice(&40u16.to_le_bytes());
            page[6..8].copy_from_slice(&9u16.to_le_bytes());
            protect(&mut page, 40, 1 + rng.below(0xfffe) as u16);
            page
        })
        .collect()
}

/// `PAGES` NTFS boot sectors of volumes of different sizes and geometries.
pub fn boot_sectors() -> Vec<Vec<u8>> {
    let mut rng = Rng::new(17);
    (0..PAGES)
        .map(|_| {
            let mut boot = vec![0u8; 512];
            boot[3..11].copy_from_slice(b"NTFS    ");
            boot[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
            let sectors_per_cluster = [1u8, 2, 4, 8, 16, 32, 64, 128][rng.below(8) as usize];
            let sectors = 1u64 << (20 + rng.below(20));
            let clusters = sectors / u64::from(sectors_per_cluster);
            boot[0x0d] = sectors_per_cluster;
            boot[0x28..0x30].copy_from_slice(&sectors.to_le_bytes());
            boot[0x30..0x38].copy_from_slice(&(4 + rng.below(clusters / 4)).to_le_bytes());
            boot[0x38..0x40]
                .copy_from_slice(&(clusters / 2 + rng.below(clusters / 4)).to_le_bytes());
            boot[0x40] = [0xf6, 0xf4][rng.below(2) as usize];
            boot[0x44] = [0x01, 0xf4][rng.below(2) as usize];
            boot[0x48..0x50].copy_from_slice(&rng.next_u64().to_le_bytes());
            boot[0x1fe] = 0x55;
            boot[0x1ff] = 0xaa;
            boot
        })
        .collect()
}
