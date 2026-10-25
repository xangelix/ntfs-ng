//! Mapping pairs: where a non-resident stream's clusters are on the volume.
//!
//! Each pair starts with a header byte whose low four bits give the size of the run length
//! (1–8 bytes) and whose high four bits give the size of the LCN delta (0–8 bytes); both fields
//! follow, little-endian, the delta signed and relative to the previous run's LCN. A delta of
//! size 0 is a sparse run: no clusters are allocated, and the next delta stays relative to the
//! last allocated run. A zero header byte ends the pairs.
//!
//! An attribute whose mapping pairs start at its very end has none: an empty stream.
//!
//! [`MappingPairs`] reads the runs with the VCN each starts at; [`Pairs`] reads the same pairs
//! without counting VCNs, for a caller with rules of its own about the runs' total.

use crate::{
    FormatError,
    bytes::{signed, take, unsigned},
};

/// One run of clusters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    /// The run's first VCN within its stream.
    pub vcn: u64,
    /// Clusters in the run, never 0.
    pub length: u64,
    /// The first cluster on the volume, or `None` for a sparse run.
    pub lcn: Option<u64>,
}

/// One mapping pair as stored: a run's length and where it is, without the VCN it starts at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair {
    /// Clusters in the run, never 0.
    pub length: u64,
    /// The first cluster on the volume, or `None` for a sparse run.
    pub lcn: Option<u64>,
    /// The pair's offset in its record.
    pub offset: usize,
}

/// The pairs of a mapping pairs array, in order: up to the zero byte that ends them, or the end
/// of their attribute (as ntfs-3g and Linux's ntfs3 read pairs that fill it). The first invalid
/// pair ends the iteration with its error.
///
/// Each pair is read and checked as [`MappingPairs`] reads it, but no VCNs are counted: a
/// caller that keeps its own count reads every pair, whatever VCN the runs reach. Once the
/// pairs are read, [`Self::terminated`] tells which end they had.
#[derive(Clone, Debug)]
pub struct Pairs<'a> {
    data: &'a [u8],
    /// The array's offset in its record, for errors.
    base: usize,
    at: usize,
    lcn: i64,
    done: bool,
    terminated: bool,
}

impl<'a> Pairs<'a> {
    /// The pairs in `data`, which starts at offset `base` of its record and runs to the end of
    /// its attribute.
    #[must_use]
    pub const fn new(data: &'a [u8], base: usize) -> Self {
        Self {
            data,
            base,
            at: 0,
            lcn: 0,
            done: data.is_empty(),
            terminated: false,
        }
    }

    /// Whether the zero byte that ends the pairs was read: `false` until it is, and for pairs
    /// that ran to the end of their attribute (or hold none).
    #[must_use]
    pub const fn terminated(&self) -> bool {
        self.terminated
    }

    #[inline]
    fn read(&mut self) -> Result<Option<Pair>, FormatError> {
        let at = self.base + self.at;
        // The attribute's end ends the pairs as the zero byte does.
        let Some(&header) = self.data.get(self.at) else {
            return Ok(None);
        };
        self.at += 1;
        if header == 0 {
            self.terminated = true;
            return Ok(None);
        }
        let length_size = usize::from(header & 0x0f);
        let offset_size = usize::from(header >> 4);
        if !(1..=8).contains(&length_size) || offset_size > 8 {
            return Err(FormatError::MappingPairWidths { at, header });
        }
        let length = unsigned(take(
            self.data,
            self.at,
            length_size,
            "mapping pair length",
        )?);
        self.at += length_size;
        if length == 0 {
            return Err(FormatError::ZeroLengthRun { at });
        }
        if offset_size == 0 {
            return Ok(Some(Pair {
                length,
                lcn: None,
                offset: at,
            }));
        }
        let delta = signed(take(
            self.data,
            self.at,
            offset_size,
            "mapping pair offset",
        )?);
        self.lcn = self
            .lcn
            .checked_add(delta)
            .ok_or(FormatError::LcnOverflow { at })?;
        let lcn = u64::try_from(self.lcn)
            .ok()
            .filter(|&lcn| {
                lcn.checked_add(length)
                    .is_some_and(|end| end <= i64::MAX as u64)
            })
            .ok_or(FormatError::LcnRange {
                at,
                lcn: self.lcn,
                length,
            })?;
        self.at += offset_size;
        Ok(Some(Pair {
            length,
            lcn: Some(lcn),
            offset: at,
        }))
    }
}

