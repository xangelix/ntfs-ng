# Changelog

## Unreleased

- `runs::Pairs`: mapping pairs read and checked without counting VCNs, each a `runs::Pair`
  (length, LCN and offset), for callers with rules of their own about the runs' total.
- `terminated()` on `Pairs` and `MappingPairs`: whether the zero byte ended the pairs, rather
  than the end of their attribute.
- Every attribute header field: `Attribute::resident_header` (`ResidentHeader`: the value's
  length and offset, the indexed byte and the reserved one), and `NonResident`'s
  `mapping_pairs_offset` and `reserved`.
- `MappingPairs` checks a run's VCN range after the rest of its pair: a pair also invalid on the
  volume fails with that error, not `VcnOverflow`.

## 0.1.0 (unreleased)

First release.

- Structures: `FILE` records with in-place, exactly reversible update sequences; attributes;
  mapping pairs; `$STANDARD_INFORMATION`, `$FILE_NAME`, `$ATTRIBUTE_LIST`, `$OBJECT_ID`,
  `$INDEX_ROOT`, `INDX` blocks and index entries, `$REPARSE_POINT` (with symbolic link and
  junction targets), `$VOLUME_INFORMATION`; UTF-16 names (lossy, strict and WTF-8 decoding);
  update sequences of `INDX`, `RSTR` and `RCRD` pages; the boot sector; LZNT1; FILETIMEs.
  Builds without `std`.
- Sources: positional reads with holes and alignment; files (with pooled handles on Windows)
  and raw devices.
- `$MFT`: standalone dumps and volumes, mapped through every extent, those in extension records
  included.
- The parallel scan, ordered or unordered, with caller-defined projections; workers can wait for
  the sink, so that outputs of many allocations are freed without contending with their workers.
- The directory index with `LIVE`, `STRICT` and `RECOVERY` path policies.
- The file table: files, names and sizes with extension records folded in; the disk-usage
  table, a third of its memory, for showing sizes and times.
- Volumes: records, attribute lists, owned extension records, streams with typed holes,
  compressed streams, `$Bitmap`.
- `testing`: record builders, a realistic `$MFT` generator with ground truth, volume images.
