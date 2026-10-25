//! Attribute values of arbitrary bytes, each parser on its own.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_ng::{
    BootSector, MappingPairs,
    value::{
        AttributeList, FileName, IndexRoot, ObjectId, ReparsePoint, StandardInformation,
        VolumeInformation,
    },
};

fuzz_target!(|data: &[u8]| {
    let _ = StandardInformation::parse(data);
    let _ = FileName::parse(data).map(|name| name.name.to_string_lossy());
    let _ = FileName::link(data);
    for _ in AttributeList::new(data) {}
    let _ = ObjectId::parse(data);
    let _ = VolumeInformation::parse(data);
    if let Ok(root) = IndexRoot::parse(data)
        && let Ok(entries) = root.entries(data)
    {
        for _ in entries {}
    }
    if let Ok(point) = ReparsePoint::parse(data) {
        let _ = point.link_target();
    }
    let _ = BootSector::parse(data);
    for start in [0u64, 1, u64::MAX / 2] {
        let mut pairs = MappingPairs::new(data, 0, start);
        for _ in pairs.by_ref() {}
        let _ = pairs.next_vcn();
    }
});
