//! Attributes: what a `FILE` record holds after its header, each an attribute record of its
//! own, until the `$END` marker.
//!
//! Every attribute starts with a common header; a resident one continues with the size and
//! offset of its value (24 bytes in all), a non-resident one with its VCN range, sizes and the
//! offset of its mapping pairs (64 bytes, or 72 with the compressed size).
//!
//! | Offset | Size | Field |
//! |---|---|---|
//! | 0x00 | 4 | type ([`crate::types`]); `0xFFFFFFFF` is `$END` |
//! | 0x04 | 4 | length of the attribute record, a multiple of 8 |
//! | 0x08 | 1 | form: 0 resident, 1 non-resident |
//! | 0x09 | 1 | name length, in UTF-16 code units |
//! | 0x0A | 2 | name offset |
//! | 0x0C | 2 | flags ([`flags`]) |
//! | 0x0E | 2 | attribute identifier ("instance"), unique in the record |
//!
//! Resident, from 0x10: value size (4), value offset (2), indexed flag (1), padding (1).
//! Non-resident, from 0x10: lowest VCN (8), highest VCN (8), mapping pairs offset (2),
//! compression unit as a power of two clusters (2), padding (4), allocated size (8), data size
//! (8), initialized size (8), and for compressed or sparse streams the compressed size (8).
//!
//! Accessors are split so a scan pays only for what it reads: [`Attribute::type_code`] and the
//! other common fields need no check beyond the iterator's; [`Attribute::value`],
//! [`Attribute::non_resident`] and [`Attribute::name`] check their own parts.

use crate::{
    FormatError,
    bytes::{array, byte, le16, le32, le64, take},
    name::Name,
    runs::MappingPairs,
    types,
};

/// An attribute's flags.
pub mod flags {
    /// The compression method (LZNT1 is 1).
    pub const COMPRESSION_MASK: u16 = 0x00FF;
    /// The stream is encrypted (EFS).
    pub const ENCRYPTED: u16 = 0x4000;
    /// The stream is sparse.
    pub const SPARSE: u16 = 0x8000;
}

/// The common header's size, and a resident header's.
pub const RESIDENT_HEADER: usize = 24;
/// A non-resident header's size, without the compressed size.
pub const NON_RESIDENT_HEADER: usize = 64;
/// A non-resident header's size with the compressed size.
pub const COMPRESSED_HEADER: usize = 72;

/// A record's attributes, in order. Each is checked to be at least a header long, a multiple
/// of 8 bytes, and within the bytes in use; the first that is not ends the iteration with its
/// error. The `$END` marker ends it cleanly, and nothing after it is read.
#[derive(Clone, Debug)]
pub struct Attributes<'a> {
    used: &'a [u8],
    at: usize,
    done: bool,
}

impl<'a> Attributes<'a> {
    /// The attributes of `used`, a record's bytes in use, from `first`.
    #[must_use]
    pub const fn new(used: &'a [u8], first: usize) -> Self {
        Self {
            used,
            at: first,
            done: false,
        }
    }

    #[inline]
    fn read(&self) -> Result<Option<Attribute<'a>>, FormatError> {
        let at = self.at;
        let head = take(self.used, at, 4, "attribute type")?;
        if le32(head, 0) == types::END {
            return Ok(None);
        }
        let length = le32(take(self.used, at + 4, 4, "attribute length")?, 0);
        if (length as usize) < RESIDENT_HEADER || !length.is_multiple_of(8) {
            return Err(FormatError::AttributeLength { at, length });
        }
        let bytes = take(self.used, at, length as usize, "attribute")?;
        Ok(Some(Attribute { at, bytes }))
    }
}

