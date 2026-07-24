use std::fs;

use neon_cxl_cache::cli::{DEFAULT_POOL_PATH, path_value};
use neon_cxl_cache::{FilePool, MappingAccess, PAGE_SIZE, PageLocation, Result, crc32c, read_page};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pool_path = path_value(&args, "--pool-file", DEFAULT_POOL_PATH)?;
    let location_path = path_value(&args, "--location-file", "page.location")?;
    let location = PageLocation::decode(&fs::read(&location_path)?)?;
    let pool = FilePool::open(&pool_path, MappingAccess::ReadOnly)?;
    let page = read_page(&pool, location)?;
    let first = page[0];
    if page.iter().any(|byte| *byte != first) {
        return Err("page does not contain a uniform test pattern".into());
    }
    assert_eq!(page.len(), PAGE_SIZE as usize);
    println!(
        "READ_OK region_id={} slot_id={} control={} checksum={} byte={}",
        location.region_id,
        location.slot_id,
        location.expected_control,
        crc32c(&page),
        first
    );
    Ok(())
}
