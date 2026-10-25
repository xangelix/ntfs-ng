//! NTFS volume images with content, for tests of volumes and streams.
//!
//! [`ImageBuilder`] lays out a volume: a boot sector, `$MFT` in one to three extents (the later
//! ones optionally named only by record 0's attribute list and held in extension records, the
//! last of them in a part of `$MFT` only the previous extension maps), `$Bitmap` with every
//! allocated cluster marked, the metadata files, and the files and directories asked for, with
//! content resident, contiguous, fragmented, sparse, compressed (LZNT1), uninitialized past a
//! point, or continued in an extension record; deleted files whose clusters another file took
//! since; alternate data streams.

use super::{
    lznt1_compress,
    record::{NonResidentSpec, RecordBuilder, attribute_list_entry, put, utf16},
};
use crate::{
    attribute::flags as attribute_flags,
    record::{FileReference, next_sequence},
    records, types,
    value::{Namespace, Times, file_attributes},
};

/// The volume's geometry.
#[derive(Clone, Copy, Debug)]
pub struct ImageConfig {
    /// Bytes per cluster.
    pub cluster_size: u64,
    /// Bytes per record.
    pub record_size: usize,
    /// Clusters on the volume.
    pub clusters: u64,
    /// Records in `$MFT`.
    pub mft_records: u64,
    /// Extents of `$MFT` (1 to 3).
    pub mft_fragments: usize,
    /// Whether `$MFT`'s extents after the first are held in extension records, named by record
    /// 0's attribute list.
    pub mft_in_extensions: bool,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            cluster_size: 4096,
            record_size: 1024,
            clusters: 8192,
            mft_records: 192,
            mft_fragments: 1,
            mft_in_extensions: false,
        }
    }
}

/// A stream's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    /// No `$DATA` bytes (a resident, empty value).
    Empty,
    /// In the record.
    Resident(Vec<u8>),
    /// In one run of clusters.
    Contiguous(Vec<u8>),
    /// In `n` runs of clusters, scattered.
    Fragmented(Vec<u8>, usize),
    /// `len` bytes, sparse but for the pieces given (cluster-aligned offsets).
    Sparse {
        /// The stream's length.
        len: u64,
        /// Stored pieces: offset (a multiple of the cluster size) and bytes.
        pieces: Vec<(u64, Vec<u8>)>,
    },
    /// LZNT1-compressed in units of 16 clusters.
    Compressed(Vec<u8>),
    /// Stored, but initialized only up to `initialized` bytes (the rest reads as zeros).
    Uninitialized {
        /// The stored bytes (the stream's data size).
        data: Vec<u8>,
        /// Bytes initialized.
        initialized: u64,
    },
}

impl Content {
    /// What reading the stream gives.
    #[must_use]
    pub fn expected(&self) -> Vec<u8> {
        match self {
            Self::Empty => Vec::new(),
            Self::Resident(data)
            | Self::Contiguous(data)
            | Self::Fragmented(data, _)
            | Self::Compressed(data) => data.clone(),
            Self::Sparse { len, pieces } => {
                let mut out = vec![0u8; usize::try_from(*len).unwrap_or(0)];
                for (offset, bytes) in pieces {
                    put(
                        &mut out,
                        usize::try_from(*offset).unwrap_or(usize::MAX),
                        bytes,
                    );
                }
                out
            }
            Self::Uninitialized { data, initialized } => {
                let mut out = data.clone();
                for byte in out
                    .iter_mut()
                    .skip(usize::try_from(*initialized).unwrap_or(usize::MAX))
                {
                    *byte = 0;
                }
                out
            }
        }
    }
}

