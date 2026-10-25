//! A deterministic generator of realistic `$MFT`s, streamed, with their ground truth.
//!
//! The mix follows real volumes: about one record in six is a directory, a third of the slots
//! hold deleted records, a few percent of files are hard-linked across directories, have
//! alternate data streams, reparse points, or continue in an extension record through an
//! attribute list, and names come in Win32, Win32-and-DOS, POSIX and 8.3 forms, some in
//! scripts beyond ASCII (surrogate pairs included), some 255 units long. Deletion is
//! bottom-up, as NTFS does it (a directory is emptied before it goes), each freed record's
//! sequence is one more than its last use, and some deleted names point at directories whose
//! records were reused since (orphans). A few slots are damaged (`BAAD`, torn) and the tail is
//! never used.
//!
//! [`write_mft`] streams the records to any writer (gigabytes need not fit in memory);
//! [`Truth`] holds what was written, for tests to check parsers against.

use std::io::{self, Write};

use super::{
    Rng,
    record::{NonResidentSpec, RecordBuilder, attribute_list_entry, utf16},
};
use crate::{
    record::FileReference,
    records, types,
    value::{Namespace, Times, file_attributes, tags},
};

/// What to generate. Rates are per million records (or files, where noted).
#[derive(Clone, Copy, Debug)]
pub struct CorpusConfig {
    /// Slots in `$MFT`.
    pub records: u64,
    /// Bytes per record.
    pub record_size: usize,
    /// The seed: the same configuration gives the same bytes.
    pub seed: u64,
    /// Directories among new records.
    pub directories: u32,
    /// Deleted records among new records.
    pub deleted: u32,
    /// Hard-linked files among files.
    pub hard_links: u32,
    /// Files with an alternate data stream.
    pub ads: u32,
    /// Files whose `$DATA` continues in an extension record.
    pub extensions: u32,
    /// Files with a reparse point.
    pub reparse: u32,
    /// Names beyond ASCII.
    pub unicode: u32,
    /// Names of 100 to 255 units.
    pub long_names: u32,
    /// Long names with a separate 8.3 alias.
    pub dos_aliases: u32,
    /// POSIX names.
    pub posix: u32,
    /// Damaged slots.
    pub damaged: u32,
    /// Deleted names whose parent directory's record was reused since.
    pub orphans: u32,
    /// Never-used slots at the end.
    pub tail: u32,
    /// The deepest directory.
    pub max_depth: u16,
}

impl CorpusConfig {
    /// A realistic `$MFT` of `records` 1 KiB records.
    #[must_use]
    pub const fn realistic(records: u64) -> Self {
        Self {
            records,
            record_size: 1024,
            seed: 0x6e74_6673_2d6e_6721,
            directories: 170_000,
            deleted: 300_000,
            hard_links: 27_000,
            ads: 15_000,
            extensions: 20_000,
            reparse: 12_000,
            unicode: 40_000,
            long_names: 15_000,
            dos_aliases: 250_000,
            posix: 5_000,
            damaged: 100,
            orphans: 50_000,
            tail: 15_000,
            max_depth: 24,
        }
    }
}

/// A directory, as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruthDirectory {
    /// Its record and sequence now.
    pub reference: FileReference,
    /// Whether it is in use.
    pub in_use: bool,
    /// Its parent reference, as its name records it.
    pub parent: FileReference,
    /// Its name.
    pub name: String,
}

/// A name of a file, as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruthName {
    /// The parent reference the name records.
    pub parent: FileReference,
    /// The name.
    pub name: String,
    /// Its namespace.
    pub namespace: Namespace,
}

/// A file (not a directory), as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruthFile {
    /// Its record and sequence now.
    pub reference: FileReference,
    /// Whether it is in use.
    pub in_use: bool,
    /// Its names, DOS aliases included.
    pub names: Vec<TruthName>,
    /// The unnamed `$DATA`'s size.
    pub size: u64,
    /// Alternate data streams: name and size.
    pub streams: Vec<(String, u64)>,
    /// The extension record holding its `$DATA`, if any.
    pub extension: Option<u64>,
    /// Its reparse tag, if any.
    pub reparse: Option<u32>,
}

