use std::fs;

use neon_cxl_cache::cli::{
    DEFAULT_POOL_PATH, DEFAULT_SOCKET_PATH, option_value, path_value, required_value,
};
use neon_cxl_cache::{DaemonClient, FilePool, MappingAccess, PAGE_SIZE, Result, write_page};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pool_path = path_value(&args, "--pool-file", DEFAULT_POOL_PATH)?;
    let socket_path = path_value(&args, "--socket", DEFAULT_SOCKET_PATH)?;
    let location_path = path_value(&args, "--location-file", "page.location")?;
    let pageserver_id: u64 = required_value(&args, "--pageserver-id")?.parse()?;
    let process_nonce: u64 =
        option_value(&args, "--nonce", &std::process::id().to_string())?.parse()?;
    let slot_id: u64 = option_value(&args, "--slot", "0")?.parse()?;
    let byte: u8 = option_value(&args, "--byte", "165")?.parse()?;
    let page_lsn: u64 = option_value(&args, "--lsn", "1")?.parse()?;

    let pool = FilePool::open(&pool_path, MappingAccess::ReadWrite)?;
    let allocation = DaemonClient::allocate(
        &socket_path,
        pageserver_id,
        pool.header().layout.region_bytes,
        process_nonce,
    )?;
    let page = [byte; PAGE_SIZE as usize];
    let location = write_page(
        &pool,
        allocation.region_id,
        allocation.region_epoch,
        slot_id,
        u64::from(byte),
        page_lsn,
        &page,
    )?;
    fs::write(&location_path, location.encode())?;
    println!(
        "PUBLISHED pageserver_id={} region_id={} region_epoch={} slot_id={} control={} checksum={} byte={} location={}",
        pageserver_id,
        location.region_id,
        location.region_epoch,
        location.slot_id,
        location.expected_control,
        location.checksum_crc32c,
        byte,
        location_path.display()
    );
    Ok(())
}
