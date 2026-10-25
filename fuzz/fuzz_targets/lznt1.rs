//! LZNT1: arbitrary input never panics or overruns, and compressed data round-trips.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_ng::{lznt1, testing::lznt1_compress};

fuzz_target!(|data: &[u8]| {
    let mut out = vec![0u8; 65_536];
    if let Ok(written) = lznt1::decompress(data, &mut out) {
        assert!(written <= out.len());
    }
    let input = data.get(..data.len().min(65_536)).unwrap_or_default();
    let packed = lznt1_compress(input);
    let mut back = vec![0u8; input.len().next_multiple_of(4096).max(4096)];
    lznt1::decompress(&packed, &mut back).expect("compressed data decodes");
    assert_eq!(&back[..input.len()], input);
});
