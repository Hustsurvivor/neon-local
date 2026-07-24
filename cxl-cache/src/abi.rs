use std::mem::{align_of, size_of};
use std::sync::atomic::{AtomicU32, AtomicU64};

pub const KIB: u64 = 1024;
pub const MIB: u64 = 1024 * KIB;
pub const GIB: u64 = 1024 * MIB;

pub const PAGE_SIZE: u64 = 8 * KIB;
pub const SUPERBLOCK_BYTES: u64 = 4 * KIB;
pub const REGION_TABLE_BYTES: u64 = 4 * KIB;
pub const POOL_CONTROL_BYTES: u64 = SUPERBLOCK_BYTES + REGION_TABLE_BYTES;
pub const REGION_HEADER_BYTES: u64 = 4 * KIB;
pub const SLOT_META_BYTES: u64 = 64;
pub const REGION_DESCRIPTOR_BYTES: u64 = 64;

pub const DEFAULT_POOL_DATA_BYTES: u64 = 24 * GIB;
pub const DEFAULT_REGION_BYTES: u64 = 12 * GIB;
pub const DEFAULT_REGION_COUNT: u32 = 2;

pub const POOL_MAGIC: [u8; 8] = *b"NEONCXL\0";
pub const FORMAT_VERSION: u32 = 1;

pub const CONTROL_BUSY: u64 = 1;
pub const CONTROL_VALID: u64 = 2;
const CONTROL_GENERATION_SHIFT: u32 = 2;

pub const fn make_control(generation: u64, busy: bool, valid: bool) -> u64 {
    (generation << CONTROL_GENERATION_SHIFT)
        | if busy { CONTROL_BUSY } else { 0 }
        | if valid { CONTROL_VALID } else { 0 }
}

pub const fn control_generation(control: u64) -> u64 {
    control >> CONTROL_GENERATION_SHIFT
}

pub const fn control_is_busy(control: u64) -> bool {
    control & CONTROL_BUSY != 0
}

pub const fn control_is_valid(control: u64) -> bool {
    control & CONTROL_VALID != 0
}

pub const fn align_up(value: u64, alignment: u64) -> u64 {
    value.div_ceil(alignment) * alignment
}

#[repr(C, align(64))]
pub struct SlotMeta {
    pub control: AtomicU64,
    pub checksum: AtomicU32,
    pub length: AtomicU32,
    pub key_hash: AtomicU64,
    pub page_lsn: AtomicU64,
    reserved: [u8; 32],
}

const _: () = assert!(size_of::<SlotMeta>() == SLOT_META_BYTES as usize);
const _: () = assert!(align_of::<SlotMeta>() == SLOT_META_BYTES as usize);

pub const REGION_FREE: u32 = 0;
pub const REGION_ALLOCATED: u32 = 1;

#[repr(C, align(64))]
pub struct RegionDescriptor {
    pub state: AtomicU32,
    pub region_id: u32,
    pub pageserver_id: AtomicU64,
    pub process_nonce: AtomicU64,
    pub region_epoch: AtomicU64,
    pub region_offset: u64,
    pub region_size: u64,
    pub slot_count: u64,
    reserved: [u8; 8],
}

impl RegionDescriptor {
    pub fn new(layout: RegionLayout) -> Self {
        Self {
            state: AtomicU32::new(REGION_FREE),
            region_id: layout.region_id,
            pageserver_id: AtomicU64::new(0),
            process_nonce: AtomicU64::new(0),
            region_epoch: AtomicU64::new(0),
            region_offset: layout.region_offset,
            region_size: layout.region_bytes,
            slot_count: layout.slot_count,
            reserved: [0; 8],
        }
    }
}

