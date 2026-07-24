use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::Result;

pub const PROTOCOL_VERSION: u16 = 1;
const MSG_ALLOCATE: u16 = 1;
const MSG_RELEASE: u16 = 2;
const MSG_ALLOCATED: u16 = 101;
const MSG_RELEASED: u16 = 102;
const MSG_ERROR: u16 = 199;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonRequest {
    Allocate {
        pageserver_id: u64,
        requested_size: u64,
        process_nonce: u64,
    },
    Release {
        pageserver_id: u64,
        process_nonce: u64,
        region_id: u32,
        region_epoch: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allocation {
    pub pool_uuid: [u8; 16],
    pub pool_epoch: u64,
    pub region_id: u32,
    pub region_epoch: u64,
    pub region_offset: u64,
    pub region_size: u64,
    pub slot_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonResponse {
    Allocated(Allocation),
    Released,
    Error(String),
}

fn write_frame(stream: &mut UnixStream, message_type: u16, payload: &[u8]) -> Result<()> {
    stream.write_all(&PROTOCOL_VERSION.to_le_bytes())?;
    stream.write_all(&message_type.to_le_bytes())?;
    stream.write_all(&(payload.len() as u32).to_le_bytes())?;
    stream.write_all(payload)?;
    Ok(())
}

fn read_frame(stream: &mut UnixStream) -> Result<(u16, Vec<u8>)> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;
    let version = u16::from_le_bytes(header[0..2].try_into().unwrap());
    if version != PROTOCOL_VERSION {
        return Err(format!("unsupported daemon protocol version {version}").into());
    }
    let message_type = u16::from_le_bytes(header[2..4].try_into().unwrap());
    let length = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
    if length > 4096 {
        return Err(format!("daemon frame is too large: {length}").into());
    }
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload)?;
    Ok((message_type, payload))
}

impl DaemonRequest {
    pub fn read_from(stream: &mut UnixStream) -> Result<Self> {
        let (message_type, payload) = read_frame(stream)?;
        match message_type {
            MSG_ALLOCATE if payload.len() == 24 => Ok(Self::Allocate {
                pageserver_id: u64::from_le_bytes(payload[0..8].try_into().unwrap()),
                requested_size: u64::from_le_bytes(payload[8..16].try_into().unwrap()),
                process_nonce: u64::from_le_bytes(payload[16..24].try_into().unwrap()),
            }),
            MSG_RELEASE if payload.len() == 32 => Ok(Self::Release {
                pageserver_id: u64::from_le_bytes(payload[0..8].try_into().unwrap()),
                process_nonce: u64::from_le_bytes(payload[8..16].try_into().unwrap()),
                region_id: u32::from_le_bytes(payload[16..20].try_into().unwrap()),
                region_epoch: u64::from_le_bytes(payload[24..32].try_into().unwrap()),
            }),
            _ => Err(format!(
                "invalid daemon request type {message_type} with {} bytes",
                payload.len()
            )
            .into()),
        }
    }

    pub fn write_to(self, stream: &mut UnixStream) -> Result<()> {
        let mut payload = Vec::with_capacity(32);
        let message_type = match self {
            Self::Allocate {
                pageserver_id,
                requested_size,
                process_nonce,
            } => {
                payload.extend_from_slice(&pageserver_id.to_le_bytes());
                payload.extend_from_slice(&requested_size.to_le_bytes());
                payload.extend_from_slice(&process_nonce.to_le_bytes());
                MSG_ALLOCATE
            }
            Self::Release {
                pageserver_id,
                process_nonce,
                region_id,
                region_epoch,
            } => {
                payload.extend_from_slice(&pageserver_id.to_le_bytes());
                payload.extend_from_slice(&process_nonce.to_le_bytes());
                payload.extend_from_slice(&region_id.to_le_bytes());
                payload.extend_from_slice(&0u32.to_le_bytes());
                payload.extend_from_slice(&region_epoch.to_le_bytes());
                MSG_RELEASE
            }
        };
        write_frame(stream, message_type, &payload)
    }
}

impl DaemonResponse {
    pub fn read_from(stream: &mut UnixStream) -> Result<Self> {
        let (message_type, payload) = read_frame(stream)?;
        match message_type {
            MSG_ALLOCATED if payload.len() == 68 => Ok(Self::Allocated(Allocation {
                pool_uuid: payload[0..16].try_into().unwrap(),
                pool_epoch: u64::from_le_bytes(payload[16..24].try_into().unwrap()),
                region_id: u32::from_le_bytes(payload[24..28].try_into().unwrap()),
                region_epoch: u64::from_le_bytes(payload[28..36].try_into().unwrap()),
                region_offset: u64::from_le_bytes(payload[36..44].try_into().unwrap()),
                region_size: u64::from_le_bytes(payload[44..52].try_into().unwrap()),
                slot_count: u64::from_le_bytes(payload[52..60].try_into().unwrap()),
            })),
            MSG_RELEASED if payload.is_empty() => Ok(Self::Released),
            MSG_ERROR => Ok(Self::Error(String::from_utf8(payload)?)),
            _ => Err(format!(
                "invalid daemon response type {message_type} with {} bytes",
                payload.len()
            )
            .into()),
        }
    }

    pub fn write_to(&self, stream: &mut UnixStream) -> Result<()> {
        match self {
            Self::Allocated(allocation) => {
                let mut payload = Vec::with_capacity(68);
                payload.extend_from_slice(&allocation.pool_uuid);
                payload.extend_from_slice(&allocation.pool_epoch.to_le_bytes());
                payload.extend_from_slice(&allocation.region_id.to_le_bytes());
                payload.extend_from_slice(&allocation.region_epoch.to_le_bytes());
                payload.extend_from_slice(&allocation.region_offset.to_le_bytes());
                payload.extend_from_slice(&allocation.region_size.to_le_bytes());
                payload.extend_from_slice(&allocation.slot_count.to_le_bytes());
                payload.extend_from_slice(&0u64.to_le_bytes());
                write_frame(stream, MSG_ALLOCATED, &payload)
            }
            Self::Released => write_frame(stream, MSG_RELEASED, &[]),
            Self::Error(message) => write_frame(stream, MSG_ERROR, message.as_bytes()),
        }
    }
}

pub struct DaemonClient;

impl DaemonClient {
    pub fn allocate(
        socket: impl AsRef<Path>,
        pageserver_id: u64,
        requested_size: u64,
        process_nonce: u64,
    ) -> Result<Allocation> {
        let mut stream = UnixStream::connect(socket)?;
        DaemonRequest::Allocate {
            pageserver_id,
            requested_size,
            process_nonce,
        }
        .write_to(&mut stream)?;
        match DaemonResponse::read_from(&mut stream)? {
            DaemonResponse::Allocated(allocation) => Ok(allocation),
            DaemonResponse::Error(message) => Err(message.into()),
            other => Err(format!("unexpected daemon response: {other:?}").into()),
        }
    }
}
