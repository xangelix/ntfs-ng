//! A `$MFT` of arbitrary records: the scan, the file table, the directory index under every
//! policy, and every path.
#![no_main]

use libfuzzer_sys::fuzz_target;
use ntfs_ng::{
    files::{FileOptions, Files},
    index::PathPolicy,
    mft::Mft,
    scan::Scan,
};

fuzz_target!(|data: &[u8]| {
    let Ok(mft) = Mft::from_file_with_record_size(data, 1024) else {
        return;
    };
    for policy in [PathPolicy::LIVE, PathPolicy::STRICT, PathPolicy::RECOVERY] {
        let scan = Scan::new(&mft).threads(2).chunk_bytes(2048);
        let options = FileOptions {
            deleted: true,
            metadata: true,
            dos_aliases: true,
            policy: PathPolicy {
                max_depth: 8,
                ..policy
            },
        };
        let Ok(files) = Files::scan(&scan, mft.records(), options) else {
            continue;
        };
        let parents = files.parents(2);
        assert_eq!(parents.len(), files.table.link_count());
        let mut path = String::new();
        for (id, link) in files.table.links() {
            let _ = files.table.file(link.file).expect("a link's file exists");
            if let Some(&dir) = parents.get(id as usize)
                && dir != ntfs_ng::index::NONE
            {
                path.clear();
                files.index.write_path(dir, '\\', &mut path);
                assert!(files.index.depth(dir) < 8);
            }
        }
    }
});
