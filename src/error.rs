//! Why a structure was refused, and why an operation on a source failed.
//!
//! [`FormatError`] is `Copy` and allocation-free: offsets in record-level errors count from the
//! start of the `FILE` record, offsets in value-level errors from the start of the value.
//! [`Error`] (with `std`) adds I/O failures and limits.

use core::fmt;

/// A refused NTFS structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum FormatError {
    /// A multi-sector structure that is not a whole number of 512-byte sectors.
    RecordSize {
        /// Its length.
        len: usize,
    },
    /// A structure that does not start with its signature (`FILE`, `INDX`, …).
    Signature {
        /// The four bytes found.
        found: [u8; 4],
    },
    /// The record's allocated size is not its length, or its used size leaves no room for the
    /// `$END` marker after the first attribute.
    Sizes {
        /// Bytes allocated, as declared.
        allocated: u32,
        /// Bytes in use, as declared.
        used: u32,
        /// Offset of the first attribute.
        first_attribute: u16,
        /// The record's length.
        len: usize,
    },
    /// The update sequence array is misplaced, or does not hold one entry per 512-byte sector
    /// plus the update sequence number.
    UpdateSequence {
        /// Its offset.
        offset: u16,
        /// Its entries.
        count: u16,
        /// The structure's length.
        len: usize,
    },
    /// The first attribute is misaligned, or starts inside the header or the update sequence
    /// array.
    FirstAttribute {
        /// Its offset.
        offset: u16,
        /// Where the header and array end.
        header_end: usize,
    },
    /// A sector whose last two bytes are not the update sequence number: it was written apart
    /// from the rest of the structure ("torn").
    TornSector {
        /// The sector, from 0.
        sector: usize,
    },
    /// A record that states another number than its place in `$MFT` (NTFS 3.1 records carry
    /// their own number).
    RecordNumber {
        /// The record's place.
        expected: u64,
        /// The number it states.
        found: u32,
    },
    /// `need` bytes of `what` at offset `at` run past the `len` bytes that hold it.
    Truncated {
        /// The structure.
        what: &'static str,
        /// Its offset.
        at: usize,
        /// Bytes it needs.
        need: usize,
        /// Bytes available.
        len: usize,
    },
    /// `need` bytes of `what` (a non-resident header, a name, a value) from `offset` of the
    /// attribute at `at` run past its `length` bytes.
    AttributeOverrun {
        /// The attribute's offset in its record.
        at: usize,
        /// The part.
        what: &'static str,
        /// The part's offset in the attribute.
        offset: usize,
        /// Bytes it needs.
        need: usize,
        /// The attribute's length.
        length: usize,
    },
    /// An attribute shorter than its 24-byte common header, or not a multiple of 8 bytes long.
    AttributeLength {
        /// Its offset in its record.
        at: usize,
        /// Its declared length.
        length: u32,
    },
    /// An attribute that is neither resident (0) nor non-resident (1).
    AttributeForm {
        /// Its offset in its record.
        at: usize,
        /// The form byte.
        form: u8,
    },
    /// An operation that needs the other form (a resident value of a non-resident attribute).
    WrongForm {
        /// The attribute's offset in its record.
        at: usize,
    },
    /// An attribute's name starts inside its header, or at an odd offset.
    NameOffset {
        /// The attribute's offset in its record.
        at: usize,
        /// The name's offset.
        offset: u16,
        /// The header's size.
        header: usize,
    },
    /// A resident value that starts inside the attribute's header or name.
    ValueOffset {
        /// The attribute's offset in its record.
        at: usize,
        /// The value's offset.
        offset: u16,
        /// Where the header and name end.
        after: usize,
    },
    /// Mapping pairs that start inside the attribute's header or name, or past its end.
    MappingPairsOffset {
        /// The attribute's offset in its record.
        at: usize,
        /// The pairs' offset.
        offset: u16,
        /// Where the header and name end.
        after: usize,
        /// The attribute's length.
        length: u32,
    },
    /// A mapping pair whose header declares a length field of 0 or more than 8 bytes, or an
    /// offset field of more than 8.
    MappingPairWidths {
        /// The pair's offset.
        at: usize,
        /// Its header byte.
        header: u8,
    },
    /// A run of zero clusters.
    ZeroLengthRun {
        /// The pair's offset.
        at: usize,
    },
    /// A run's LCN delta overflows a 64-bit LCN.
    LcnOverflow {
        /// The pair's offset.
        at: usize,
    },
    /// A run that starts before cluster 0 or ends past the largest LCN.
    LcnRange {
        /// The pair's offset.
        at: usize,
        /// The run's first cluster.
        lcn: i64,
        /// Its clusters.
        length: u64,
    },
    /// Runs that overflow a 64-bit VCN.
    VcnOverflow {
        /// The pair's offset.
        at: usize,
    },
    /// A `$STANDARD_INFORMATION` value of neither the 48-byte (NTFS 1.2) nor the 72-byte (NTFS
    /// 3.0 and later) layout.
    StandardInformationSize {
        /// Its length.
        len: usize,
    },
    /// A `$FILE_NAME` namespace other than POSIX, Win32, DOS or both.
    Namespace {
        /// The namespace byte.
        value: u8,
    },
    /// An attribute-list entry shorter than its 26-byte header.
    AttributeListEntryLength {
        /// Its offset in the list.
        at: usize,
        /// Its declared length.
        length: u16,
    },
    /// An attribute-list entry whose name starts inside its header.
    AttributeListNameOffset {
        /// Its offset in the list.
        at: usize,
        /// The name's offset.
        offset: u8,
    },
    /// An index entry shorter than its 16-byte header.
    IndexEntryLength {
        /// Its offset in its node.
        at: usize,
        /// Its declared length.
        length: u16,
    },
    /// An index node whose entries start before its header ends or past its length.
    IndexNode {
        /// Where the entries start, from the node header.
        entries_offset: u32,
        /// The node's length, from the node header.
        index_length: u32,
    },
    /// A boot sector field out of range.
    BootSector {
        /// The field.
        field: &'static str,
    },
    /// LZNT1 data that does not decode.
    Compressed {
        /// The offset in the compressed input.
        at: usize,
    },
    /// Reparse data shorter than its tag's layout needs.
    ReparseData {
        /// The part.
        what: &'static str,
    },
}

