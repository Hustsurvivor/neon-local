use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use neon_cxl_cache::{
    DaemonClient, FilePool, MappingAccess, PAGE_SIZE, PageLocation, Result, validate_location,
    write_page,
};
use once_cell::sync::OnceCell;
use pageserver_api::pagestream_api::PagestreamSharedPageLocation;
use pageserver_api::reltag::{BlockNumber, RelTag};
use utils::id::{NodeId, TimelineId};
use utils::lsn::Lsn;
use utils::shard::TenantShardId;

const POOL_FILE_ENV: &str = "NEON_CXL_CACHE_POOL_FILE";
const DAEMON_SOCKET_ENV: &str = "NEON_CXL_CACHE_DAEMON_SOCKET";
const DEFAULT_DAEMON_SOCKET: &str = "/workspace/neon/.neon/cxl-cache/daemon.sock";

static MANAGER: OnceCell<Option<Arc<CxlCacheManager>>> = OnceCell::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey {
    tenant_shard_id: TenantShardId,
    timeline_id: TimelineId,
    rel: RelTag,
    blkno: BlockNumber,
    effective_lsn: Lsn,
}

impl CacheKey {
    pub const fn new(
        tenant_shard_id: TenantShardId,
        timeline_id: TimelineId,
        rel: RelTag,
        blkno: BlockNumber,
        effective_lsn: Lsn,
    ) -> Self {
        Self {
            tenant_shard_id,
            timeline_id,
            rel,
            blkno,
            effective_lsn,
        }
    }

    fn shared_hash(self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut hasher);
        hasher.finish()
    }
}

struct CacheState {
    index: HashMap<CacheKey, PageLocation>,
    reverse: HashMap<u64, CacheKey>,
    referenced: HashSet<u64>,
    next_unused: u64,
    clock_hand: u64,
}

pub struct CxlCacheManager {
    pool: FilePool,
    region_id: u32,
    region_epoch: u64,
    slot_count: u64,
    state: Mutex<CacheState>,
}

impl CxlCacheManager {
    fn initialize(node_id: NodeId, pool_file: &str, daemon_socket: &str) -> Result<Self> {
        let pool = FilePool::open(pool_file, MappingAccess::ReadWrite)?;
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64
            ^ u64::from(std::process::id());
        let allocation = DaemonClient::allocate(
            daemon_socket,
            node_id.0,
            pool.header().layout.region_bytes,
            nonce,
        )?;
        if allocation.pool_uuid != pool.header().pool_uuid
            || allocation.pool_epoch != pool.header().pool_epoch
        {
            return Err("daemon allocation belongs to a different pool generation".into());
        }

        Ok(Self {
            pool,
            region_id: allocation.region_id,
            region_epoch: allocation.region_epoch,
            slot_count: allocation.slot_count,
            state: Mutex::new(CacheState {
                index: HashMap::new(),
                reverse: HashMap::new(),
                referenced: HashSet::new(),
                next_unused: 0,
                clock_hand: 0,
            }),
        })
    }

    pub fn lookup(&self, key: &CacheKey) -> Option<PageLocation> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let location = state.index.get(key).copied()?;
        if validate_location(&self.pool, location).is_err() {
            state.index.remove(key);
            state.reverse.remove(&location.slot_id);
            state.referenced.remove(&location.slot_id);
            return None;
        }
        state.referenced.insert(location.slot_id);
        Some(location)
    }

    pub fn publish(&self, key: CacheKey, page: &Bytes) -> Option<PageLocation> {
        let page: &[u8; PAGE_SIZE as usize] = page.as_ref().try_into().ok()?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());

        if let Some(location) = state.index.get(&key).copied() {
            if validate_location(&self.pool, location).is_ok() {
                state.referenced.insert(location.slot_id);
                return Some(location);
            }
            state.index.remove(&key);
            state.reverse.remove(&location.slot_id);
            state.referenced.remove(&location.slot_id);
        }

        let slot_id = if state.next_unused < self.slot_count {
            let slot_id = state.next_unused;
            state.next_unused += 1;
            slot_id
        } else {
            loop {
                let candidate = state.clock_hand;
                state.clock_hand = (state.clock_hand + 1) % self.slot_count;
                if state.referenced.remove(&candidate) {
                    continue;
                }
                if let Some(victim) = state.reverse.remove(&candidate) {
                    state.index.remove(&victim);
                }
                break candidate;
            }
        };

        let location = write_page(
            &self.pool,
            self.region_id,
            self.region_epoch,
            slot_id,
            key.shared_hash(),
            key.effective_lsn.0,
            page,
        )
        .ok()?;
        state.reverse.insert(slot_id, key);
        state.index.insert(key, location);
        state.referenced.insert(slot_id);
        Some(location)
    }
}

pub fn init_from_env(node_id: NodeId) -> Result<()> {
    let Some(pool_file) = std::env::var_os(POOL_FILE_ENV) else {
        let _ = MANAGER.set(None);
        return Ok(());
    };
    let pool_file = pool_file
        .into_string()
        .map_err(|_| format!("{POOL_FILE_ENV} is not valid UTF-8"))?;
    let daemon_socket =
        std::env::var(DAEMON_SOCKET_ENV).unwrap_or_else(|_| DEFAULT_DAEMON_SOCKET.to_string());
    let manager = CxlCacheManager::initialize(node_id, &pool_file, &daemon_socket)?;
    MANAGER
        .set(Some(Arc::new(manager)))
        .map_err(|_| "CXL cache manager was already initialized".into())
}

pub fn manager() -> Option<&'static Arc<CxlCacheManager>> {
    MANAGER.get().and_then(Option::as_ref)
}

pub fn protocol_location(location: PageLocation) -> PagestreamSharedPageLocation {
    PagestreamSharedPageLocation {
        pool_uuid: location.pool_uuid,
        pool_epoch: location.pool_epoch,
        region_id: location.region_id,
        region_epoch: location.region_epoch,
        slot_id: location.slot_id,
        expected_control: location.expected_control,
        absolute_offset: location.absolute_offset,
        length: location.length,
        checksum_crc32c: location.checksum_crc32c,
    }
}