impl Iterator for Pairs<'_> {
    type Item = Result<Pair, FormatError>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let pair = self.read();
        if !matches!(pair, Ok(Some(_))) {
            self.done = true;
        }
        pair.transpose()
    }
}

impl core::iter::FusedIterator for Pairs<'_> {}

/// The runs of a mapping pairs array, in order, each with the VCN it starts at: the pairs
/// ([`Pairs`]), and a run past the largest VCN (`i64::MAX`) as an error that ends them.
#[derive(Clone, Debug)]
pub struct MappingPairs<'a> {
    pairs: Pairs<'a>,
    vcn: u64,
}

impl<'a> MappingPairs<'a> {
    /// The pairs in `data`, which starts at offset `base` of its record and runs to the end of
    /// its attribute, describing a stream from `first_vcn`.
    #[must_use]
    pub const fn new(data: &'a [u8], base: usize, first_vcn: u64) -> Self {
        Self {
            pairs: Pairs::new(data, base),
            vcn: first_vcn,
        }
    }

    /// The VCN after the last run read so far.
    #[must_use]
    pub const fn next_vcn(&self) -> u64 {
        self.vcn
    }

    /// Whether the zero byte that ends the pairs was read ([`Pairs::terminated`]).
    #[must_use]
    pub const fn terminated(&self) -> bool {
        self.pairs.terminated()
    }
}

impl Iterator for MappingPairs<'_> {
    type Item = Result<Run, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        let pair = match self.pairs.next()? {
            Ok(pair) => pair,
            Err(error) => return Some(Err(error)),
        };
        let vcn = self.vcn;
        let Some(end) = vcn
            .checked_add(pair.length)
            .filter(|end| *end <= i64::MAX as u64)
        else {
            self.pairs.done = true;
            return Some(Err(FormatError::VcnOverflow { at: pair.offset }));
        };
        self.vcn = end;
        Some(Ok(Run {
            vcn,
            length: pair.length,
            lcn: pair.lcn,
        }))
    }
}

