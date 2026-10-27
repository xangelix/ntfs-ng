# ntfs-ng

Fast, hardened NTFS and `$MFT` parsing for Rust.

`ntfs-ng` reads the Master File Table of a live volume, a disk image or an exported `$MFT`
dump, and gives you records, files, names, paths and streams — at memory speed, from as many
cores as you have, without trusting a single byte of the input.

```rust
use ntfs_ng::{files::{FileOptions, Files}, mft::Mft, scan::Scan, source::FileSource};

let mft = Mft::from_file(FileSource::open("$MFT")?)?;
let files = Files::scan(&Scan::new(&mft), mft.records(), FileOptions::LIVE)?;
let parents = files.parents(8);
for (id, link) in files.table.links() {
    let file = files.table.file(link.file).unwrap();
    let dir = parents[id as usize];
    if dir != ntfs_ng::index::NONE && !file.is_directory() {
        println!("{}\\{} {} bytes", files.index.path(dir, '\\'), link.name, file.size);
    }
}
# Ok::<(), ntfs_ng::Error>(())
```

## Why another NTFS crate

The same `$MFT` can be read very differently:

- a few fields of every live file, as fast as the disk can deliver them;
- every byte of every record checked, deleted records included, with the bytes as stored kept;
- live volumes, with attribute lists followed into extension records only when they truly belong
  to the file, streams copied without a byte ever invented, and hostile images survived.

`ntfs-ng` does all of it from one core, because it decides nothing for you: you choose what to
read from each record (a [`Projection`](src/scan.rs)), how strictly to judge it, and how paths
are resolved (a [`PathPolicy`](src/index.rs)).

## What's inside

| Layer | Modules | Notes |
|---|---|---|
| Structures (`no_std`) | `record`, `attribute`, `runs`, `value`, `name`, `usa`, `boot`, `lznt1`, `time` | Zero-copy views; every length, count and offset checked; fused iterators; in-place update sequences that undo exactly |
| Sources & volumes | `source`, `mft`, `volume` | Positional reads with holes and alignment (raw devices, images, dumps, anything `ReadAt`); `$MFT` mapped through every extent, extension records included; streams with every hole typed (sparse, uninitialized, unmapped, past the volume, unreadable, undecodable, reallocated); LZNT1; `$Bitmap` |
| Scanning & paths | `scan`, `index`, `files`, `usage` | A parallel scan into your own per-chunk outputs, ordered or unordered; a directory index with explicit parent-sequence, deleted-directory, depth and root policies; files with their names, sizes, times and attributes, extension records folded in; and the disk-usage table, a third of that memory: each live name with its file's size, allocated size and two times |
| Test support (`testing`) | `testing` | Byte-exact record builders, a deterministic generator of realistic `$MFT`s of any size with ground truth, and volume images with content |

## Performance

Measured with [`examples/ntfs-bench`](examples/ntfs-bench) on a 16-core Ryzen 9 9950X3D, page
cache warm. Run `./scripts/bench.sh` to reproduce on synthetic corpora, or pass your own
`--mft` dumps and `--image`s (they never leave your machine).

| Workload | What it does | Real volume, 1.85 M records (1.9 GB) |
|---|---|---|
| `du` | live files: names, sizes, times; extension records folded in; tree built and walked (the disk-usage table) | **0.10 s** |
| `paths` | every record, a strict directory index, a full path for every name | **0.18 s** |
| `inventory` | every record in use or deleted, every name placed (deleted directories included), every stream listed | **0.47 s** |
| `dirs` | the directory index alone | **0.06 s** |

The benchmark suites go well beyond that: Criterion micro-benchmarks of every parser
(`benches/parsers.rs`) and of the engine by projection, thread count, chunk size, delivery
order, record size and stream kind (`benches/engine.rs`), and the parsers' instruction counts
under Valgrind (`benches/instructions.rs`, with [gungraun](https://docs.rs/gungraun)), which are
exact whatever the machine's load. Every pull request is compared with its base revision on
both: wall-clock runs of the two take turns on one runner, group by group
(`scripts/criterion-ab.sh`), and instructions are counted for each
(`.github/workflows/bench.yml`).

## Guarantees

- `#![forbid(unsafe_code)]`. No dependencies.
- No input panics a reader, loops forever, or allocates without bound. Every parser is fuzzed
  (`fuzz/`), in CI and nightly.
- No byte is invented: a record's update sequence is applied in place and undone exactly
  (`Record::raw_into`); a stream's holes are reported as holes, with their reason; a deleted
  file checked against `$Bitmap` never yields another file's clusters.
- An extension record is folded into a file only when both are in use and it names that file,
  at its current sequence, as its base.

## Features

- `std` (default): sources, volumes, the scan, the index and the file table. Without it, the
  structure parsers build for `no_std` targets (with `alloc`).
- `testing`: synthetic records, `$MFT`s and images, for your own tests.

Minimum supported Rust version: 1.99.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
