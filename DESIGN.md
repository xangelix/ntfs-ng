# Design

This document explains how `ntfs-ng` is built and why. It is for contributors and for callers
who want to know what a scan costs.

## Principles

1. **Mechanism, not policy.** The crate reads structures and resolves paths; it never decides
   what a record means for a caller. Which fields to read, how strictly to judge a record, which
   parent links count, whether deleted directories hold names: all of these are the caller's,
   expressed as a projection or a policy.
2. **Pay only for what you read.** The scan is generic over the projection, so each caller gets
   its own monomorphized loop. A disk-usage projection that reads five fields of live records
   compiles to code that never touches the rest.
3. **Zero-copy, check once.** Views borrow the bytes they read. A structure's length is checked
   once; its fields are then read from fixed offsets without failure paths.
4. **Never invent, never lose.** Update sequences are applied in place and undone exactly; holes
   are reported, never zero-filled behind the caller's back; names are kept as stored (UTF-16),
   decoded lossily only when asked, exactly (WTF-8) when needed.

## The scan

`$MFT` is cut into chunks of whole records (4 MiB by default). Worker threads claim chunks in
turn from an atomic counter, each with its own aligned buffer reused for every chunk:

1. **Read.** The chunk's byte range in `$MFT` maps to pieces on the source through `$MFT`'s
   extent map. Each piece is one positional read (`pread`; on Windows one of several pooled
   handles, since a synchronous handle serializes reads), aligned when the source is a raw
   device, skipped when the source reports a hole. A piece that fails is retried record by
   record, so a bad sector costs its own records only.
2. **Classify.** Each slot is a record (its update sequence applied in place, its number checked
   against its place), zeros, damage (with the bytes as stored), unreadable, or unmapped.
3. **Project.** The caller's projection turns each slot into its output for the chunk.
4. **Deliver.** Outputs reach the caller's sink on the calling thread, in record order (a reorder
   buffer, with workers held within a window of the next chunk to deliver, which bounds memory)
   or as they finish.

The engine has no global locks on the hot path, no per-record allocation of its own, and no
dependency: threads come from `std::thread::scope`.

**Where outputs are freed.** An output is built on a worker and freed wherever the sink drops
it, usually on the calling thread. glibc's allocator locks the arena of the thread that
allocated the memory to free it, and a worker allocating its next output keeps that lock busy: a
sink handed many small allocations (an object per record) pays a contended lock per free, and
more workers make it slower. With `Scan::wait_for_sink`, a worker waits for the sink to be done
with its output before it reads another chunk, so those frees meet an idle arena: for outputs of
about five allocations per record, over 1.85 M records, it cut delivery from 1.0 s to 0.24 s, as
fast as with jemalloc or mimalloc. Outputs kept flat (columns and one buffer of names, as `files`,
`usage` and `index` keep them) do not need it, and it costs overlap with few threads, so it is off
by default.

## Paths

A name's `$FILE_NAME` records its parent by record number *and* sequence number. The
`DirectoryIndex` keeps every directory record in dense arrays indexed by record number (4 bytes
per record, plus the directories themselves), and resolves every directory's chain to the root
exactly once, iteratively, detecting cycles and depth limits. A name's directory is then two
array lookups and a sequence check (`locate`), and a path is written by walking parent indices
and copying names from a UTF-8 arena decoded once.

What counts as a link is a `PathPolicy`:

| Policy | Parent sequence | Deleted directories | Root |
|---|---|---|---|
| `LIVE` | exact | not parents | record 5 |
| `STRICT` | exact | not parents | record 5, in use, a directory, named `.`, its own parent |
| `RECOVERY` | exact, or for a deleted parent the next (freed since) | parents | record 5 |

Names whose parent does not count are orphans; directories whose chain breaks are `Orphaned`,
`Detached` (an ancestor broke), `Cycle`, `TooDeep` or `Unnamed`.

## Files