const _: () = assert!(size_of::<RegionDescriptor>() == REGION_DESCRIPTOR_BYTES as usize);
const _: () = assert!(align_of::<RegionDescriptor>() == REGION_DESCRIPTOR_BYTES as usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolLayout {
    pub pool_data_bytes: u64,
    pub region_bytes: u64,
    pub region_count: u32,
}

impl Default for PoolLayout {
    fn default() -> Self {
        Self {
            pool_data_bytes: DEFAULT_POOL_DATA_BYTES,
            region_bytes: DEFAULT_REGION_BYTES,
            region_count: DEFAULT_REGION_COUNT,
        }
    }
}

impl PoolLayout {
    pub fn validate(self) -> Result<Self, String> {
        if self.region_count != DEFAULT_REGION_COUNT {
            return Err(format!(
                "V1 supports exactly {} regions, got {}",
                DEFAULT_REGION_COUNT, self.region_count
            ));
        }
        if self.pool_data_bytes != self.region_bytes * u64::from(self.region_count) {
            return Err(format!(
                "pool data size {} must equal region size {} × region count {}",
                self.pool_data_bytes, self.region_bytes, self.region_count
            ));
        }
        if self.pool_data_bytes % PAGE_SIZE != 0 || self.region_bytes % PAGE_SIZE != 0 {
            return Err("pool and region sizes must be multiples of 8 KiB".into());
        }
        if self.region_bytes <= REGION_HEADER_BYTES + PAGE_SIZE + SLOT_META_BYTES {
            return Err("region is too small to contain a slot".into());
        }
        Ok(self)
    }

    pub const fn file_bytes(self) -> u64 {
        POOL_CONTROL_BYTES + self.pool_data_bytes
    }

    pub fn region_descriptor_offset(self, region_id: u32) -> Result<u64, String> {
        if region_id >= self.region_count {
            return Err(format!(
                "region {} is out of range 0..{}",
                region_id, self.region_count
            ));
        }
        Ok(SUPERBLOCK_BYTES + u64::from(region_id) * REGION_DESCRIPTOR_BYTES)
    }

    pub fn region(self, region_id: u32) -> Result<RegionLayout, String> {
        if region_id >= self.region_count {
            return Err(format!(
                "region {} is out of range 0..{}",
                region_id, self.region_count
            ));
        }

        let region_offset = POOL_CONTROL_BYTES + u64::from(region_id) * self.region_bytes;
        let mut slot_count =
            (self.region_bytes - REGION_HEADER_BYTES) / (SLOT_META_BYTES + PAGE_SIZE);

        loop {
            let meta_offset = region_offset + REGION_HEADER_BYTES;
            let data_offset = align_up(meta_offset + slot_count * SLOT_META_BYTES, PAGE_SIZE);
            let region_end = region_offset + self.region_bytes;
            if data_offset + slot_count * PAGE_SIZE <= region_end {
                return Ok(RegionLayout {
                    region_id,
                    region_offset,
                    region_bytes: self.region_bytes,
                    meta_offset,
                    data_offset,
                    slot_count,
                });
            }
            slot_count = slot_count
                .checked_sub(1)
                .ok_or_else(|| "region cannot fit a slot".to_string())?;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegionLayout {
    pub region_id: u32,
    pub region_offset: u64,
    pub region_bytes: u64,
    pub meta_offset: u64,
    pub data_offset: u64,
    pub slot_count: u64,
}

impl RegionLayout {
    pub fn slot_meta_offset(self, slot_id: u64) -> Result<u64, String> {
        if slot_id >= self.slot_count {
            return Err(format!(
                "slot {} is out of range 0..{}",
                slot_id, self.slot_count
            ));
        }
        Ok(self.meta_offset + slot_id * SLOT_META_BYTES)
    }

    pub fn slot_data_offset(self, slot_id: u64) -> Result<u64, String> {
        if slot_id >= self.slot_count {
            return Err(format!(
                "slot {} is out of range 0..{}",
                slot_id, self.slot_count
            ));
        }
        Ok(self.data_offset + slot_id * PAGE_SIZE)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolHeader {
    pub pool_uuid: [u8; 16],
    pub pool_epoch: u64,
    pub layout: PoolLayout,
}

impl PoolHeader {
    pub fn encode(self) -> [u8; SUPERBLOCK_BYTES as usize] {
        let mut bytes = [0u8; SUPERBLOCK_BYTES as usize];
        bytes[0..8].copy_from_slice(&POOL_MAGIC);
        bytes[8..12].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&(SUPERBLOCK_BYTES as u32).to_le_bytes());
        bytes[16..32].copy_from_slice(&self.pool_uuid);
        bytes[32..40].copy_from_slice(&self.pool_epoch.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.layout.pool_data_bytes.to_le_bytes());
        bytes[48..56].copy_from_slice(&self.layout.region_bytes.to_le_bytes());
        bytes[56..60].copy_from_slice(&self.layout.region_count.to_le_bytes());
        bytes[60..64].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
        bytes[64..68].copy_from_slice(&(SLOT_META_BYTES as u32).to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < SUPERBLOCK_BYTES as usize {
            return Err("pool file is smaller than the superblock".into());
        }
        if bytes[0..8] != POOL_MAGIC {
            return Err("invalid pool magic".into());
        }
        let format_version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if format_version != FORMAT_VERSION {
            return Err(format!(
                "unsupported format version {format_version}, expected {FORMAT_VERSION}"
            ));
        }
        let page_size = u32::from_le_bytes(bytes[60..64].try_into().unwrap());
        let slot_meta_size = u32::from_le_bytes(bytes[64..68].try_into().unwrap());
        if u64::from(page_size) != PAGE_SIZE || u64::from(slot_meta_size) != SLOT_META_BYTES {
            return Err("pool ABI sizes do not match this build".into());
        }

        let layout = PoolLayout {
            pool_data_bytes: u64::from_le_bytes(bytes[40..48].try_into().unwrap()),
            region_bytes: u64::from_le_bytes(bytes[48..56].try_into().unwrap()),
            region_count: u32::from_le_bytes(bytes[56..60].try_into().unwrap()),
        }
        .validate()?;

        Ok(Self {
            pool_uuid: bytes[16..32].try_into().unwrap(),
            pool_epoch: u64::from_le_bytes(bytes[32..40].try_into().unwrap()),
            layout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_has_two_twelve_gib_regions() {
        let layout = PoolLayout::default().validate().unwrap();
        assert_eq!(layout.pool_data_bytes, 24 * GIB);
        assert_eq!(layout.region_bytes, 12 * GIB);
        assert_eq!(layout.region_count, 2);
        assert_eq!(layout.file_bytes(), 24 * GIB + 8 * KIB);

        let first = layout.region(0).unwrap();
        let second = layout.region(1).unwrap();
        assert_eq!(first.region_bytes, 12 * GIB);
        assert_eq!(second.region_bytes, 12 * GIB);
        assert_eq!(
            first.region_offset + first.region_bytes,
            second.region_offset
        );
        assert!(first.slot_count > 1_000_000);
        assert!(second.slot_count > 1_000_000);
    }

    #[test]
    fn header_round_trip() {
        let header = PoolHeader {
            pool_uuid: [7; 16],
            pool_epoch: 42,
            layout: PoolLayout::default(),
        };
        assert_eq!(PoolHeader::decode(&header.encode()).unwrap(), header);
    }

    #[test]
    fn control_encodes_state_and_generation() {
        let control = make_control(99, false, true);
        assert_eq!(control_generation(control), 99);
        assert!(!control_is_busy(control));
        assert!(control_is_valid(control));
    }
}
