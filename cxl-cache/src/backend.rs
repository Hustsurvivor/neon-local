#[cfg(not(unix))]
compile_error!("neon-cxl-cache currently supports Unix hosts only");

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

use crate::Result;
use crate::abi::{PoolHeader, PoolLayout, RegionDescriptor, SUPERBLOCK_BYTES};

const PROT_READ: i32 = 0x1;
const PROT_WRITE: i32 = 0x2;
const MAP_SHARED: i32 = 0x01;

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        length: usize,
        prot: i32,
        flags: i32,
        fd: i32,
        offset: isize,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, length: usize) -> i32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MappingAccess {
    ReadOnly,
    ReadWrite,
}

pub struct FilePool {
    path: PathBuf,
    file: File,
    base: NonNull<u8>,
    len: usize,
    access: MappingAccess,
    header: PoolHeader,
}

unsafe impl Send for FilePool {}
unsafe impl Sync for FilePool {}

impl FilePool {
    pub fn create(
        path: impl AsRef<Path>,
        layout: PoolLayout,
        pool_uuid: [u8; 16],
        pool_epoch: u64,
    ) -> Result<Self> {
        let layout = layout.validate()?;
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(path)?;
        file.set_len(layout.file_bytes())?;
        file.set_permissions(std::fs::Permissions::from_mode(0o660))?;

        let header = PoolHeader {
            pool_uuid,
            pool_epoch,
            layout,
        };
        file.write_all(&header.encode())?;
        file.flush()?;
        drop(file);

        let pool = Self::open(path, MappingAccess::ReadWrite)?;
        for region_id in 0..layout.region_count {
            let descriptor_offset = layout.region_descriptor_offset(region_id)?;
            let descriptor = RegionDescriptor::new(layout.region(region_id)?);
            let descriptor_ptr = pool.checked_ptr::<RegionDescriptor>(descriptor_offset)?;
            unsafe { descriptor_ptr.write(descriptor) };
        }
        Ok(pool)
    }

    pub fn open(path: impl AsRef<Path>, access: MappingAccess) -> Result<Self> {
        let path = path.as_ref();
        let mut options = OpenOptions::new();
        options.read(true);
        if access == MappingAccess::ReadWrite {
            options.write(true);
        }
        let mut file = options.open(path)?;
        let file_len = file.metadata()?.len();
        let len = usize::try_from(file_len)
            .map_err(|_| format!("pool file {file_len} does not fit in address space"))?;

        let mut header_bytes = [0u8; SUPERBLOCK_BYTES as usize];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header_bytes)?;
        let header = PoolHeader::decode(&header_bytes)?;
        if file_len != header.layout.file_bytes() {
            return Err(format!(
                "pool file length {file_len} differs from header length {}",
                header.layout.file_bytes()
            )
            .into());
        }

        let protection = match access {
            MappingAccess::ReadOnly => PROT_READ,
            MappingAccess::ReadWrite => PROT_READ | PROT_WRITE,
        };
        let raw = unsafe {
            mmap(
                std::ptr::null_mut(),
                len,
                protection,
                MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if raw as isize == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        let base = NonNull::new(raw.cast::<u8>())
            .ok_or_else(|| "mmap unexpectedly returned null".to_string())?;

        Ok(Self {
            path: path.to_path_buf(),
            file,
            base,
            len,
            access,
            header,
        })
    }

    pub const fn header(&self) -> PoolHeader {
        self.header
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn access(&self) -> MappingAccess {
        self.access
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub(crate) fn checked_ptr<T>(&self, offset: u64) -> Result<*mut T> {
        let offset = usize::try_from(offset).map_err(|_| "offset does not fit usize")?;
        let end = offset
            .checked_add(std::mem::size_of::<T>())
            .ok_or("offset overflow")?;
        if end > self.len {
            return Err(format!("mapping access {offset}..{end} exceeds {}", self.len).into());
        }
        let ptr = unsafe { self.base.as_ptr().add(offset).cast::<T>() };
        if (ptr as usize) % std::mem::align_of::<T>() != 0 {
            return Err(format!(
                "offset {offset} is not aligned to {}",
                std::mem::align_of::<T>()
            )
            .into());
        }
        Ok(ptr)
    }

    pub fn region_descriptor(&self, region_id: u32) -> Result<&RegionDescriptor> {
        let offset = self.header.layout.region_descriptor_offset(region_id)?;
        let ptr = self.checked_ptr::<RegionDescriptor>(offset)?;
        Ok(unsafe { &*ptr.cast_const() })
    }
}

impl Drop for FilePool {
    fn drop(&mut self) {
        let rc = unsafe { munmap(self.base.as_ptr().cast::<c_void>(), self.len) };
        debug_assert_eq!(rc, 0, "munmap failed: {}", std::io::Error::last_os_error());
    }
}
