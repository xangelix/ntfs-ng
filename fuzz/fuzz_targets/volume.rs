//! A volume image of arbitrary bytes behind a valid boot sector: `$MFT` mapping, records,
//! attribute lists, streams and `$Bitmap`, all bounded.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_ng::{
    types,
    volume::{Volume, bitmap::ClusterBitmap, stream::StreamReader},
};

fuzz_target!(|data: &[u8]| {
    // A boot sector that places $MFT at cluster 1 of 512-byte clusters, so arbitrary bytes
    // after it become $MFT's records.
    let mut image = vec![0u8; 512];
    image[3..11].copy_from_slice(b"NTFS    ");
    image[0x0b..0x0d].copy_from_slice(&512u16.to_le_bytes());
    image[0x0d] = 1;
    image[0x28..0x30].copy_from_slice(&256u64.to_le_bytes());
    image[0x30..0x38].copy_from_slice(&1u64.to_le_bytes());
    image[0x40] = 0xf6;
    image[0x44] = 0xf6;
    image[0x1fe] = 0x55;
    image[0x1ff] = 0xaa;
    image.extend_from_slice(data);
    image.resize(256 * 512, 0);
    let Ok(volume) = Volume::open(image.as_slice()) else {
        return;
    };
    let mut bitmap = ClusterBitmap::open(&volume).ok();
    let mut buf = Vec::new();
    for number in 0..volume.mft().records().min(16) {
        let Ok(record) = volume.record(number, &mut buf) else {
            continue;
        };
        let list = volume.attribute_list(&record).ok().flatten();
        if let Ok(Some((map, sizes))) =
            volume.runs(number, &record, list.as_deref(), types::DATA, &[])
        {
            let mut reader = StreamReader::new(&volume, map, sizes).budget(1 << 20);
            let _ = reader.read(
                0..u64::MAX,
                bitmap.as_mut(),
                &mut |_, _| -> Result<(), ()> { Ok(()) },
            );
        }
    }
});