impl fmt::Display for FormatError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::RecordSize { len } => write!(
                f,
                "a structure of {len} bytes is not a whole number of 512-byte sectors"
            ),
            Self::Signature { found } => write!(f, "unexpected signature {found:02x?}"),
            Self::Sizes {
                allocated,
                used,
                first_attribute,
                len,
            } => write!(
                f,
                "record of {len} bytes declares {allocated} bytes allocated and {used} used, \
                 with its first attribute at {first_attribute}"
            ),
            Self::UpdateSequence { offset, count, len } => write!(
                f,
                "update sequence array of {count} entries at {offset} does not fit the first \
                 sector or cover the {len}-byte structure"
            ),
            Self::FirstAttribute { offset, header_end } => write!(
                f,
                "first attribute at {offset} is misaligned or inside the header, which ends at \
                 {header_end}"
            ),
            Self::TornSector { sector } => write!(
                f,
                "sector {sector} does not end with the update sequence number: the structure \
                 is torn"
            ),
            Self::RecordNumber { expected, found } => {
                write!(f, "record {expected} states that it is record {found}")
            }
            Self::Truncated {
                what,
                at,
                need,
                len,
            } => write!(
                f,
                "{what} at offset {at} needs {need} bytes, past the {len} bytes available"
            ),
            Self::AttributeOverrun {
                at,
                what,
                offset,
                need,
                length,
            } => write!(
                f,
                "{what} of the attribute at offset {at} needs {need} bytes from {offset}, past \
                 its {length} bytes"
            ),
            Self::AttributeLength { at, length } => write!(
                f,
                "attribute at offset {at} is {length} bytes long: shorter than its header, or \
                 not a multiple of 8"
            ),
            Self::AttributeForm { at, form } => write!(
                f,
                "attribute at offset {at} is neither resident nor non-resident (form {form})"
            ),
            Self::WrongForm { at } => {
                write!(
                    f,
                    "attribute at offset {at} does not have the form asked for"
                )
            }
            Self::NameOffset { at, offset, header } => write!(
                f,
                "name of the attribute at offset {at} starts at {offset}, inside its \
                 {header}-byte header or at an odd offset"
            ),
            Self::ValueOffset { at, offset, after } => write!(
                f,
                "value of the attribute at offset {at} starts at {offset}, before the end of \
                 its header and name at {after}"
            ),
            Self::MappingPairsOffset {
                at,
                offset,
                after,
                length,
            } => write!(
                f,
                "mapping pairs of the attribute at offset {at} start at {offset}: before the \
                 end of its header and name at {after}, or past its {length} bytes"
            ),
            Self::MappingPairWidths { at, header } => write!(
                f,
                "mapping pair at offset {at} declares invalid field widths ({header:#04x})"
            ),
            Self::ZeroLengthRun { at } => {
                write!(
                    f,
                    "mapping pair at offset {at} describes a run of no clusters"
                )
            }
            Self::LcnOverflow { at } => {
                write!(
                    f,
                    "mapping pair at offset {at} overflows the cluster number"
                )
            }
            Self::LcnRange { at, lcn, length } => write!(
                f,
                "mapping pair at offset {at} describes {length} clusters at LCN {lcn}, outside \
                 any volume"
            ),
            Self::VcnOverflow { at } => {
                write!(
                    f,
                    "mapping pair at offset {at} overflows the virtual cluster number"
                )
            }
            Self::StandardInformationSize { len } => write!(
                f,
                "$STANDARD_INFORMATION of {len} bytes is neither the 48-byte nor the 72-byte \
                 layout"
            ),
            Self::Namespace { value } => write!(f, "unknown $FILE_NAME namespace {value}"),
            Self::AttributeListEntryLength { at, length } => write!(
                f,
                "attribute-list entry at offset {at} is {length} bytes, shorter than its header"
            ),
            Self::AttributeListNameOffset { at, offset } => write!(
                f,
                "name of the attribute-list entry at offset {at} starts at {offset}, inside \
                 its header"
            ),
            Self::IndexEntryLength { at, length } => write!(
                f,
                "index entry at offset {at} is {length} bytes, shorter than its header"
            ),
            Self::IndexNode {
                entries_offset,
                index_length,
            } => write!(
                f,
                "index node entries at {entries_offset} lie outside its {index_length} bytes"
            ),
            Self::BootSector { field } => write!(f, "boot sector: invalid {field}"),
            Self::Compressed { at } => write!(f, "LZNT1 data does not decode at offset {at}"),
            Self::ReparseData { what } => write!(f, "reparse data too short for its {what}"),
        }
    }
}

