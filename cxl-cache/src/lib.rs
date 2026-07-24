#![cfg_attr(not(unix), allow(dead_code))]

mod abi;
mod backend;
mod cache;
pub mod cli;
mod crc32c;
mod daemon_protocol;

pub use abi::{
    CONTROL_BUSY, CONTROL_VALID, DEFAULT_POOL_DATA_BYTES, DEFAULT_REGION_BYTES,
    DEFAULT_REGION_COUNT, PAGE_SIZE, POOL_CONTROL_BYTES, PoolHeader, PoolLayout, REGION_ALLOCATED,
    REGION_FREE, RegionDescriptor, RegionLayout, SlotMeta, control_generation, control_is_busy,
    control_is_valid, make_control,
};
pub use backend::{FilePool, MappingAccess};
pub use cache::{PageLocation, ReadError, read_page, validate_location, write_page};
pub use crc32c::crc32c;
pub use daemon_protocol::{
    Allocation, DaemonClient, DaemonRequest, DaemonResponse, PROTOCOL_VERSION,
};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