impl core::iter::FusedIterator for MappingPairs<'_> {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{vec, vec::Vec};

    use super::*;

    fn runs(data: &[u8]) -> Result<Vec<Run>, FormatError> {
        MappingPairs::new(data, 0, 0).collect()
    }

    #[test]
    fn sparse_runs_keep_the_previous_lcn() {
        let decoded = runs(&[0x11, 2, 100, 0x01, 3, 0x11, 1, 0xf6, 0]).unwrap();
        assert_eq!(
            decoded,
            [
                Run {
                    vcn: 0,
                    length: 2,
                    lcn: Some(100)
                },
                Run {
                    vcn: 2,
                    length: 3,
                    lcn: None
                },
                Run {
                    vcn: 5,
                    length: 1,
                    lcn: Some(90)
                },
            ]
        );
        let mut wide = vec![0x88];
        wide.extend(5u64.to_le_bytes());
        wide.extend(7i64.to_le_bytes());
        wide.push(0);
        let mut pairs = MappingPairs::new(&wide, 0, 10);
        assert_eq!(
            pairs.next().unwrap().unwrap(),
            Run {
                vcn: 10,
                length: 5,
                lcn: Some(7)
            }
        );
        assert!(pairs.next().is_none());
        assert_eq!(pairs.next_vcn(), 15);
    }

    #[test]
    fn empty_pairs_hold_no_runs_and_invalid_pairs_fail() {
        assert_eq!(runs(&[]).unwrap(), []);
        assert_eq!(runs(&[0]).unwrap(), []);
        // Pairs that fill their attribute end with it, without a zero byte.
        assert_eq!(
            runs(&[0x11, 1, 1]).unwrap(),
            [Run {
                vcn: 0,
                length: 1,
                lcn: Some(1)
            }]
        );
        type Check = fn(&FormatError) -> bool;
        let cases: [(&[u8], Check); 5] = [
            (&[0x10, 1, 0], |e| {
                matches!(e, FormatError::MappingPairWidths { .. })
            }),
            (&[0x91, 1, 0], |e| {
                matches!(e, FormatError::MappingPairWidths { .. })
            }),
            (&[0x11, 0, 1, 0], |e| {
                matches!(e, FormatError::ZeroLengthRun { .. })
            }),
            (&[0x11, 1, 0xff, 0], |e| {
                matches!(e, FormatError::LcnRange { .. })
            }),
            (&[0x41, 1, 1], |e| {
                matches!(e, FormatError::Truncated { .. })
            }),
        ];
        for (data, expected) in cases {
            let error = runs(data).unwrap_err();
            assert!(expected(&error), "{data:?}: {error:?}");
        }
        let mut overflow = vec![0x81, 1];
        overflow.extend((i64::MAX - 1).to_le_bytes());
        overflow.extend([0x81, 1]);
        overflow.extend(i64::MAX.to_le_bytes());
        overflow.push(0);
        assert!(matches!(
            runs(&overflow).unwrap_err(),
            FormatError::LcnOverflow { .. }
        ));
        let mut long = vec![0x08];
        long.extend(u64::MAX.to_le_bytes());
        long.push(0);
        assert!(matches!(
            runs(&long).unwrap_err(),
            FormatError::VcnOverflow { at: 0 }
        ));
    }

    #[test]
    fn pairs_count_no_vcns_and_tell_how_they_ended() {
        // Two sparse runs past the largest VCN: every pair is read, then the zero byte.
        let mut huge = vec![0x08];
        huge.extend((1u64 << 62).to_le_bytes());
        huge.extend([0x08]);
        huge.extend((3u64 << 62).to_le_bytes());
        huge.extend([0x11, 1, 5, 0]);
        let mut pairs = Pairs::new(&huge, 100);
        let read: Vec<_> = pairs.by_ref().collect::<Result<_, _>>().unwrap();
        assert_eq!(
            read,
            [
                Pair {
                    length: 1 << 62,
                    lcn: None,
                    offset: 100
                },
                Pair {
                    length: 3 << 62,
                    lcn: None,
                    offset: 109
                },
                Pair {
                    length: 1,
                    lcn: Some(5),
                    offset: 118
                },
            ]
        );
        assert!(pairs.terminated());
        // The runs stop at the second, and nothing is read after it.
        let mut with_vcns = MappingPairs::new(&huge, 100, 0);
        assert!(with_vcns.next().unwrap().is_ok());
        assert!(matches!(
            with_vcns.next().unwrap().unwrap_err(),
            FormatError::VcnOverflow { at: 109 }
        ));
        assert!(with_vcns.next().is_none());
        assert!(!with_vcns.terminated());

        // Pairs that fill their attribute, and none at all, are not terminated.
        let mut filled = Pairs::new(&[0x11, 1, 1], 0);
        assert_eq!(filled.by_ref().count(), 1);
        assert!(!filled.terminated());
        let mut empty = Pairs::new(&[], 0);
        assert!(empty.next().is_none());
        assert!(!empty.terminated());
        let mut zero = MappingPairs::new(&[0], 0, 0);
        assert!(zero.next().is_none());
        assert!(zero.terminated());

        // A run out of range on the volume fails as such, whatever VCN it would reach.
        let mut far = vec![0x88];
        far.extend(u64::MAX.to_le_bytes());
        far.extend(1i64.to_le_bytes());
        far.push(0);
        assert!(matches!(
            runs(&far).unwrap_err(),
            FormatError::LcnRange { .. }
        ));
    }
}