/// A file or directory to place.
#[derive(Clone, Debug)]
pub struct FileSpec {
    /// Its name.
    pub name: String,
    /// The directory holding it: [`ROOT`] or the index of an earlier directory spec.
    pub parent: usize,
    /// Whether it is a directory.
    pub directory: bool,
    /// Whether it is deleted (its record freed after its name was written).
    pub deleted: bool,
    /// The unnamed `$DATA`.
    pub content: Content,
    /// Alternate data streams.
    pub streams: Vec<(String, Content)>,
    /// Whether its `$DATA` lives in an extension record, named by an attribute list.
    pub extension: bool,
    /// The deleted spec whose clusters this file now holds (its content is written over them).
    pub takes_clusters_of: Option<usize>,
}

/// The root, as a [`FileSpec::parent`].
pub const ROOT: usize = usize::MAX;

impl FileSpec {
    /// A file named `name` in `parent` holding `content`.
    #[must_use]
    pub fn file(name: &str, parent: usize, content: Content) -> Self {
        Self {
            name: name.to_owned(),
            parent,
            directory: false,
            deleted: false,
            content,
            streams: Vec::new(),
            extension: false,
            takes_clusters_of: None,
        }
    }

    /// A directory named `name` in `parent`.
    #[must_use]
    pub fn directory(name: &str, parent: usize) -> Self {
        Self {
            directory: true,
            ..Self::file(name, parent, Content::Empty)
        }
    }
}

/// A built image.
#[derive(Clone, Debug)]
pub struct Image {
    /// The volume's bytes.
    pub bytes: Vec<u8>,
    /// Each spec's record and sequence.
    pub records: Vec<FileReference>,
    /// `$MFT`'s extents: first cluster and clusters.
    pub mft_extents: Vec<(u64, u64)>,
    /// The configuration.
    pub config: ImageConfig,
}

/// Lays out a volume.
#[derive(Clone, Debug, Default)]
pub struct ImageBuilder {
    config: ImageConfig,
    specs: Vec<FileSpec>,
}

/// Bump allocation of clusters.
struct Clusters {
    next: u64,
    used: Vec<(u64, u64)>,
}

impl Clusters {
    fn take(&mut self, count: u64) -> u64 {
        let lcn = self.next;
        self.next += count;
        if count > 0 {
            self.used.push((lcn, count));
        }
        lcn
    }

    /// Leaves a gap (fragmentation).
    fn skip(&mut self, count: u64) {
        self.next += count;
    }
}

impl ImageBuilder {
    /// A builder for a volume of `config`'s geometry.
    #[must_use]
    pub fn new(config: ImageConfig) -> Self {
        Self {
            config,
            specs: Vec::new(),
        }
    }

    /// Adds a file or directory; returns its index (a parent for later specs).
    pub fn add(&mut self, spec: FileSpec) -> usize {
        self.specs.push(spec);
        self.specs.len() - 1
    }

