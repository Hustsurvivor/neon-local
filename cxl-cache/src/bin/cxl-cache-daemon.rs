use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::Ordering;

use neon_cxl_cache::cli::{
    DEFAULT_POOL_PATH, DEFAULT_SOCKET_PATH, option_value, parse_size, path_value, random_uuid,
};
use neon_cxl_cache::{
    Allocation, DEFAULT_POOL_DATA_BYTES, DEFAULT_REGION_BYTES, DaemonRequest, DaemonResponse,
    FilePool, PoolLayout, REGION_ALLOCATED, REGION_FREE, Result,
};

#[derive(Clone, Copy)]
struct Owner {
    pageserver_id: u64,
    process_nonce: u64,
    epoch: u64,
}

fn handle_connection(
    stream: &mut UnixStream,
    pool: &FilePool,
    owners: &mut [Option<Owner>; 2],
) -> Result<()> {
    let request = DaemonRequest::read_from(stream)?;
    let response = match request {
        DaemonRequest::Allocate {
            pageserver_id,
            requested_size,
            process_nonce,
        } => {
            if requested_size != pool.header().layout.region_bytes {
                DaemonResponse::Error(format!(
                    "requested region is {requested_size} bytes; daemon requires {} bytes",
                    pool.header().layout.region_bytes
                ))
            } else if let Some((region_id, owner)) =
                owners.iter().enumerate().find_map(|(id, owner)| {
                    owner
                        .filter(|owner| {
                            owner.pageserver_id == pageserver_id
                                && owner.process_nonce == process_nonce
                        })
                        .map(|owner| (id, owner))
                })
            {
                let region = pool.header().layout.region(region_id as u32)?;
                DaemonResponse::Allocated(Allocation {
                    pool_uuid: pool.header().pool_uuid,
                    pool_epoch: pool.header().pool_epoch,
                    region_id: region_id as u32,
                    region_epoch: owner.epoch,
                    region_offset: region.region_offset,
                    region_size: region.region_bytes,
                    slot_count: region.slot_count,
                })
            } else if let Some(region_id) = owners
                .iter()
                .position(|owner| owner.is_some_and(|owner| owner.pageserver_id == pageserver_id))
            {
                /*
                 * A local Pageserver restart has a new process nonce. Reclaim
                 * its previous region and advance the epoch so locations
                 * published by the old process are rejected by readers.
                 */
                let descriptor = pool.region_descriptor(region_id as u32)?;
                let epoch = descriptor
                    .region_epoch
                    .load(Ordering::Acquire)
                    .checked_add(1)
                    .ok_or("region epoch exhausted")?;
                descriptor
                    .process_nonce
                    .store(process_nonce, Ordering::Relaxed);
                descriptor.region_epoch.store(epoch, Ordering::Release);
                owners[region_id] = Some(Owner {
                    pageserver_id,
                    process_nonce,
                    epoch,
                });
                let region = pool.header().layout.region(region_id as u32)?;
                DaemonResponse::Allocated(Allocation {
                    pool_uuid: pool.header().pool_uuid,
                    pool_epoch: pool.header().pool_epoch,
                    region_id: region_id as u32,
                    region_epoch: epoch,
                    region_offset: region.region_offset,
                    region_size: region.region_bytes,
                    slot_count: region.slot_count,
                })
            } else if let Some(region_id) = owners.iter().position(Option::is_none) {
                let descriptor = pool.region_descriptor(region_id as u32)?;
                let epoch = descriptor
                    .region_epoch
                    .load(Ordering::Acquire)
                    .checked_add(1)
                    .ok_or("region epoch exhausted")?;
                descriptor
                    .pageserver_id
                    .store(pageserver_id, Ordering::Relaxed);
                descriptor
                    .process_nonce
                    .store(process_nonce, Ordering::Relaxed);
                descriptor.region_epoch.store(epoch, Ordering::Relaxed);
                descriptor.state.store(REGION_ALLOCATED, Ordering::Release);
                owners[region_id] = Some(Owner {
                    pageserver_id,
                    process_nonce,
                    epoch,
                });
                let region = pool.header().layout.region(region_id as u32)?;
                DaemonResponse::Allocated(Allocation {
                    pool_uuid: pool.header().pool_uuid,
                    pool_epoch: pool.header().pool_epoch,
                    region_id: region_id as u32,
                    region_epoch: epoch,
                    region_offset: region.region_offset,
                    region_size: region.region_bytes,
                    slot_count: region.slot_count,
                })
            } else {
                DaemonResponse::Error("both 12 GiB Pageserver regions are allocated".into())
            }
        }
        DaemonRequest::Release {
            pageserver_id,
            process_nonce,
            region_id,
            region_epoch,
        } => match owners.get_mut(region_id as usize) {
            None => DaemonResponse::Error(format!("unknown region {region_id}")),
            Some(owner_slot) => {
                if let Some(owner) = *owner_slot {
                    if owner.pageserver_id != pageserver_id
                        || owner.process_nonce != process_nonce
                        || owner.epoch != region_epoch
                    {
                        DaemonResponse::Error("release owner or epoch mismatch".into())
                    } else {
                        let descriptor = pool.region_descriptor(region_id)?;
                        descriptor.state.store(REGION_FREE, Ordering::Release);
                        descriptor.pageserver_id.store(0, Ordering::Relaxed);
                        descriptor.process_nonce.store(0, Ordering::Relaxed);
                        *owner_slot = None;
                        DaemonResponse::Released
                    }
                } else {
                    DaemonResponse::Error(format!("region {region_id} is not allocated"))
                }
            }
        },
    };
    response.write_to(stream)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let pool_path = path_value(&args, "--pool-file", DEFAULT_POOL_PATH)?;
    let socket_path = path_value(&args, "--socket", DEFAULT_SOCKET_PATH)?;
    let pool_data_bytes = parse_size(&option_value(
        &args,
        "--pool-size",
        &DEFAULT_POOL_DATA_BYTES.to_string(),
    )?)?;
    let region_bytes = parse_size(&option_value(
        &args,
        "--region-size",
        &DEFAULT_REGION_BYTES.to_string(),
    )?)?;
    let layout = PoolLayout {
        pool_data_bytes,
        region_bytes,
        region_count: 2,
    }
    .validate()?;

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if Path::new(&socket_path).exists() {
        return Err(format!(
            "daemon socket {} already exists; remove it only after confirming no daemon is running",
            socket_path.display()
        )
        .into());
    }

    let pool = FilePool::create(&pool_path, layout, random_uuid()?, 1)?;
    let listener = UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))?;
    let mut owners = [None, None];

    println!(
        "READY pool={} file_bytes={} data_bytes={} regions=2 region_bytes={} slots_per_region={} socket={}",
        pool_path.display(),
        layout.file_bytes(),
        layout.pool_data_bytes,
        layout.region_bytes,
        layout.region(0)?.slot_count,
        socket_path.display()
    );

    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if let Err(error) = handle_connection(&mut stream, &pool, &mut owners) {
                    let _ = DaemonResponse::Error(error.to_string()).write_to(&mut stream);
                }
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}