/// Everything written.
#[derive(Clone, Debug, Default)]
pub struct Truth {
    /// Bytes per record.
    pub record_size: usize,
    /// Slots written.
    pub records: u64,
    /// Directories (the root included), by record.
    pub directories: Vec<TruthDirectory>,
    /// Files and metadata files, by record.
    pub files: Vec<TruthFile>,
    /// Extension records, by record.
    pub extensions: Vec<u64>,
    /// Damaged slots.
    pub damaged: Vec<u64>,
    /// Never-used slots.
    pub empty: Vec<u64>,
}

/// Counts of what [`write_mft`] wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CorpusStats {
    /// Slots.
    pub records: u64,
    /// Directories in use.
    pub directories: u64,
    /// Files in use.
    pub files: u64,
    /// Deleted records.
    pub deleted: u64,
    /// Extension records.
    pub extensions: u64,
    /// Damaged slots.
    pub damaged: u64,
    /// Never-used slots.
    pub empty: u64,
}

const SYLLABLES: [&str; 32] = [
    "ka", "lo", "mi", "nu", "pe", "ra", "si", "to", "va", "zu", "bel", "cor", "dan", "fin", "gal",
    "hex", "ion", "jet", "kin", "lux", "mon", "nor", "orb", "pix", "qua", "rio", "sol", "tek",
    "una", "vex", "wyn", "xor",
];
const EXTENSIONS: [&str; 16] = [
    "txt", "dll", "exe", "json", "png", "jpg", "log", "dat", "sys", "ini", "xml", "html", "js",
    "rs", "pdf", "mp4",
];
const UNICODE: [&str; 12] = [
    "日本語",
    "données",
    "Ελληνικά",
    "русский",
    "한국어",
    "中文文件",
    "ñandú",
    "Übersicht",
    "😀emoji",
    "𝄞music",
    "عربي",
    "עברית",
];

/// A directory being generated.
#[derive(Clone, Copy, Debug)]
struct Dir {
    reference: FileReference,
    depth: u16,
}

struct Generator<'a, W: Write> {
    config: CorpusConfig,
    rng: Rng,
    out: W,
    truth: Option<&'a mut Truth>,
    stats: CorpusStats,
    /// Directories in use.
    dirs: Vec<Dir>,
    next: u64,
    clock: u64,
}

/// Streams the `$MFT` `config` describes to `out`; with `truth`, records what was written.
///
/// # Errors
/// The writer's.
pub fn write_mft<W: Write>(
    config: &CorpusConfig,
    out: W,
    truth: Option<&mut Truth>,
) -> io::Result<CorpusStats> {
    let mut generator = Generator {
        config: *config,
        rng: Rng::new(config.seed),
        out,
        truth,
        stats: CorpusStats::default(),
        dirs: Vec::new(),
        next: 0,
        clock: 0x01d6_0000_0000_0000,
    };
    if let Some(truth) = generator.truth.as_deref_mut() {
        truth.record_size = config.record_size;
    }
    generator.run()?;
    if let Some(truth) = generator.truth.as_deref_mut() {
        truth.records = generator.stats.records;
    }
    generator.out.flush()?;
    Ok(generator.stats)
}

/// The `$MFT` `config` describes, in memory, with its truth.
#[must_use]
#[allow(clippy::expect_used)]
pub fn mft(config: &CorpusConfig) -> (Vec<u8>, Truth) {
    let mut bytes =
        Vec::with_capacity(usize::try_from(config.records).unwrap_or(0) * config.record_size);
    let mut truth = Truth::default();
    write_mft(config, &mut bytes, Some(&mut truth)).expect("writing to memory cannot fail");
    (bytes, truth)
}