    /// Builds the image.
    ///
    /// # Panics
    /// When the specs do not fit the volume or `$MFT` (a bug in the test).
    #[must_use]
    #[allow(clippy::too_many_lines, clippy::expect_used, clippy::panic)]
    pub fn build(&self) -> Image {
        let config = self.config;
        let cs = config.cluster_size;
        let rs = config.record_size;
        let size = usize::try_from(config.clusters * cs).expect("the image fits memory");
        let mut bytes = vec![0u8; size];
        let mut clusters = Clusters {
            next: 16,
            used: vec![(0, 2)],
        };
        let times = Times {
            created: 0x01d9_0000_0000_0000,
            modified: 0x01d9_0000_0001_0000,
            changed: 0x01d9_0000_0002_0000,
            accessed: 0x01d9_0000_0003_0000,
        };
        // $MFT's extents.
        let mft_bytes = config.mft_records * rs as u64;
        let mft_clusters = mft_bytes.div_ceil(cs);
        let fragments = config.mft_fragments.clamp(1, 3) as u64;
        let mut extents = Vec::new();
        let first = if fragments == 1 {
            mft_clusters
        } else {
            (mft_clusters / 2).max(32 * rs as u64 / cs)
        };
        let mut left = mft_clusters;
        for index in 0..fragments {
            let count = if index == 0 {
                first.min(left)
            } else if index + 1 == fragments {
                left
            } else {
                left / 2
            };
            extents.push((clusters.take(count), count));
            left -= count;
            clusters.skip(7);
        }
        let records_in = |extent: usize| {
            extents
                .get(extent)
                .map_or(0, |(_, count)| count * cs / rs as u64)
        };
        // Extension records holding $MFT's later extents: record 16 (in the first extent)
        // holds the second; the first record of the second extent holds the third.
        let chain = config.mft_in_extensions && fragments > 1;
        let mut reserved = Vec::new();
        if chain {
            reserved.push(16);
            if fragments == 3 {
                reserved.push(records_in(0));
            }
        }
        let mirror = clusters.take(1);
        let mft_bitmap = clusters.take(1);
        let log = clusters.take(4);
        let bitmap_clusters = config.clusters.div_ceil(8).div_ceil(cs);
        let bitmap_lcn = clusters.take(bitmap_clusters);
        clusters.skip(3);

        let mut mft = vec![0u8; usize::try_from(mft_bytes).expect("$MFT fits memory")];
        let place = |mft: &mut Vec<u8>, number: u64, record: Vec<u8>| {
            let at = usize::try_from(number).expect("record number") * rs;
            assert!(at + rs <= mft.len(), "record {number} is past $MFT");
            put(mft, at, &record);
        };
        let root = FileReference::new(records::ROOT, 5);
        let builder = |number: u64, sequence: u16| {
            RecordBuilder::new(number)
                .size(rs)
                .sequence(sequence)
                .update_sequence_number(u16::try_from(number + 1).unwrap_or(1))
        };
        let system = |number: u64, name: &str| {
            builder(number, u16::try_from(number.max(1)).unwrap_or(1))
                .links(1)
                .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
                .file_name(root, Namespace::Win32AndDos, name, times, 0)
        };
        let non_resident_spec = |runs: Vec<(u64, Option<u64>)>, data: u64| {
            let allocated: u64 = runs.iter().map(|(count, _)| count * cs).sum();
            NonResidentSpec {
                runs,
                allocated,
                data,
                initialized: data,
                ..NonResidentSpec::default()
            }
        };

        // Record 0, $MFT.
        let mut record0 = system(0, "$MFT").non_resident(
            types::BITMAP,
            "",
            &non_resident_spec(vec![(1, Some(mft_bitmap))], config.mft_records.div_ceil(8)),
        );
        let mft_data_runs: Vec<(u64, Option<u64>)> = extents
            .iter()
            .map(|&(lcn, count)| (count, Some(lcn)))
            .collect();
        if chain {
            let list_at = record0.next_instance();
            record0 = record0.attribute(Vec::new());
            let data_instance = record0.next_instance();
            let first_extent = mft_data_runs
                .first()
                .copied()
                .into_iter()
                .collect::<Vec<_>>();
            let mut spec = non_resident_spec(first_extent, mft_bytes);
            spec.allocated = mft_clusters * cs;
            record0 = record0.non_resident(types::DATA, "", &spec);
            let mut list = Vec::new();
            list.extend(attribute_list_entry(
                types::STANDARD_INFORMATION,
                &[],
                0,
                FileReference::new(0, 1),
                0,
            ));
            list.extend(attribute_list_entry(
                types::DATA,
                &[],
                0,
                FileReference::new(0, 1),
                data_instance,
            ));
            let mut vcn = extents.first().map_or(0, |(_, count)| *count);
            for (index, &extension) in reserved.iter().enumerate() {
                let (lcn, count) = extents
                    .get(index + 1)
                    .copied()
                    .expect("an extent per extension");
                list.extend(attribute_list_entry(
                    types::DATA,
                    &[],
                    vcn,
                    FileReference::new(extension, 1),
                    0,
                ));
                let ext = builder(extension, 1)
                    .base(FileReference::new(0, 1))
                    .non_resident(
                        types::DATA,
                        "",
                        &NonResidentSpec {
                            lowest_vcn: vcn,
                            runs: vec![(count, Some(lcn))],
                            ..NonResidentSpec::default()
                        },
                    )
                    .build();
                place(&mut mft, extension, ext);
                vcn += count;
            }
            record0 = record0.replace(
                list_at,
                super::record::resident(types::ATTRIBUTE_LIST, list_at, &[], &list, 0),
            );
        } else {
            let mut spec = non_resident_spec(mft_data_runs, mft_bytes);
            spec.allocated = mft_clusters * cs;
            record0 = record0.non_resident(types::DATA, "", &spec);
        }
        place(&mut mft, 0, record0.build());
        place(
            &mut mft,
            1,
            system(1, "$MFTMirr")
                .non_resident(
                    types::DATA,
                    "",
                    &non_resident_spec(vec![(1, Some(mirror))], cs),
                )
                .build(),
        );
        place(
            &mut mft,
            2,
            system(2, "$LogFile")
                .non_resident(
                    types::DATA,
                    "",
                    &non_resident_spec(vec![(4, Some(log))], 4 * cs),
                )
                .build(),
        );
        let mut info = vec![0u8; 12];
        info[8] = 3;
        info[9] = 1;
        place(
            &mut mft,
            3,
            system(3, "$Volume")
                .resident(types::VOLUME_NAME, "", &utf16("synthetic"))
                .resident(types::VOLUME_INFORMATION, "", &info)
                .resident(types::DATA, "", &[])
                .build(),
        );
        place(
            &mut mft,
            4,
            system(4, "$AttrDef").resident(types::DATA, "", &[]).build(),
        );
        place(
            &mut mft,
            5,
            builder(5, 5)
                .directory(true)
                .links(1)
                .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
                .file_name(root, Namespace::Win32AndDos, ".", times, 0)
                .build(),
        );
        let bitmap_bytes = config.clusters.div_ceil(8);
        place(
            &mut mft,
            6,
            system(6, "$Bitmap")
                .non_resident(
                    types::DATA,
                    "",
                    &non_resident_spec(vec![(bitmap_clusters, Some(bitmap_lcn))], bitmap_bytes),
                )
                .build(),
        );
        place(
            &mut mft,
            7,
            system(7, "$Boot")
                .non_resident(
                    types::DATA,
                    "",
                    &non_resident_spec(vec![(2, Some(0))], 2 * cs),
                )
                .build(),
        );
        place(
            &mut mft,
            8,
            system(8, "$BadClus").resident(types::DATA, "", &[]).build(),
        );
        place(&mut mft, 9, system(9, "$Secure").build());
        place(
            &mut mft,
            10,
            system(10, "$UpCase").resident(types::DATA, "", &[]).build(),
        );
        place(
            &mut mft,
            11,
            builder(11, 11)
                .directory(true)
                .links(1)
                .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
                .file_name(root, Namespace::Win32AndDos, "$Extend", times, 0)
                .build(),
        );
        for number in 12..16 {
            place(
                &mut mft,
                number,
                builder(number, u16::try_from(number).unwrap_or(1))
                    .standard_information(times, 0)
                    .build(),
            );
        }

        // The specs, from record 16 on (skipping reserved ones).
        let mut next = 16u64;
        let mut take_number = |reserved: &[u64]| {
            while reserved.contains(&next) {
                next += 1;
            }
            let number = next;
            next += 1;
            number
        };
        let mut references: Vec<FileReference> = Vec::new();
        let mut placed_clusters: Vec<Vec<(u64, Option<u64>)>> = Vec::new();
        let data = |clusters: &mut Clusters,
                    bytes: &mut [u8],
                    content: &Content,
                    reuse: Option<&Vec<(u64, Option<u64>)>>|
         -> (Vec<(u64, Option<u64>)>, NonResidentSpec) {
            let write = |bytes: &mut [u8], lcn: u64, piece: &[u8]| {
                put(
                    bytes,
                    usize::try_from(lcn * cs).unwrap_or(usize::MAX),
                    piece,
                );
            };
            match content {
                Content::Contiguous(payload) | Content::Uninitialized { data: payload, .. } => {
                    let count = (payload.len() as u64).div_ceil(cs);
                    let lcn = match reuse
                        .and_then(|runs| runs.first())
                        .and_then(|(_, lcn)| *lcn)
                    {
                        Some(lcn) => lcn,
                        None => clusters.take(count),
                    };
                    write(bytes, lcn, payload);
                    let runs = if count == 0 {
                        Vec::new()
                    } else {
                        vec![(count, Some(lcn))]
                    };
                    let mut spec = non_resident_spec(runs.clone(), payload.len() as u64);
                    if let Content::Uninitialized { initialized, .. } = content {
                        spec.initialized = *initialized;
                    }
                    (runs, spec)
                }
                Content::Fragmented(payload, pieces) => {
                    let count = (payload.len() as u64).div_ceil(cs);
                    let pieces = (*pieces as u64).clamp(1, count.max(1));
                    let mut runs = Vec::new();
                    let mut done = 0u64;
                    for index in 0..pieces {
                        let take = if index + 1 == pieces {
                            count - done
                        } else {
                            count / pieces
                        };
                        let lcn = clusters.take(take);
                        clusters.skip(3);
                        let start = usize::try_from(done * cs).unwrap_or(usize::MAX);
                        let end = usize::try_from(((done + take) * cs).min(payload.len() as u64))
                            .unwrap_or(usize::MAX);
                        write(bytes, lcn, payload.get(start..end).unwrap_or_default());
                        runs.push((take, Some(lcn)));
                        done += take;
                    }
                    (runs.clone(), non_resident_spec(runs, payload.len() as u64))
                }
                Content::Sparse { len, pieces } => {
                    let count = len.div_ceil(cs);
                    let mut runs = Vec::new();
                    let mut at = 0u64;
                    for (offset, piece) in pieces {
                        let start = offset / cs;
                        if start > at {
                            runs.push((start - at, None));
                        }
                        let take = (piece.len() as u64).div_ceil(cs);
                        let lcn = clusters.take(take);
                        write(bytes, lcn, piece);
                        runs.push((take, Some(lcn)));
                        at = start + take;
                    }
                    if count > at {
                        runs.push((count - at, None));
                    }
                    let mut spec = non_resident_spec(runs.clone(), *len);
                    spec.allocated = count * cs;
                    spec.flags = attribute_flags::SPARSE;
                    spec.total_allocated = Some(
                        runs.iter()
                            .filter(|(_, lcn)| lcn.is_some())
                            .map(|(n, _)| n * cs)
                            .sum(),
                    );
                    (runs, spec)
                }
                Content::Compressed(payload) => {
                    let unit = 16 * cs;
                    let mut runs = Vec::new();
                    let mut stored = 0;
                    for chunk in payload.chunks(usize::try_from(unit).unwrap_or(usize::MAX)) {
                        let packed = lznt1_compress(chunk);
                        let unit_clusters = (chunk.len() as u64).div_ceil(cs);
                        let packed_clusters = (packed.len() as u64).div_ceil(cs);
                        if packed_clusters < unit_clusters.min(16) {
                            let lcn = clusters.take(packed_clusters);
                            write(bytes, lcn, &packed);
                            runs.push((packed_clusters, Some(lcn)));
                            runs.push((16 - packed_clusters, None));
                            stored += packed_clusters;
                        } else {
                            // Stored whole, as NTFS does with a unit that does not shrink.
                            let lcn = clusters.take(16);
                            write(bytes, lcn, chunk);
                            runs.push((16, Some(lcn)));
                            stored += 16;
                        }
                    }
                    let mut spec = non_resident_spec(runs.clone(), payload.len() as u64);
                    spec.allocated = runs.iter().map(|(n, _)| n * cs).sum();
                    spec.compression_unit = 4;
                    spec.flags = 1;
                    spec.total_allocated = Some(stored * cs);
                    (merge_runs(runs.clone()), {
                        spec.runs = merge_runs(spec.runs);
                        spec
                    })
                }
                Content::Empty | Content::Resident(_) => (Vec::new(), NonResidentSpec::default()),
            }
        };
        let mut extensions = Vec::new();
        let numbers: Vec<u64> = self.specs.iter().map(|_| take_number(&reserved)).collect();
        for (spec, &number) in self.specs.iter().zip(&numbers) {
            let sequence = 1 + u16::try_from(number % 7).unwrap_or(0);
            let now = if spec.deleted {
                next_sequence(sequence)
            } else {
                sequence
            };
            let parent = match spec.parent {
                ROOT => root,
                index => {
                    let reference = *references.get(index).expect("parents come first");
                    // A name records the parent's sequence when written: one less if the parent
                    // was freed since.
                    let deleted = self.specs.get(index).is_some_and(|parent| parent.deleted);
                    if deleted {
                        FileReference::new(
                            reference.record,
                            crate::record::previous_sequence(reference.sequence).unwrap_or(1),
                        )
                    } else {
                        reference
                    }
                }
            };
            let reuse = spec
                .takes_clusters_of
                .and_then(|index| placed_clusters.get(index));
            let mut record = builder(number, now)
                .directory(spec.directory)
                .in_use(!spec.deleted)
                .links(1)
                .standard_information(
                    times,
                    if spec.directory {
                        file_attributes::DIRECTORY
                    } else {
                        file_attributes::ARCHIVE
                    },
                );
            let list_at = record.next_instance();
            if spec.extension {
                record = record.attribute(Vec::new());
            }
            let data_size = spec.content.expected().len() as u64;
            record = record.file_name(parent, Namespace::Win32, &spec.name, times, data_size);
            let (runs, layout) = data(&mut clusters, &mut bytes, &spec.content, reuse);
            placed_clusters.push(runs);
            if spec.extension {
                let extension = take_number(&reserved);
                let reference = FileReference::new(number, now);
                let ext = builder(extension, 1)
                    .base(reference)
                    .non_resident(types::DATA, "", &layout)
                    .build();
                extensions.push((extension, ext));
                let mut list = Vec::new();
                list.extend(attribute_list_entry(
                    types::STANDARD_INFORMATION,
                    &[],
                    0,
                    reference,
                    0,
                ));
                list.extend(attribute_list_entry(
                    types::FILE_NAME,
                    &[],
                    0,
                    reference,
                    list_at + 1,
                ));
                list.extend(attribute_list_entry(
                    types::DATA,
                    &[],
                    0,
                    FileReference::new(extension, 1),
                    0,
                ));
                record = record.replace(
                    list_at,
                    super::record::resident(types::ATTRIBUTE_LIST, list_at, &[], &list, 0),
                );
            } else {
                record = match &spec.content {
                    Content::Empty if !spec.directory => record.resident(types::DATA, "", &[]),
                    Content::Empty => record,
                    Content::Resident(payload) => record.resident(types::DATA, "", payload),
                    _ => record.non_resident(types::DATA, "", &layout),
                };
            }
            for (name, content) in &spec.streams {
                record = match content {
                    Content::Resident(payload) => record.resident(types::DATA, name, payload),
                    other => {
                        let (_, layout) = data(&mut clusters, &mut bytes, other, None);
                        record.non_resident(types::DATA, name, &layout)
                    }
                };
            }
            place(&mut mft, number, record.build());
            references.push(FileReference::new(number, now));
        }
        for (number, record) in extensions {
            place(&mut mft, number, record);
        }
        // $Bitmap: every cluster taken, but those of deleted files nobody took since.
        let mut bitmap = vec![0u8; usize::try_from(bitmap_bytes).unwrap_or(0)];
        let freed: Vec<&Vec<(u64, Option<u64>)>> = self
            .specs
            .iter()
            .zip(&placed_clusters)
            .filter(|(spec, _)| spec.deleted)
            .map(|(_, runs)| runs)
            .collect();
        let retaken: Vec<usize> = self
            .specs
            .iter()
            .filter_map(|spec| spec.takes_clusters_of)
            .collect();
        let is_freed = |lcn: u64| {
            freed.iter().any(|runs| {
                runs.iter().any(|(count, start)| {
                    start.is_some_and(|start| (start..start + count).contains(&lcn))
                })
            })
        };
        let is_retaken = |lcn: u64| {
            retaken.iter().any(|&index| {
                placed_clusters.get(index).is_some_and(|runs| {
                    runs.iter().any(|(count, start)| {
                        start.is_some_and(|start| (start..start + count).contains(&lcn))
                    })
                })
            })
        };
        for &(start, count) in &clusters.used {
            for lcn in start..start + count {
                if is_freed(lcn) && !is_retaken(lcn) {
                    continue;
                }
                if let Some(byte) = bitmap.get_mut(usize::try_from(lcn / 8).unwrap_or(usize::MAX)) {
                    *byte |= 1 << (lcn % 8);
                }
            }
        }
        put(
            &mut bytes,
            usize::try_from(bitmap_lcn * cs).unwrap_or(usize::MAX),
            &bitmap,
        );
        assert!(
            clusters.next <= config.clusters,
            "the specs do not fit the volume"
        );
        // $MFT into its extents.
        let mut at = 0usize;
        for &(lcn, count) in &extents {
            let len = usize::try_from(count * cs).unwrap_or(0).min(mft.len() - at);
            put(
                &mut bytes,
                usize::try_from(lcn * cs).unwrap_or(usize::MAX),
                mft.get(at..at + len).unwrap_or_default(),
            );
            at += len;
        }
        // The boot sector.
        let mut boot = vec![0u8; 512];
        boot[0..3].copy_from_slice(&[0xeb, 0x52, 0x90]);
        boot[3..11].copy_from_slice(b"NTFS    ");
        put(&mut boot, 0x0b, &512u16.to_le_bytes());
        boot[0x0d] = u8::try_from(cs / 512).unwrap_or(8);
        boot[0x15] = 0xf8;
        put(
            &mut boot,
            0x28,
            &(config.clusters * cs / 512 - 1).to_le_bytes(),
        );
        put(
            &mut boot,
            0x30,
            &extents.first().map_or(0, |(lcn, _)| *lcn).to_le_bytes(),
        );
        put(&mut boot, 0x38, &mirror.to_le_bytes());
        #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
        let record_byte = if rs as u64 >= cs {
            (rs as u64 / cs) as u8
        } else {
            (-(rs.trailing_zeros() as i8)) as u8
        };
        boot[0x40] = record_byte;
        boot[0x44] = 1;
        put(&mut boot, 0x48, &0x1234_5678_9abc_def0u64.to_le_bytes());
        boot[0x1fe] = 0x55;
        boot[0x1ff] = 0xaa;
        put(&mut bytes, 0, &boot);
        Image {
            bytes,
            records: references,
            mft_extents: extents,
            config,
        }
    }
}

/// Merges adjacent sparse runs.
fn merge_runs(runs: Vec<(u64, Option<u64>)>) -> Vec<(u64, Option<u64>)> {
    let mut out: Vec<(u64, Option<u64>)> = Vec::new();
    for (count, lcn) in runs {
        match (out.last_mut(), lcn) {
            (Some((last, None)), None) => *last += count,
            _ => out.push((count, lcn)),
        }
    }
    out.retain(|(count, _)| *count > 0);
    out
}
