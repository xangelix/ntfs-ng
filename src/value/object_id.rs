//! `$OBJECT_ID` (0x40).

use crate::{
    FormatError,
    bytes::{field, take},
};

/// `$OBJECT_ID` (0x40): the file's distributed link tracking identifier, and in the 64-byte
/// form where it was born.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObjectId {
    /// The GUID as stored (little-endian fields).
    pub object_id: [u8; 16],
    /// The birth volume, birth object and domain identifiers, present when the value is the
    /// 64-byte form.
    pub birth: Option<[[u8; 16]; 3]>,
}

impl ObjectId {
    /// The 64-byte form.
    pub const LONG: usize = 64;

    /// Read `value`. Values of other lengths than 16 and 64 are read for the object identifier
    /// alone.
    ///
    /// # Errors
    /// If it is shorter than one GUID.
    pub fn parse(value: &[u8]) -> Result<Self, FormatError> {
        let object_id = field(take(value, 0, 16, "$OBJECT_ID")?, 0);
        Ok(Self {
            object_id,
            birth: (value.len() == Self::LONG)
                .then(|| [field(value, 16), field(value, 32), field(value, 48)]),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]

    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn object_ids_keep_birth_identifiers_only_in_the_long_form() {
        let value: Vec<u8> = (0..64).collect();
        let long = ObjectId::parse(&value).unwrap();
        assert_eq!(long.object_id[15], 15);
        assert_eq!(long.birth.unwrap()[2][0], 48);
        assert_eq!(ObjectId::parse(&value[..16]).unwrap().birth, None);
        assert_eq!(ObjectId::parse(&value[..40]).unwrap().birth, None);
        assert!(ObjectId::parse(&value[..15]).is_err());
    }
}