A file whose record overflowed keeps attributes in extension records: names, and the first
extent of its `$DATA`, may live there, the base record holding only an `$ATTRIBUTE_LIST`. The
`FileTable` folds them in: an extension record in use contributes to the base it names, when
that base is in use at the sequence it names; its names become links (DOS aliases marked across
base and extension), and its `$DATA` gives the file's size. A freed extension record still names
its base but no longer holds what the file has, and a deleted file's extension records may belong
to another file by now: a deleted file reports what its own record holds. The directory index
takes directories' names from extension records by the same rule.

The table keeps the scan's batches as they are. Files and links get dense ids mapped to their
batch by small arrays, so nothing is copied together after the scan; links from extension
records live beside the batches. Building it costs two array fills and a sort of the extension
links.

## Disk usage

Showing sizes and times needs less of each file than the file table keeps: its size, allocated size,
two times and a few flags, for each live name. `Usage` keeps exactly that, one 48-byte entry per
name with its file's values copied in (hard links are rare), names in each chunk's arena. Only a
base record with an attribute list can be completed from extension records, so only those are
remembered for the fold; DOS aliases in the same record are dropped as they are read, and those
across records marked in the fold. On a 1.85 M-record volume this holds a third of what the file
table does, and builds faster for it.

## Volumes and streams

`Mft::from_volume` maps `$MFT` through record 0's runs, then through the extents its attribute
list names in extension records, pass by pass (an extension record may lie in a part of `$MFT`
only an earlier pass maps), following an entry only into a record that is in use, has the
entry's sequence and names record 0 as its base.

`StreamReader` reads a non-resident stream through its runs from every extent, handing the
caller data and typed holes: sparse, uninitialized (past the initialized size), unmapped,
past the volume, unreadable, undecodable (LZNT1), reallocated (clusters `$Bitmap` shows in use,
for deleted streams), or over the budget. Compressed streams are decoded one compression unit at
a time; whole units inside one run are handled together (one hole for a sparse or unmapped run,
one read for a run on the volume, stored as it is), so the work is bounded by the runs, not by
the sizes a header declares.

## Leniency, and where it stops

Where the format is ambiguous, ntfs-ng reads as ntfs-3g and Linux's ntfs3 do:

- Mapping pairs end at a zero byte or at the end of their attribute, whichever comes first.
- A record that states 0 as its number is not checked against its place: formatters leave the
  number unset in records they reserve (`mkntfs` in records 16 to 23). Any other number must be
  the record's own.
- A record whose allocated size is not the volume's record size is refused, `$MFT`'s record 0
  included (the volume then does not open), and so is one whose first attribute lies past the
  bytes it uses.

## Panics

None is expected: the parsers index nothing unchecked and are fuzzed. Should one happen anyway
in a projection or a scan's sink, the scan stops its other workers and the panic goes on to the
caller, never a hang.

## Testing

- Unit tests next to every parser, with hostile cases.
- `testing::corpus` generates realistic `$MFT`s of any size, deterministically, with the ground
  truth of every directory, file, name and size; tests check the file table and every path
  policy against it.
- `testing::image` builds volumes with content of every kind; `tests/volume.rs` reads them back.
- On Windows runners, `tests/live_windows.rs` reads real volumes raw, through the cache and around
  it, and checks every name, size, flag and stream against what Windows reports: a volume
  formatted with 1 KiB and with 4 KiB records and filled by `scripts/windows-live-fixtures.ps1`
  (compressed, sparse, uninitialized and fragmented streams, alternate streams, 300 hard links of
  one file, reparse points, deleted files, a path past `MAX_PATH`), a shadow copy of it, and the
  system volume (`.github/workflows/windows-live.yml`).
- `fuzz/` holds a target per surface: records with every value, values alone, LZNT1 (with a
  round trip), pages and update sequences, a full scan into the file table and index, and
  volumes with streams and `$Bitmap`.
