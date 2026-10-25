//! A record of arbitrary bytes: fixed up in place, every attribute and value read, and the
//! update sequence undone exactly.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_ng::{
    Form, Record, types,
    value::{
        AttributeList, FileName, IndexRoot, ObjectId, ReparsePoint, StandardInformation,
        VolumeInformation,
    },
};

fuzz_target!(|data: &[u8]| {
    for size in [512usize, 1024, 4096] {
        let Some(slot) = data.get(..size.min(data.len())) else {
            continue;
        };
        let mut buf = slot.to_vec();
        buf.resize(size, 0);
        let before = buf.clone();
        let Ok(record) = Record::fixup(&mut buf) else {
            assert_eq!(buf, before, "a refused record was changed");
            continue;
        };
        let mut raw = Vec::new();
        record.raw_into(&mut raw);
        assert_eq!(raw, before, "the update sequence did not undo exactly");
        for attribute in record.attributes() {
            let Ok(attribute) = attribute else {
                break;
            };
            let _ = attribute.name().map(|name| name.to_string_lossy());
            let _ = attribute.data_size();
            match attribute.form() {
                Ok(Form::Resident(value)) => match attribute.type_code() {
                    types::STANDARD_INFORMATION => {
                        let _ = StandardInformation::parse(value);
                    }
                    types::FILE_NAME => {
                        if let Ok(name) = FileName::parse(value) {
                            let mut text = String::new();
                            name.name.push_lossy(&mut text);
                            let mut wtf8 = Vec::new();
                            name.name.push_wtf8(&mut wtf8);
                        }
                    }
                    types::ATTRIBUTE_LIST => for _ in AttributeList::new(value) {},
                    types::OBJECT_ID => {
                        let _ = ObjectId::parse(value);
                    }
                    types::INDEX_ROOT => {
                        if let Ok(root) = IndexRoot::parse(value)
                            && let Ok(entries) = root.entries(value)
                        {
                            for entry in entries.flatten() {
                                let _ = entry.key().map(FileName::parse);
                                let _ = entry.child_vcn();
                            }
                        }
                    }
                    types::REPARSE_POINT => {
                        if let Ok(point) = ReparsePoint::parse(value) {
                            let _ = point.link_target();
                        }
                    }
                    types::VOLUME_INFORMATION => {
                        let _ = VolumeInformation::parse(value);
                    }
                    _ => {}
                },
                Ok(Form::NonResident(extent)) => for _ in extent.runs() {},
                Err(_) => {}
            }
        }
    }
});