impl<'a> Iterator for Attributes<'a> {
    type Item = Result<Attribute<'a>, FormatError>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.read() {
            Ok(Some(attribute)) => {
                self.at += attribute.bytes.len();
                Some(Ok(attribute))
            }
            Ok(None) => {
                self.done = true;
                None
            }
            Err(error) => {
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

impl core::iter::FusedIterator for Attributes<'_> {}

/// One attribute record, at least its 24-byte common header long.
#[derive(Clone, Copy, Debug)]
pub struct Attribute<'a> {
    at: usize,
    bytes: &'a [u8],
}

impl<'a> Attribute<'a> {
    /// `bytes` as an attribute record at offset `at` of its record, if it is at least a common
    /// header long.
    #[must_use]
    pub const fn new(at: usize, bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < RESIDENT_HEADER {
            return None;
        }
        Some(Self { at, bytes })
    }

    /// The attribute's offset in its record.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.at
    }

    /// The whole attribute record.
    #[must_use]
    pub const fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The type code ([`crate::types`]).
    #[must_use]
    #[inline]
    pub fn type_code(&self) -> u32 {
        le32(self.bytes, 0x00)
    }

    /// The record's length as declared, which is its length.
    #[must_use]
    pub fn length(&self) -> u32 {
        le32(self.bytes, 0x04)
    }

    /// 0 resident, 1 non-resident; anything else is refused by [`Self::form`].
    #[must_use]
    #[inline]
    pub fn form_code(&self) -> u8 {
        byte(self.bytes, 0x08)
    }

    /// Whether the attribute is resident (form 0).
    #[must_use]
    #[inline]
    pub fn is_resident(&self) -> bool {
        self.form_code() == 0
    }

    /// The name's length in UTF-16 code units; 0 when unnamed.
    #[must_use]
    #[inline]
    pub fn name_length(&self) -> u8 {
        byte(self.bytes, 0x09)
    }

    /// Whether the attribute is unnamed (the default stream of its type).
    #[must_use]
    #[inline]
    pub fn is_unnamed(&self) -> bool {
        self.name_length() == 0
    }

    /// Where the name starts; meaningless when unnamed.
    #[must_use]
    pub fn name_offset(&self) -> u16 {
        le16(self.bytes, 0x0a)
    }

    /// The flags ([`flags`]).
    #[must_use]
    #[inline]
    pub fn flags(&self) -> u16 {
        le16(self.bytes, 0x0c)
    }

    /// The attribute identifier, unique within its record.
    #[must_use]
    pub fn instance(&self) -> u16 {
        le16(self.bytes, 0x0e)
    }

    /// Whether the stream is compressed.
    #[must_use]
    pub fn is_compressed(&self) -> bool {
        self.flags() & flags::COMPRESSION_MASK != 0
    }

    /// Whether the stream is sparse.
    #[must_use]
    pub fn is_sparse(&self) -> bool {
        self.flags() & flags::SPARSE != 0
    }

