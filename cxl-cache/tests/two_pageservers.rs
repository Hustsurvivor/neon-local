use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use neon_cxl_cache::{
    FilePool, MappingAccess, PAGE_SIZE, PoolLayout, REGION_ALLOCATED, ReadError, read_page,
    write_page,
};

fn temp_pool_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("neon-cxl-cache-{}-{nonce}.bin", std::process::id()))
}

#[test]
fn two_pageservers_have_isolated_regions_and_stale_reads_are_rejected() {
    let path = temp_pool_path();
    let region_bytes = 32 * 1024 * 1024;
    let layout = PoolLayout {
        pool_data_bytes: region_bytes * 2,
        region_bytes,
        region_count: 2,
    };
    let writer_pool = FilePool::create(&path, layout, [9; 16], 1).unwrap();

    for region_id in 0..2 {
        let descriptor = writer_pool.region_descriptor(region_id).unwrap();
        descriptor
            .pageserver_id
            .store(u64::from(region_id) + 100, Ordering::Relaxed);
        descriptor.region_epoch.store(1, Ordering::Relaxed);
        descriptor.state.store(REGION_ALLOCATED, Ordering::Release);
    }

    let page_zero = [0x11; PAGE_SIZE as usize];
    let page_one = [0x22; PAGE_SIZE as usize];
    let location_zero = write_page(&writer_pool, 0, 1, 0, 11, 100, &page_zero).unwrap();
    let location_one = write_page(&writer_pool, 1, 1, 0, 22, 200, &page_one).unwrap();

    let reader_pool = FilePool::open(&path, MappingAccess::ReadOnly).unwrap();
    assert_eq!(read_page(&reader_pool, location_zero).unwrap(), page_zero);
    assert_eq!(read_page(&reader_pool, location_one).unwrap(), page_one);
    assert_ne!(location_zero.absolute_offset, location_one.absolute_offset);

    let replacement = [0x33; PAGE_SIZE as usize];
    let replacement_location = write_page(&writer_pool, 0, 1, 0, 33, 300, &replacement).unwrap();
    assert_eq!(
        read_page(&reader_pool, location_zero),
        Err(ReadError::Stale)
    );
    assert_eq!(
        read_page(&reader_pool, replacement_location).unwrap(),
        replacement
    );
    assert_eq!(read_page(&reader_pool, location_one).unwrap(), page_one);

    drop(reader_pool);
    drop(writer_pool);
    std::fs::remove_file(path).unwrap();
}
