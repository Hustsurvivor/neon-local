use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use crate::Result;

pub const DEFAULT_POOL_PATH: &str = "/workspace/neon/.neon/cxl-cache/pool.bin";
pub const DEFAULT_SOCKET_PATH: &str = "/workspace/neon/.neon/cxl-cache/daemon.sock";

pub fn parse_size(value: &str) -> Result<u64> {
    let value = value.trim();
    let split_at = value
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u64 = value[..split_at].parse()?;
    let suffix = value[split_at..].trim().to_ascii_lowercase();
    let multiplier = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        _ => return Err(format!("unsupported size suffix in {value:?}").into()),
    };
    number
        .checked_mul(multiplier)
        .ok_or_else(|| "size overflow".into())
}

pub fn random_uuid() -> Result<[u8; 16]> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes)
}

pub fn option_value(args: &[String], name: &str, default: &str) -> Result<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter
                .next()
                .cloned()
                .ok_or_else(|| format!("missing value for {name}").into());
        }
    }
    Ok(default.to_string())
}

pub fn required_value(args: &[String], name: &str) -> Result<String> {
    let value = option_value(args, name, "")?;
    if value.is_empty() {
        Err(format!("required option {name} is missing").into())
    } else {
        Ok(value)
    }
}

pub fn path_value(args: &[String], name: &str, default: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(option_value(args, name, default)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_sizes() {
        assert_eq!(parse_size("24GiB").unwrap(), 24 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("12g").unwrap(), 12 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("8192").unwrap(), 8192);
    }
}