    /// Whether the stream is encrypted.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.flags() & flags::ENCRYPTED != 0
    }

    /// `need` bytes from `offset` of the attribute, or why not.
    fn within(
        &self,
        offset: usize,
        need: usize,
        what: &'static str,
    ) -> Result<&'a [u8], FormatError> {
        take(self.bytes, offset, need, what).map_err(|_| FormatError::AttributeOverrun {
            at: self.at,
            what,
            offset,
            need,
            length: self.bytes.len(),
        })
    }

    /// The non-resident header's size for this attribute: 72 when it holds the compressed size
    /// (a compression unit, or a compressed or sparse stream whose layout leaves room for it),
    /// 64 otherwise.
    fn non_resident_header_len(&self) -> usize {
        let unit = self
            .bytes
            .get(0x22..0x24)
            .is_some_and(|unit| unit != [0, 0]);
        if unit {
            return COMPRESSED_HEADER;
        }
        let compressed_or_sparse = self.flags() & (flags::COMPRESSION_MASK | flags::SPARSE) != 0;
        let pairs = array::<2>(self.bytes, 0x20)
            .map_or(0, |offset| usize::from(u16::from_le_bytes(offset)));
        let name_start = if self.is_unnamed() {
            usize::MAX
        } else {
            usize::from(self.name_offset())
        };
        if compressed_or_sparse
            && self.bytes.len() >= COMPRESSED_HEADER
            && pairs.min(name_start) >= COMPRESSED_HEADER
        {
            COMPRESSED_HEADER
        } else {
            NON_RESIDENT_HEADER
        }
    }

    /// The header's size: 24 resident, 64 or 72 non-resident.
    #[must_use]
    pub fn header_len(&self) -> usize {
        if self.is_resident() {
            RESIDENT_HEADER
        } else {
            self.non_resident_header_len()
        }
    }

    /// Where the name ends, or the header when unnamed: the value or the mapping pairs start no
    /// earlier.
    fn name_end(&self, header_len: usize) -> usize {
        if self.is_unnamed() {
            header_len
        } else {
            usize::from(self.name_offset()) + usize::from(self.name_length()) * 2
        }
    }

    /// The name (empty when unnamed), checked to lie within the attribute, after its header, at
    /// an even offset.
    ///
    /// # Errors
    /// If it does not.
    pub fn name(&self) -> Result<Name<'a>, FormatError> {
        let units = usize::from(self.name_length());
        if units == 0 {
            return Ok(Name::default());
        }
        let offset = self.name_offset();
        let header = self.header_len();
        if usize::from(offset) < header || !offset.is_multiple_of(2) {
            return Err(FormatError::NameOffset {
                at: self.at,
                offset,
                header,
            });
        }
        Ok(Name::new(self.within(
            usize::from(offset),
            units * 2,
            "name",
        )?))
    }

    /// A resident attribute's value: checked to start after the header and name, and to lie
    /// within the attribute.
    ///
    /// # Errors
    /// If the attribute is not resident, or the value is misplaced.
    #[inline]
    pub fn value(&self) -> Result<&'a [u8], FormatError> {
        match self.form_code() {
            0 => {}
            1 => return Err(FormatError::WrongForm { at: self.at }),
            form => return Err(FormatError::AttributeForm { at: self.at, form }),
        }
        let length = le32(self.bytes, 0x10) as usize;
        let offset = le16(self.bytes, 0x14);
        let after = self.name_end(RESIDENT_HEADER);
        if usize::from(offset) < after {
            return Err(FormatError::ValueOffset {
                at: self.at,
                offset,
                after,
            });
        }
        self.within(usize::from(offset), length, "value")
    }

    /// Whether a resident value is indexed (set for `$FILE_NAME`s).
    #[must_use]
    pub fn is_indexed(&self) -> bool {
        self.is_resident() && byte(self.bytes, 0x16) & 1 != 0
    }

    /// A resident attribute's own header fields, as stored and unchecked ([`Self::value`]
    /// checks where the value is); `None` for another form.
    #[must_use]
    pub fn resident_header(&self) -> Option<ResidentHeader> {
        self.is_resident().then(|| ResidentHeader {
            value_length: le32(self.bytes, 0x10),
            value_offset: le16(self.bytes, 0x14),
            indexed: byte(self.bytes, 0x16),
            reserved: byte(self.bytes, 0x17),
        })
    }

    /// A non-resident attribute's header and mapping pairs.
    ///
    /// # Errors
    /// If the attribute is not non-resident, its header runs past it, or its mapping pairs are
    /// misplaced.
    pub fn non_resident(&self) -> Result<NonResident<'a>, FormatError> {
        match self.form_code() {
            1 => {}
            0 => return Err(FormatError::WrongForm { at: self.at }),
            form => return Err(FormatError::AttributeForm { at: self.at, form }),
        }
        let header_len = self.non_resident_header_len();
        let bytes = self.within(0, header_len, "non-resident header")?;
        let after = self.name_end(header_len);
        let pairs_offset = le16(bytes, 0x20);
        let offset = usize::from(pairs_offset);
        let pairs = match self.bytes.get(offset..) {
            Some(pairs) if offset >= after => pairs,
            _ => {
                return Err(FormatError::MappingPairsOffset {
                    at: self.at,
                    offset: pairs_offset,
                    after,
                    length: self.length(),
                });
            }
        };
        Ok(NonResident {
            lowest_vcn: le64(bytes, 0x10),
            highest_vcn: le64(bytes, 0x18),
            compression_unit: le16(bytes, 0x22),
            allocated_size: le64(bytes, 0x28),
            data_size: le64(bytes, 0x30),
            initialized_size: le64(bytes, 0x38),
            total_allocated: (header_len == COMPRESSED_HEADER).then(|| le64(bytes, 0x40)),
            mapping_pairs_offset: pairs_offset,
            reserved: le32(bytes, 0x24),
            pairs,
            pairs_at: self.at + offset,
        })
    }

    /// The data size alone: a resident value's length, or a non-resident stream's data size
    /// (valid in the stream's first extent only). Cheaper than [`Self::value`] or
    /// [`Self::non_resident`]: the offsets are checked, the mapping pairs are not.
    ///
    /// # Errors
    /// If the form is unknown, or the field lies past the attribute.
    #[inline]
    pub fn data_size(&self) -> Result<u64, FormatError> {
        match self.form_code() {
            0 => Ok(u64::from(le32(self.bytes, 0x10))),
            1 => Ok(le64(
                self.within(0, NON_RESIDENT_HEADER, "non-resident header")?,
                0x30,
            )),
            form => Err(FormatError::AttributeForm { at: self.at, form }),
        }
    }

    /// The attribute's form, with its value or mapping pairs.
    ///
    /// # Errors
    /// As [`Self::value`] or [`Self::non_resident`].
    pub fn form(&self) -> Result<Form<'a>, FormatError> {
        match self.form_code() {
            0 => Ok(Form::Resident(self.value()?)),
            1 => Ok(Form::NonResident(self.non_resident()?)),
            form => Err(FormatError::AttributeForm { at: self.at, form }),
        }
    }
}

