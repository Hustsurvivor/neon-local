use std::sync::atomic::Ordering;

use crate::abi::{
    PAGE_SIZE, REGION_ALLOCATED, SlotMeta, control_generation, control_is_busy, control_is_valid,
    make_control,
};
use crate::{FilePool, MappingAccess, Result, crc32c};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageLocation {
    pub pool_uuid: [u8; 16],
    pub pool_epoch: u64,
    pub region_id: u32,
    pub region_epoch: u64,
    pub slot_id: u64,
    pub expected_control: u64,
    pub absolute_offset: u64,
    pub length: u32,
    pub checksum_crc32c: u32,
}

impl PageLocation {
    pub const ENCODED_SIZE: usize = 68;

    pub fn encode(self) -> [u8; Self::ENCODED_SIZE] {
        let mut bytes = [0u8; Self::ENCODED_SIZE];
        bytes[0..16].copy_from_slice(&self.pool_uuid);
        bytes[16..24].copy_from_slice(&self.pool_epoch.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.region_id.to_le_bytes());
        bytes[28..36].copy_from_slice(&self.region_epoch.to_le_bytes());
        bytes[36..44].copy_from_slice(&self.slot_id.to_le_bytes());
        bytes[44..52].copy_from_slice(&self.expected_control.to_le_bytes());
        bytes[52..60].copy_from_slice(&self.absolute_offset.to_le_bytes());
        bytes[60..64].copy_from_slice(&self.length.to_le_bytes());
        bytes[64..68].copy_from_slice(&self.checksum_crc32c.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != Self::ENCODED_SIZE {
            return Err(format!(
                "PageLocation requires {} bytes, got {}",
                Self::ENCODED_SIZE,
                bytes.len()
            )
            .into());
        }
        Ok(Self {
            pool_uuid: bytes[0..16].try_into().unwrap(),
            pool_epoch: u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            region_id: u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            region_epoch: u64::from_le_bytes(bytes[28..36].try_into().unwrap()),
            slot_id: u64::from_le_bytes(bytes[36..44].try_into().unwrap()),
            expected_control: u64::from_le_bytes(bytes[44..52].try_into().unwrap()),
            absolute_offset: u64::from_le_bytes(bytes[52..60].try_into().unwrap()),
            length: u32::from_le_bytes(bytes[60..64].try_into().unwrap()),
            checksum_crc32c: u32::from_le_bytes(bytes[64..68].try_into().unwrap()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadError {
    PoolIdentity,
    Region,
    Bounds,
    Stale,
    Busy,
    Invalid,
    Length,
    Changed,
    Checksum,
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for ReadError {}

fn slot_meta(pool: &FilePool, region_id: u32, slot_id: u64) -> Result<&SlotMeta> {
    let region = pool.header().layout.region(region_id)?;
    let offset = region.slot_meta_offset(slot_id)?;
    let ptr = pool.checked_ptr::<SlotMeta>(offset)?;
    Ok(unsafe { &*ptr.cast_const() })
}

fn page_words(pool: &FilePool, absolute_offset: u64) -> Result<*mut std::sync::atomic::AtomicU64> {
    pool.checked_ptr::<std::sync::atomic::AtomicU64>(absolute_offset)
}

pub fn validate_location(
    pool: &FilePool,
    location: PageLocation,
) -> std::result::Result<(), ReadError> {
    let header = pool.header();
    if header.pool_uuid != location.pool_uuid || header.pool_epoch != location.pool_epoch {
        return Err(ReadError::PoolIdentity);
    }
    if location.length != PAGE_SIZE as u32 {
        return Err(ReadError::Length);
    }
    let region = header
        .layout
        .region(location.region_id)
        .map_err(|_| ReadError::Region)?;
    let descriptor = pool
        .region_descriptor(location.region_id)
        .map_err(|_| ReadError::Region)?;
    if descriptor.state.load(Ordering::Acquire) != REGION_ALLOCATED
        || descriptor.region_epoch.load(Ordering::Acquire) != location.region_epoch
    {
        return Err(ReadError::Region);
    }
    let expected_offset = region
        .slot_data_offset(location.slot_id)
        .map_err(|_| ReadError::Bounds)?;
    if expected_offset != location.absolute_offset {
        return Err(ReadError::Bounds);
    }
    let meta =
        slot_meta(pool, location.region_id, location.slot_id).map_err(|_| ReadError::Bounds)?;
    let control = meta.control.load(Ordering::Acquire);
    if control != location.expected_control {
        return Err(ReadError::Stale);
    }
    if control_is_busy(control) {
        return Err(ReadError::Busy);
    }
    if !control_is_valid(control) {
        return Err(ReadError::Invalid);
    }
    Ok(())
}

pub fn write_page(
    pool: &FilePool,
    region_id: u32,
    region_epoch: u64,
    slot_id: u64,
    key_hash: u64,
    page_lsn: u64,
    page: &[u8; PAGE_SIZE as usize],
) -> Result<PageLocation> {
    if pool.access() != MappingAccess::ReadWrite {
        return Err("cannot publish through a read-only pool mapping".into());
    }
    let region = pool.header().layout.region(region_id)?;
    let descriptor = pool.region_descriptor(region_id)?;
    if descriptor.state.load(Ordering::Acquire) != REGION_ALLOCATED
        || descriptor.region_epoch.load(Ordering::Acquire) != region_epoch
    {
        return Err("cannot publish through an unallocated or stale region".into());
    }
    let meta = slot_meta(pool, region_id, slot_id)?;
    let old = meta.control.load(Ordering::Acquire);
    let generation = control_generation(old)
        .checked_add(1)
        .ok_or("slot generation exhausted")?;
    let writing = make_control(generation, true, false);
    meta.control.store(writing, Ordering::Release);

    let data_offset = region.slot_data_offset(slot_id)?;
    let words = page_words(pool, data_offset)?;
    for (index, chunk) in page.chunks_exact(8).enumerate() {
        let value = u64::from_ne_bytes(chunk.try_into().unwrap());
        unsafe { &*words.add(index) }.store(value, Ordering::Relaxed);
    }

    let checksum = crc32c(page);
    meta.checksum.store(checksum, Ordering::Relaxed);
    meta.length.store(PAGE_SIZE as u32, Ordering::Relaxed);
    meta.key_hash.store(key_hash, Ordering::Relaxed);
    meta.page_lsn.store(page_lsn, Ordering::Relaxed);

    let published = make_control(generation, false, true);
    meta.control.store(published, Ordering::Release);

    Ok(PageLocation {
        pool_uuid: pool.header().pool_uuid,
        pool_epoch: pool.header().pool_epoch,
        region_id,
        region_epoch,
        slot_id,
        expected_control: published,
        absolute_offset: data_offset,
        length: PAGE_SIZE as u32,
        checksum_crc32c: checksum,
    })
}

pub fn read_page(
    pool: &FilePool,
    location: PageLocation,
) -> std::result::Result<[u8; PAGE_SIZE as usize], ReadError> {
    validate_location(pool, location)?;
    let meta =
        slot_meta(pool, location.region_id, location.slot_id).map_err(|_| ReadError::Bounds)?;
    let before = meta.control.load(Ordering::Acquire);
    if before != location.expected_control {
        return Err(ReadError::Stale);
    }
    if control_is_busy(before) {
        return Err(ReadError::Busy);
    }
    if !control_is_valid(before) {
        return Err(ReadError::Invalid);
    }

    let words = page_words(pool, location.absolute_offset).map_err(|_| ReadError::Bounds)?;
    let mut page = [0u8; PAGE_SIZE as usize];
    for (index, chunk) in page.chunks_exact_mut(8).enumerate() {
        let value = unsafe { &*words.add(index) }.load(Ordering::Relaxed);
        chunk.copy_from_slice(&value.to_ne_bytes());
    }

    let after = meta.control.load(Ordering::Acquire);
    if after != before {
        return Err(ReadError::Changed);
    }
    if crc32c(&page) != location.checksum_crc32c {
        return Err(ReadError::Checksum);
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use crate::abi::{GIB, PoolHeader, PoolLayout};

    #[test]
    fn pool_layout_type_is_used() {
        let layout = PoolLayout {
            pool_data_bytes: 2 * GIB,
            region_bytes: GIB,
            region_count: 2,
        };
        let header = PoolHeader {
            pool_uuid: [0; 16],
            pool_epoch: 1,
            layout,
        };
        assert_eq!(header.layout.region_count, 2);
    }
}