impl<W: Write> Generator<'_, W> {
    fn run(&mut self) -> io::Result<()> {
        self.metadata()?;
        let tail = self.config.records * u64::from(self.config.tail) / 1_000_000;
        let body_end = self.config.records.saturating_sub(tail);
        while self.next < body_end {
            self.one(body_end)?;
        }
        while self.next < self.config.records {
            self.empty()?;
        }
        Ok(())
    }

    fn times(&mut self) -> Times {
        self.clock += self.rng.below(20_000_000_000);
        let created = self.clock;
        let modified = created + self.rng.below(5_000_000_000_000);
        Times {
            created,
            modified,
            changed: modified + self.rng.below(1_000_000),
            accessed: modified + self.rng.below(50_000_000_000),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.next += 1;
        self.stats.records += 1;
        Ok(())
    }

    fn builder(&self, sequence: u16) -> RecordBuilder {
        RecordBuilder::new(self.next)
            .size(self.config.record_size)
            .sequence(sequence)
            .update_sequence_number(u16::try_from(self.next % 0xfff0 + 1).unwrap_or(1))
    }

    fn empty(&mut self) -> io::Result<()> {
        if let Some(truth) = self.truth.as_deref_mut() {
            truth.empty.push(self.next);
        }
        self.stats.empty += 1;
        self.write(&vec![0u8; self.config.record_size])
    }

    /// A metadata file named `name` in the root (or in `$Extend`).
    fn system_file(&mut self, name: &str, parent: FileReference, data: u64) -> io::Result<()> {
        let times = self.times();
        let number = self.next;
        let reference = FileReference::new(
            number,
            if number < 16 {
                u16::try_from(number.max(1)).unwrap_or(1)
            } else {
                1
            },
        );
        let raw = self
            .builder(reference.sequence)
            .links(1)
            .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
            .file_name(parent, Namespace::Win32AndDos, name, times, data)
            .non_resident(
                types::DATA,
                "",
                &NonResidentSpec {
                    runs: if data == 0 {
                        Vec::new()
                    } else {
                        vec![(data.div_ceil(4096), Some(1000 + number * 64))]
                    },
                    allocated: data.next_multiple_of(4096),
                    data,
                    initialized: data,
                    ..NonResidentSpec::default()
                },
            )
            .build();
        self.record_file(
            reference,
            true,
            vec![(parent, name.to_owned(), Namespace::Win32AndDos)],
            data,
            Vec::new(),
            None,
            None,
        );
        self.write(&raw)
    }

    fn metadata(&mut self) -> io::Result<()> {
        let root = FileReference::new(records::ROOT, 5);
        // $MFT: its $DATA covers the whole table (one extent, as in a dump).
        let times = self.times();
        let size = self.config.records * self.config.record_size as u64;
        let mft = self
            .builder(1)
            .links(1)
            .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
            .file_name(root, Namespace::Win32AndDos, "$MFT", times, size)
            .non_resident(
                types::DATA,
                "",
                &NonResidentSpec {
                    runs: vec![(size.div_ceil(4096), Some(786_432))],
                    allocated: size.next_multiple_of(4096),
                    data: size,
                    initialized: size,
                    ..NonResidentSpec::default()
                },
            )
            .non_resident(
                types::BITMAP,
                "",
                &NonResidentSpec {
                    runs: vec![(1, Some(786_000))],
                    allocated: 4096,
                    data: self.config.records.div_ceil(8),
                    initialized: self.config.records.div_ceil(8),
                    ..NonResidentSpec::default()
                },
            )
            .build();
        self.record_file(
            FileReference::new(0, 1),
            true,
            vec![(root, "$MFT".to_owned(), Namespace::Win32AndDos)],
            size,
            Vec::new(),
            None,
            None,
        );
        self.write(&mft)?;
        for (name, data) in [("$MFTMirr", 4096u64), ("$LogFile", 64 << 20)] {
            self.system_file(name, root, data)?;
        }
        // $Volume: label and version.
        let times = self.times();
        let mut info = vec![0u8; 12];
        info[8] = 3;
        info[9] = 1;
        let volume = self
            .builder(3)
            .links(1)
            .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
            .file_name(root, Namespace::Win32AndDos, "$Volume", times, 0)
            .resident(types::VOLUME_NAME, "", &utf16("ntfs-ng"))
            .resident(types::VOLUME_INFORMATION, "", &info)
            .resident(types::DATA, "", &[])
            .build();
        self.record_file(
            FileReference::new(3, 3),
            true,
            vec![(root, "$Volume".to_owned(), Namespace::Win32AndDos)],
            0,
            Vec::new(),
            None,
            None,
        );
        self.write(&volume)?;
        self.system_file("$AttrDef", root, 2560)?;
        // The root: a directory in use, named ".", its own parent.
        let times = self.times();
        let raw = self
            .builder(5)
            .directory(true)
            .links(1)
            .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
            .file_name(root, Namespace::Win32AndDos, ".", times, 0)
            .build();
        self.record_directory(root, true, root, ".");
        self.dirs.push(Dir {
            reference: root,
            depth: 0,
        });
        self.write(&raw)?;
        for (name, data) in [
            (
                "$Bitmap",
                self.config.records.div_ceil(8).next_multiple_of(4096),
            ),
            ("$Boot", 8192),
            ("$BadClus", 0),
            ("$Secure", 0),
            ("$UpCase", 131_072),
        ] {
            self.system_file(name, root, data)?;
        }
        // $Extend.
        let times = self.times();
        let extend = FileReference::new(records::EXTEND, 11);
        let raw = self
            .builder(11)
            .directory(true)
            .links(1)
            .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
            .file_name(root, Namespace::Win32AndDos, "$Extend", times, 0)
            .build();
        self.record_directory(extend, true, root, "$Extend");
        self.write(&raw)?;
        // Records 12 to 15: in use, nameless.
        while self.next < records::FIRST_USER {
            let times = self.times();
            let number = self.next;
            let raw = self
                .builder(u16::try_from(number).unwrap_or(1))
                .standard_information(times, 0)
                .build();
            self.record_file(
                FileReference::new(number, u16::try_from(number).unwrap_or(1)),
                true,
                Vec::new(),
                0,
                Vec::new(),
                None,
                None,
            );
            self.write(&raw)?;
        }
        for name in ["$Quota", "$ObjId", "$Reparse", "$RmMetadata"] {
            self.system_file(name, extend, 0)?;
        }
        // $UsnJrnl: a sparse $J and a small $Max.
        let times = self.times();
        let number = self.next;
        let raw = self
            .builder(1)
            .links(1)
            .standard_information(times, file_attributes::HIDDEN | file_attributes::SYSTEM)
            .file_name(extend, Namespace::Win32AndDos, "$UsnJrnl", times, 0)
            .resident(types::DATA, "$Max", &[0u8; 32])
            .non_resident(
                types::DATA,
                "$J",
                &NonResidentSpec {
                    runs: vec![(1 << 20, None), (256, Some(2_000_000))],
                    allocated: ((1 << 20) + 256) * 4096,
                    data: ((1 << 20) + 200) * 4096,
                    initialized: ((1 << 20) + 200) * 4096,
                    flags: crate::attribute::flags::SPARSE,
                    total_allocated: Some(256 * 4096),
                    ..NonResidentSpec::default()
                },
            )
            .build();
        self.record_file(
            FileReference::new(number, 1),
            true,
            vec![(extend, "$UsnJrnl".to_owned(), Namespace::Win32AndDos)],
            0,
            vec![
                ("$Max".to_owned(), 32),
                ("$J".to_owned(), ((1 << 20) + 200) * 4096),
            ],
            None,
            None,
        );
        self.write(&raw)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_file(
        &mut self,
        reference: FileReference,
        in_use: bool,
        names: Vec<(FileReference, String, Namespace)>,
        size: u64,
        streams: Vec<(String, u64)>,
        extension: Option<u64>,
        reparse: Option<u32>,
    ) {
        if in_use {
            self.stats.files += 1;
        } else {
            self.stats.deleted += 1;
        }
        if let Some(truth) = self.truth.as_deref_mut() {
            truth.files.push(TruthFile {
                reference,
                in_use,
                names: names
                    .into_iter()
                    .map(|(parent, name, namespace)| TruthName {
                        parent,
                        name,
                        namespace,
                    })
                    .collect(),
                size,
                streams,
                extension,
                reparse,
            });
        }
    }

    fn record_directory(
        &mut self,
        reference: FileReference,
        in_use: bool,
        parent: FileReference,
        name: &str,
    ) {
        if in_use {
            self.stats.directories += 1;
        } else {
            self.stats.deleted += 1;
        }
        if let Some(truth) = self.truth.as_deref_mut() {
            truth.directories.push(TruthDirectory {
                reference,
                in_use,
                parent,
                name: name.to_owned(),
            });
        }
    }

    /// A random name: `long` asks for 100 to 255 units.
    fn name(&mut self, directory: bool) -> String {
        let mut name = String::new();
        if self.rng.chance(self.config.unicode)
            && let Some(word) = self.rng.pick(&UNICODE)
        {
            name.push_str(word);
        }
        let syllables = 1 + self.rng.below(4);
        for _ in 0..syllables {
            if let Some(syllable) = self.rng.pick(&SYLLABLES) {
                name.push_str(syllable);
            }
        }
        if self.rng.chance(300_000) {
            name.push_str(&self.rng.below(1000).to_string());
        }
        if self.rng.chance(self.config.long_names) {
            let target = 100 + self.rng.below(156) as usize;
            while name.encode_utf16().count() < target {
                name.push(char::from(
                    b'a' + u8::try_from(self.rng.below(26)).unwrap_or(0),
                ));
            }
            while name.encode_utf16().count() > 255 {
                name.pop();
            }
        }
        if !directory
            && let Some(extension) = self.rng.pick(&EXTENSIONS)
            && name.encode_utf16().count() + 1 + extension.len() <= 255
        {
            name.push('.');
            name.push_str(extension);
        }
        name
    }

    /// The 8.3 alias of `name`.
    fn alias(name: &str, serial: u64) -> String {
        let stem: String = name
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(6)
            .collect::<String>()
            .to_ascii_uppercase();
        let stem = if stem.is_empty() {
            "FILE".to_owned()
        } else {
            stem
        };
        let extension: String = name
            .rsplit_once('.')
            .map(|(_, extension)| {
                extension
                    .chars()
                    .filter(char::is_ascii_alphanumeric)
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();
        let suffix = 1 + serial % 9;
        if extension.is_empty() {
            format!("{stem}~{suffix}")
        } else {
            format!("{stem}~{suffix}.{}", extension.to_ascii_uppercase())
        }
    }

    /// Names for a new record in `parent`: one or two `$FILE_NAME`s (a long name and its
    /// alias, or a name valid in both).
    fn names_in(
        &mut self,
        parent: FileReference,
        directory: bool,
    ) -> Vec<(FileReference, String, Namespace)> {
        let name = self.name(directory);
        let ascii_short = name.len() <= 12 && name.is_ascii() && !name.contains(char::is_lowercase);
        if self.rng.chance(self.config.posix) {
            vec![(parent, name, Namespace::Posix)]
        } else if !ascii_short && self.rng.chance(self.config.dos_aliases) {
            let alias = Self::alias(&name, self.next);
            vec![
                (parent, name, Namespace::Win32),
                (parent, alias, Namespace::Dos),
            ]
        } else if ascii_short || self.rng.chance(500_000) {
            vec![(parent, name, Namespace::Win32AndDos)]
        } else {
            vec![(parent, name, Namespace::Win32)]
        }
    }

    /// Keeps a file's names within its record, beside the most its other attributes take: the
    /// last names (hard links, then a DOS alias) go first, then the first name is shortened.
    /// Only names that would not fit change, and no draw is made: every other record is as the
    /// seed makes it.
    fn fit(&self, names: &mut Vec<(FileReference, String, Namespace)>, extension: bool) {
        // A `$FILE_NAME`: its header, its value's 66 bytes, and the name.
        let bytes = |name: &str| (24 + 66 + 2 * name.encode_utf16().count()).next_multiple_of(8);
        // The header with its update sequence, `$STANDARD_INFORMATION`, a reparse point,
        // `$DATA` (non-resident), the end marker, and with an extension the attribute list.
        let others = super::record::first_attribute(self.config.record_size)
            + 96
            + 40
            + 104
            + 8
            + if extension { 24 + 3 * 32 } else { 0 };
        let room = self.config.record_size.saturating_sub(others);
        let total = |names: &[(FileReference, String, Namespace)]| {
            names.iter().map(|(_, name, _)| bytes(name)).sum::<usize>()
        };
        while names.len() > 1 && total(names) > room {
            names.pop();
        }
        if let Some((_, name, _)) = names.first_mut() {
            while bytes(name) > room && name.pop().is_some() {}
            if name.is_empty() {
                name.push('x');
            }
        }
    }

    /// One new record (or two: a file and its extension record).
    fn one(&mut self, body_end: u64) -> io::Result<()> {
        if self.rng.chance(self.config.damaged) {
            return self.damaged();
        }
        let deleted = self.rng.chance(self.config.deleted);
        if self.rng.chance(self.config.directories) {
            return self.directory(deleted);
        }
        let extension =
            !deleted && self.next + 1 < body_end && self.rng.chance(self.config.extensions);
        self.file(deleted, extension)
    }

    fn damaged(&mut self) -> io::Result<()> {
        let times = self.times();
        let mut raw = self.builder(1).standard_information(times, 0).build();
        if self.rng.chance(500_000) {
            // `BAAD`: a record chkdsk found torn.
            super::record::put(&mut raw, 0, b"BAAD");
        } else if let Some(byte) = raw.get_mut(1022.min(self.config.record_size - 2)) {
            // Torn: the second stride's tail is not the update sequence number.
            *byte ^= 0x5a;
        }
        if let Some(truth) = self.truth.as_deref_mut() {
            truth.damaged.push(self.next);
        }
        self.stats.damaged += 1;
        self.write(&raw)
    }

    fn pick_parent(&mut self) -> Dir {
        // Mostly recent directories (locality, as Windows allocates), sometimes anywhere.
        let len = self.dirs.len() as u64;
        let at = if self.rng.chance(700_000) {
            len.saturating_sub(1 + self.rng.below(len.min(64)))
        } else {
            self.rng.below(len)
        };
        self.dirs
            .get(usize::try_from(at).unwrap_or(0))
            .copied()
            .unwrap_or(Dir {
                reference: FileReference::new(records::ROOT, 5),
                depth: 0,
            })
    }

    fn directory(&mut self, deleted: bool) -> io::Result<()> {
        let mut parent = self.pick_parent();
        while parent.depth >= self.config.max_depth {
            parent = self.pick_parent();
        }
        let times = self.times();
        let sequence = 1 + u16::try_from(self.rng.below(12)).unwrap_or(0);
        // Freed after its name was written: the record's sequence is one more.
        let now = if deleted {
            crate::record::next_sequence(sequence)
        } else {
            sequence
        };
        let reference = FileReference::new(self.next, now);
        let names = self.names_in(parent.reference, true);
        let mut builder = self
            .builder(now)
            .directory(true)
            .in_use(!deleted)
            .links(1)
            .standard_information(times, file_attributes::DIRECTORY);
        for (parent, name, namespace) in &names {
            builder = builder.file_name(*parent, *namespace, name, times, 0);
        }
        let raw = builder.build();
        let primary = names
            .first()
            .map(|(_, name, _)| name.clone())
            .unwrap_or_default();
        self.record_directory(reference, !deleted, parent.reference, &primary);
        if !deleted {
            self.dirs.push(Dir {
                reference,
                depth: parent.depth + 1,
            });
        }
        self.write(&raw)
    }

    fn file(&mut self, deleted: bool, extension: bool) -> io::Result<()> {
        let parent = self.pick_parent();
        let times = self.times();
        let sequence = 1 + u16::try_from(self.rng.below(30)).unwrap_or(0);
        let now = if deleted {
            crate::record::next_sequence(sequence)
        } else {
            sequence
        };
        let reference = FileReference::new(self.next, now);
        // A deleted name may point at a directory reused since: one sequence behind.
        let parent_reference = if deleted
            && parent.reference.record != records::ROOT
            && self.rng.chance(self.config.orphans)
        {
            FileReference::new(
                parent.reference.record,
                parent.reference.sequence.wrapping_sub(1).max(1),
            )
        } else {
            parent.reference
        };
        let mut names = self.names_in(parent_reference, false);
        if !deleted && self.rng.chance(self.config.hard_links) {
            for _ in 0..1 + self.rng.below(2) {
                let other = self.pick_parent();
                let name = self.name(false);
                names.push((other.reference, name, Namespace::Win32AndDos));
            }
        }
        self.fit(&mut names, extension);
        let size = match self.rng.below(10) {
            0 => 0,
            1..=3 => self.rng.below(700),
            4..=7 => 700 + self.rng.below(200_000),
            8 => self.rng.below(50 << 20),
            _ => self.rng.below(4 << 30),
        };
        let links = u16::try_from(
            names
                .iter()
                .filter(|(.., namespace)| !namespace.is_dos_only())
                .count(),
        )
        .unwrap_or(1);
        let mut builder = self
            .builder(now)
            .in_use(!deleted)
            .links(links)
            .standard_information(times, file_attributes::ARCHIVE);
        let list_at = builder.next_instance();
        if extension {
            // Room for the attribute list, written once the extension's place is known.
            builder = builder.attribute(Vec::new());
        }
        for (parent, name, namespace) in &names {
            builder = builder.file_name(*parent, *namespace, name, times, size);
        }
        let reparse = if self.rng.chance(self.config.reparse) {
            let tag = *self
                .rng
                .pick(&[tags::SYMLINK, tags::MOUNT_POINT, tags::WOF, 0x9000_601A])
                .unwrap_or(&tags::WOF);
            let mut value = vec![0u8; 8];
            super::record::put(&mut value, 0, &tag.to_le_bytes());
            builder = builder.resident(types::REPARSE_POINT, "", &value);
            Some(tag)
        } else {
            None
        };
        let clusters = size.div_ceil(4096);
        let spec = |lowest_vcn: u64, runs: Vec<(u64, Option<u64>)>, first: bool| NonResidentSpec {
            lowest_vcn,
            runs,
            allocated: if first { clusters * 4096 } else { 0 },
            data: if first { size } else { 0 },
            initialized: if first { size } else { 0 },
            ..NonResidentSpec::default()
        };
        let lcn = 3_000_000 + self.rng.below(200_000_000);
        if extension {
            // $DATA in two extents: the first here, the rest in the next record.
            let clusters = clusters.max(2);
            let first = clusters / 2;
            let ext_number = self.next + 1;
            let ext_reference = FileReference::new(ext_number, 1);
            let data_instance = builder.next_instance();
            builder = builder.non_resident(
                types::DATA,
                "",
                &NonResidentSpec {
                    allocated: clusters * 4096,
                    data: clusters * 4096,
                    initialized: clusters * 4096,
                    ..spec(0, vec![(first, Some(lcn))], true)
                },
            );
            let extension_record = RecordBuilder::new(ext_number)
                .size(self.config.record_size)
                .base(reference)
                .update_sequence_number(0x0b0b)
                .non_resident(
                    types::DATA,
                    "",
                    &spec(
                        first,
                        vec![(clusters - first, Some(lcn + first + 7))],
                        false,
                    ),
                )
                .build();
            let mut list = Vec::new();
            list.extend(attribute_list_entry(
                types::STANDARD_INFORMATION,
                &[],
                0,
                reference,
                0,
            ));
            list.extend(attribute_list_entry(
                types::DATA,
                &[],
                0,
                reference,
                data_instance,
            ));
            list.extend(attribute_list_entry(
                types::DATA,
                &[],
                first,
                ext_reference,
                0,
            ));
            builder = builder.replace(
                list_at,
                super::record::resident(types::ATTRIBUTE_LIST, list_at, &[], &list, 0),
            );
            let raw = builder.build();
            self.record_file(
                reference,
                true,
                names,
                clusters * 4096,
                Vec::new(),
                Some(ext_number),
                reparse,
            );
            self.write(&raw)?;
            if let Some(truth) = self.truth.as_deref_mut() {
                truth.extensions.push(ext_number);
            }
            self.stats.extensions += 1;
            return self.write(&extension_record);
        }
        if size < 700 {
            builder = builder.resident(
                types::DATA,
                "",
                &vec![0x61u8; usize::try_from(size).unwrap_or(0)],
            );
        } else {
            let runs = if clusters > 64 && self.rng.chance(300_000) {
                let half = clusters / 2;
                vec![
                    (half, Some(lcn)),
                    (
                        clusters - half,
                        Some(lcn + half + 100 + self.rng.below(10_000)),
                    ),
                ]
            } else {
                vec![(clusters, Some(lcn))]
            };
            builder = builder.non_resident(types::DATA, "", &spec(0, runs, true));
        }
        let mut streams = Vec::new();
        if self.rng.chance(self.config.ads) {
            let name = if self.rng.chance(500_000) {
                "Zone.Identifier"
            } else {
                "meta"
            };
            let value = b"[ZoneTransfer]\r\nZoneId=3\r\n";
            builder = builder.resident(types::DATA, name, value);
            streams.push((name.to_owned(), value.len() as u64));
        }
        let raw = match builder.try_build() {
            Some(raw) => raw,
            // Too many names for one record: keep the first.
            None => {
                names.truncate(1);
                let mut builder = self
                    .builder(now)
                    .in_use(!deleted)
                    .links(1)
                    .standard_information(times, file_attributes::ARCHIVE);
                for (parent, name, namespace) in &names {
                    builder = builder.file_name(*parent, *namespace, name, times, size);
                }
                streams.clear();
                builder.build()
            }
        };
        self.record_file(reference, !deleted, names, size, streams, None, reparse);
        self.write(&raw)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use super::*;
    use crate::{
        mft::Mft,
        scan::{Entry, Scan},
    };

    #[test]
    fn corpora_are_deterministic_and_parse() {
        let config = CorpusConfig::realistic(5_000);
        let (bytes, truth) = mft(&config);
        let (again, _) = mft(&config);
        assert_eq!(bytes, again);
        assert_eq!(bytes.len(), 5_000 * 1024);
        assert_eq!(truth.records, 5_000);
        let mft = Mft::from_file(bytes.as_slice()).unwrap();
        assert_eq!(mft.records(), 5_000);
        let mut records = 0;
        let mut damaged = 0;
        let mut empty = 0;
        Scan::new(&mft)
            .run(&Counter, |_, (r, d, e)| {
                records += r;
                damaged += d;
                empty += e;
                std::ops::ControlFlow::Continue(())
            })
            .unwrap();
        assert_eq!(damaged, truth.damaged.len());
        assert_eq!(empty, truth.empty.len());
        assert_eq!(records + damaged + empty, 5_000);
        assert!(truth.directories.len() > 500, "{}", truth.directories.len());
        assert!(truth.extensions.len() > 20);
    }

    struct Counter;

    impl crate::scan::Projection for Counter {
        type Output = (usize, usize, usize);
        type Scratch = ();

        fn scratch(&self) {}

        fn begin(&self, (): &mut (), _: crate::scan::Chunk) -> Self::Output {
            (0, 0, 0)
        }

        fn entry(&self, out: &mut Self::Output, (): &mut (), _: u64, entry: Entry<'_>) {
            match entry {
                Entry::Record(record) => {
                    assert!(record.attributes().all(|attribute| attribute.is_ok()));
                    out.0 += 1;
                }
                Entry::Damaged { .. } => out.1 += 1,
                Entry::Empty => out.2 += 1,
                other => panic!("{other:?}"),
            }
        }
    }
}
