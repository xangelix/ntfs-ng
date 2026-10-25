//! `INDX` blocks and update sequences of arbitrary bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_ng::{usa, value::IndexBlock};

fuzz_target!(|data: &[u8]| {
    for size in [512usize, 1024, 4096] {
        let mut buf = data
            .get(..size.min(data.len()))
            .unwrap_or_default()
            .to_vec();
        buf.resize(size, 0);
        let mut block = buf.clone();
        if let Ok(parsed) = IndexBlock::fixup(&mut block) {
            if let Ok(entries) = parsed.entries() {
                for _ in entries {}
            }
            let _ = parsed.slack();
        }
        for header in [32usize, 40] {
            let mut page = buf.clone();
            if usa::unprotect(&mut page, header).is_ok() {
                usa::protect(&mut page, header).expect("a page unprotected protects");
                assert_eq!(page, buf);
            } else {
                assert_eq!(page, buf);
            }
        }
    }
});