/// Resident or non-resident.
#[derive(Clone, Copy, Debug)]
pub enum Form<'a> {
    /// The value, in the record.
    Resident(&'a [u8]),
    /// Where the value's clusters are.
    NonResident(NonResident<'a>),
}

/// The resident part of a header (0x10 to 0x18).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResidentHeader {
    /// The value's length.
    pub value_length: u32,
    /// Where the value starts in the attribute.
    pub value_offset: u16,
    /// The indexed flag (bit 0, set for `$FILE_NAME`s), as stored.
    pub indexed: u8,
    /// The byte after it, reserved.
    pub reserved: u8,
}

/// The non-resident part of a header, with the mapping pairs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NonResident<'a> {
    /// The first VCN the mapping pairs describe.
    pub lowest_vcn: u64,
    /// The last VCN they describe; `u64::MAX` (-1) when an empty stream has none.
    pub highest_vcn: u64,
    /// Clusters per compression unit, as a power of two; 0 uncompressed.
    pub compression_unit: u16,
    /// Bytes allocated: valid in the stream's first extent (lowest VCN 0) only, as are the next
    /// two sizes.
    pub allocated_size: u64,
    /// Bytes of data.
    pub data_size: u64,
    /// Bytes initialized: past them the stream reads as zeros.
    pub initialized_size: u64,
    /// Bytes actually allocated, for compressed and sparse streams.
    pub total_allocated: Option<u64>,
    /// Where the mapping pairs start in the attribute.
    pub mapping_pairs_offset: u16,
    /// The four bytes after the compression unit, reserved.
    pub reserved: u32,
    pairs: &'a [u8],
    pairs_at: usize,
}