impl core::error::Error for FormatError {}

#[cfg(feature = "std")]
pub use self::with_std::Error;

#[cfg(feature = "std")]
mod with_std {
    use std::{fmt, io};

    use super::FormatError;

    /// A failed operation on a source, a volume or a scan.
    #[derive(Debug)]
    #[non_exhaustive]
    pub enum Error {
        /// A structure was refused.
        Format(FormatError),
        /// The source could not be read.
        Io(io::Error),
        /// `$MFT`'s runs do not map the record (or a hole in the source covers it).
        Unmapped {
            /// The record.
            record: u64,
        },
        /// A record or attribute the operation needs is missing.
        Missing {
            /// What is missing.
            what: &'static str,
        },
        /// A limit was reached.
        Limit {
            /// The limit.
            what: &'static str,
            /// Its value.
            limit: u64,
        },
        /// The operation was cancelled.
        Cancelled,
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Format(error) => error.fmt(f),
                Self::Io(error) => write!(f, "I/O: {error}"),
                Self::Unmapped { record } => {
                    write!(f, "record {record} is not mapped by the runs of $MFT")
                }
                Self::Missing { what } => write!(f, "{what} is missing"),
                Self::Limit { what, limit } => write!(f, "{what} exceeds the limit of {limit}"),
                Self::Cancelled => f.write_str("cancelled"),
            }
        }
    }

    impl std::error::Error for Error {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            match self {
                Self::Format(error) => Some(error),
                Self::Io(error) => Some(error),
                _ => None,
            }
        }
    }

    impl From<FormatError> for Error {
        fn from(error: FormatError) -> Self {
            Self::Format(error)
        }
    }

    impl From<io::Error> for Error {
        fn from(error: io::Error) -> Self {
            Self::Io(error)
        }
    }
}
