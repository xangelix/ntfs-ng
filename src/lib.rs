//! Fast, hardened NTFS and `$MFT` parsing.
//!
//! `ntfs-ng` reads NTFS at whatever depth a program needs: five fields of every live file as fast
//! as the disk can deliver them, every byte of every record checked and kept, or streams copied
//! from live volumes without ever trusting a hostile image. One core does all of it because
//! nothing here decides what a record *means*: callers choose what to read (a
//! [`scan::Projection`]), how strictly to judge it, and how paths are resolved
//! ([`index::PathPolicy`]).
//!
//! # Layers
//!
//! - **Structures** (no `std` needed): [`record`] (`FILE` records and their update sequence),
//!   [`attribute`] (attribute headers), [`runs`] (mapping pairs), [`value`] (`$STANDARD_INFORMATION`,
//!   `$FILE_NAME`, `$ATTRIBUTE_LIST`, `$OBJECT_ID`, indexes, reparse points, volume information),
//!   [`name`] (UTF-16 names), [`usa`] (the update sequence of `INDX`, `RSTR` and `RCRD` pages),
//!   [`boot`] (the boot sector), [`lznt1`] (decompression) and [`time`]. Every view borrows the
//!   bytes it reads; every length, count and offset is checked before use; no input panics.
//! - **Sources and volumes** (`std`): [`source`] (positional reads with holes, aligned devices),
//!   [`mft`] (the `$MFT` record space: a standalone dump or a volume's fragmented `$MFT`),
//!   [`volume`] (records, attribute lists, extension records, streams, `$Bitmap`).
//! - **Scanning and paths** (`std`): [`scan`] (a parallel, ordered or unordered scan of every
//!   record into caller-defined outputs), [`index`] (a directory index with explicit parent,
//!   orphan and depth policies, and fast path materialization), [`files`] (files rather than
//!   records: names and sizes with extension records folded in) and [`usage`] (each live name
//!   with its file's sizes and two times, a third of the file table's memory).
//!
//! # Guarantees
//!
//! - `#![forbid(unsafe_code)]` and no dependencies.
//! - No input makes a reader panic, loop forever, or allocate without bound: every structure is
//!   checked, iterators are fused after their first error, and sizes are capped.
//! - Bytes are never silently "fixed": a record's update sequence is applied in place and can be
//!   undone exactly ([`record::Record::raw_into`]), so the bytes as stored stay recoverable.

#![cfg_attr(not(any(feature = "std", test)), no_std)]

extern crate alloc;

pub mod attribute;
pub mod boot;
mod bytes;
mod error;
pub mod lznt1;
pub mod name;
pub mod record;
pub mod runs;
pub mod time;
pub mod usa;
pub mod value;

#[cfg(feature = "std")]
pub mod files;
#[cfg(feature = "std")]
pub mod index;
#[cfg(feature = "std")]
pub mod mft;
#[cfg(feature = "std")]
pub mod scan;
#[cfg(feature = "std")]
pub mod source;
#[cfg(feature = "std")]
pub mod usage;
#[cfg(feature = "std")]
pub mod volume;

#[cfg(feature = "testing")]
pub mod testing;

pub use attribute::{Attribute, Attributes, Form, NonResident};
pub use boot::BootSector;
#[cfg(feature = "std")]
pub use error::Error;
pub use error::FormatError;
pub use name::Name;
pub use record::{FileReference, Record, RecordHeader};
pub use runs::{MappingPairs, Run};

/// Attribute type codes.
pub mod types {
    /// `$STANDARD_INFORMATION`.
    pub const STANDARD_INFORMATION: u32 = 0x10;
    /// `$ATTRIBUTE_LIST`.
    pub const ATTRIBUTE_LIST: u32 = 0x20;
    /// `$FILE_NAME`.
    pub const FILE_NAME: u32 = 0x30;
    /// `$OBJECT_ID` (`$VOLUME_VERSION` in NTFS 1.2).
    pub const OBJECT_ID: u32 = 0x40;
    /// `$SECURITY_DESCRIPTOR`.
    pub const SECURITY_DESCRIPTOR: u32 = 0x50;
    /// `$VOLUME_NAME`.
    pub const VOLUME_NAME: u32 = 0x60;
    /// `$VOLUME_INFORMATION`.
    pub const VOLUME_INFORMATION: u32 = 0x70;
    /// `$DATA`.
    pub const DATA: u32 = 0x80;
    /// `$INDEX_ROOT`.
    pub const INDEX_ROOT: u32 = 0x90;
    /// `$INDEX_ALLOCATION`.
    pub const INDEX_ALLOCATION: u32 = 0xA0;
    /// `$BITMAP`.
    pub const BITMAP: u32 = 0xB0;
    /// `$REPARSE_POINT` (`$SYMBOLIC_LINK` in NTFS 1.2).
    pub const REPARSE_POINT: u32 = 0xC0;
    /// `$EA_INFORMATION`.
    pub const EA_INFORMATION: u32 = 0xD0;
    /// `$EA`.
    pub const EA: u32 = 0xE0;
    /// `$PROPERTY_SET` (NTFS 1.2 only).
    pub const PROPERTY_SET: u32 = 0xF0;
    /// `$LOGGED_UTILITY_STREAM`.
    pub const LOGGED_UTILITY_STREAM: u32 = 0x100;
    /// Ends a record's attributes.
    pub const END: u32 = 0xFFFF_FFFF;
}

/// Well-known record numbers of the NTFS metadata files.
pub mod records {
    /// `$MFT`.
    pub const MFT: u64 = 0;
    /// `$MFTMirr`.
    pub const MFT_MIRROR: u64 = 1;
    /// `$LogFile`.
    pub const LOG_FILE: u64 = 2;
    /// `$Volume`.
    pub const VOLUME: u64 = 3;
    /// `$AttrDef`.
    pub const ATTRIBUTE_DEFINITIONS: u64 = 4;
    /// The root directory.
    pub const ROOT: u64 = 5;
    /// `$Bitmap`.
    pub const BITMAP: u64 = 6;
    /// `$Boot`.
    pub const BOOT: u64 = 7;
    /// `$BadClus`.
    pub const BAD_CLUSTERS: u64 = 8;
    /// `$Secure`.
    pub const SECURE: u64 = 9;
    /// `$UpCase`.
    pub const UPCASE: u64 = 10;
    /// `$Extend`.
    pub const EXTEND: u64 = 11;
    /// Records below this number are reserved for metadata files.
    pub const FIRST_USER: u64 = 16;
}