impl<'a> NonResident<'a> {
    /// The runs, from [`Self::lowest_vcn`].
    #[must_use]
    pub fn runs(&self) -> MappingPairs<'a> {
        MappingPairs::new(self.pairs, self.pairs_at, self.lowest_vcn)
    }

    /// The mapping pairs, as stored (to the end of the attribute).
    #[must_use]
    pub const fn pairs(&self) -> &'a [u8] {
        self.pairs
    }

    /// Whether this is the stream's first extent, whose sizes are valid.
    #[must_use]
    pub const fn is_first(&self) -> bool {
        self.lowest_vcn == 0
    }

    /// The clusters this extent describes: `highest - lowest + 1`, or 0 for an empty stream.
    #[must_use]
    pub const fn clusters(&self) -> u64 {
        if self.highest_vcn == u64::MAX || self.highest_vcn < self.lowest_vcn {
            0
        } else {
            self.highest_vcn - self.lowest_vcn + 1
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::{vec, vec::Vec};

    use super::*;

    /// A resident attribute of `typ` named `name` holding `value`.
    fn resident(typ: u32, id: u16, name: &str, value: &[u8]) -> Vec<u8> {
        let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let name_at = RESIDENT_HEADER;
        let value_at = (name_at + units.len()).next_multiple_of(8);
        let length = (value_at + value.len()).next_multiple_of(8);
        let mut out = vec![0; length];
        out[0..4].copy_from_slice(&typ.to_le_bytes());
        out[4..8].copy_from_slice(&u32::try_from(length).unwrap().to_le_bytes());
        out[9] = u8::try_from(units.len() / 2).unwrap();
        out[10..12].copy_from_slice(&u16::try_from(name_at).unwrap().to_le_bytes());
        out[14..16].copy_from_slice(&id.to_le_bytes());
        out[16..20].copy_from_slice(&u32::try_from(value.len()).unwrap().to_le_bytes());
        out[20..22].copy_from_slice(&u16::try_from(value_at).unwrap().to_le_bytes());
        out[name_at..name_at + units.len()].copy_from_slice(&units);
        out[value_at..value_at + value.len()].copy_from_slice(value);
        out
    }

    /// An unnamed non-resident `$DATA` with three runs.
    fn non_resident() -> Vec<u8> {
        let mut out = vec![0; 80];
        out[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        out[4..8].copy_from_slice(&80u32.to_le_bytes());
        out[8] = 1;
        out[0x18..0x20].copy_from_slice(&5u64.to_le_bytes());
        out[0x20..0x22].copy_from_slice(&64u16.to_le_bytes());
        out[0x28..0x30].copy_from_slice(&(6 * 4096u64).to_le_bytes());
        out[0x30..0x38].copy_from_slice(&20_000u64.to_le_bytes());
        out[0x38..0x40].copy_from_slice(&20_000u64.to_le_bytes());
        out[64..73].copy_from_slice(&[0x11, 2, 100, 0x01, 3, 0x11, 1, 0xf6, 0]);
        out
    }

    #[test]
    fn attributes_end_at_the_end_marker_and_check_their_bounds() {
        let mut used = resident(0x80, 1, "ads", b"value");
        used.extend(resident(0x30, 2, "", &[0; 66]));
        used.extend(u32::MAX.to_le_bytes());
        used.extend([0xee; 12]);
        let attributes: Vec<_> = Attributes::new(&used, 0).collect::<Result<_, _>>().unwrap();
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes[0].name().unwrap().to_string_lossy(), "ads");
        assert_eq!(attributes[0].value().unwrap(), b"value");
        assert_eq!(attributes[0].data_size().unwrap(), 5);
        assert!(attributes[0].non_resident().is_err());
        assert_eq!(
            attributes[0].resident_header(),
            Some(ResidentHeader {
                value_length: 5,
                value_offset: 32,
                indexed: 0,
                reserved: 0
            })
        );
        assert_eq!(attributes[1].offset(), 40);
        assert!(attributes[1].is_unnamed());

        let unterminated = &used[..used.len() - 16];
        let last = Attributes::new(unterminated, 0).last().unwrap();
        assert!(matches!(last, Err(FormatError::Truncated { .. })));
        for length in [20u32, 41] {
            let mut bad = used.clone();
            bad[4..8].copy_from_slice(&length.to_le_bytes());
            assert!(matches!(
                Attributes::new(&bad, 0).next(),
                Some(Err(FormatError::AttributeLength { .. }))
            ));
        }
    }

    #[test]
    fn names_values_and_mapping_pairs_must_follow_the_header() {
        let mut attribute = resident(0x80, 1, "a", b"xy");
        attribute[10..12].copy_from_slice(&22u16.to_le_bytes());
        let parsed = Attribute::new(0, &attribute).unwrap();
        assert!(matches!(parsed.name(), Err(FormatError::NameOffset { .. })));

        let mut value = resident(0x80, 1, "", b"xy");
        value[20..22].copy_from_slice(&16u16.to_le_bytes());
        assert!(matches!(
            Attribute::new(0, &value).unwrap().value(),
            Err(FormatError::ValueOffset { .. })
        ));

        let mut data = non_resident();
        let parsed = Attribute::new(0, &data).unwrap().non_resident().unwrap();
        assert_eq!(parsed.runs().count(), 3);
        assert_eq!(parsed.data_size, 20_000);
        assert_eq!((parsed.mapping_pairs_offset, parsed.reserved), (64, 0));
        assert_eq!(Attribute::new(0, &data).unwrap().resident_header(), None);
        assert_eq!(parsed.clusters(), 6);
        assert_eq!(
            Attribute::new(0, &data).unwrap().data_size().unwrap(),
            20_000
        );
        data[32..34].copy_from_slice(&80u16.to_le_bytes());
        let parsed = Attribute::new(0, &data).unwrap().non_resident().unwrap();
        assert_eq!(parsed.runs().count(), 0);
        data[32..34].copy_from_slice(&81u16.to_le_bytes());
        assert!(Attribute::new(0, &data).unwrap().non_resident().is_err());
        for len in [24, 32, 40] {
            let mut short = vec![0; len];
            short[0..4].copy_from_slice(&0x80u32.to_le_bytes());
            short[4..8].copy_from_slice(&u32::try_from(len).unwrap().to_le_bytes());
            short[8] = 1;
            if len > 34 {
                short[34] = 4;
            }
            assert!(matches!(
                Attribute::new(0, &short).unwrap().non_resident(),
                Err(FormatError::AttributeOverrun { need: 64 | 72, .. })
            ));
        }
        data[34..36].copy_from_slice(&4u16.to_le_bytes());
        data.truncate(64);
        assert!(Attribute::new(0, &data).unwrap().non_resident().is_err());
        data[8] = 2;
        assert!(matches!(
            Attribute::new(0, &data).unwrap().form(),
            Err(FormatError::AttributeForm { form: 2, .. })
        ));
    }

    #[test]
    fn sparse_streams_carry_the_compressed_size() {
        let mut data = vec![0; 80];
        data[0..4].copy_from_slice(&0x80u32.to_le_bytes());
        data[4..8].copy_from_slice(&80u32.to_le_bytes());
        data[8] = 1;
        data[12..14].copy_from_slice(&flags::SPARSE.to_le_bytes());
        data[0x20..0x22].copy_from_slice(&72u16.to_le_bytes());
        data[0x40..0x48].copy_from_slice(&4096u64.to_le_bytes());
        let parsed = Attribute::new(0, &data).unwrap().non_resident().unwrap();
        assert_eq!(parsed.total_allocated, Some(4096));
        // Without the flag, the same bytes are a 64-byte header.
        data[12..14].copy_from_slice(&0u16.to_le_bytes());
        let parsed = Attribute::new(0, &data).unwrap().non_resident().unwrap();
        assert_eq!(parsed.total_allocated, None);
    }
}
