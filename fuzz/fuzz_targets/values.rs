//! Attribute values of arbitrary bytes, each parser on its own.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_ng::{
    BootSector, FormatError, MappingPairs,
    runs::Pairs,
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
    // The runs are the pairs, with VCNs: the same lengths and LCNs, the same error, the same
    // end; or the pairs read on where the runs pass the largest VCN.
    let mut pairs = Pairs::new(data, 0);
    let mut runs = MappingPairs::new(data, 0, 0);
    loop {
        match (pairs.next(), runs.next()) {
            (Some(Ok(pair)), Some(Ok(run))) => {
                assert_eq!((pair.length, pair.lcn), (run.length, run.lcn));
            }
            (Some(Ok(_)), Some(Err(FormatError::VcnOverflow { .. }))) => break,
            (Some(Err(pair)), Some(Err(run))) => {
                assert_eq!(pair, run);
                break;
            }
            (None, None) => {
                assert_eq!(pairs.terminated(), runs.terminated());
                break;
            }
            (pair, run) => panic!("pairs {pair:?}, runs {run:?}"),
        }
    }
});
