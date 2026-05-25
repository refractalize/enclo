use std::io;
use serde::{de::DeserializeOwned, Serialize};

pub fn encode<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Decode one newline-delimited JSON value from the buffer.
/// Returns (value, bytes_consumed) if a complete message is present.
pub fn decode_one<T: DeserializeOwned>(buf: &[u8]) -> io::Result<Option<(T, usize)>> {
    let Some(newline) = buf.iter().position(|&b| b == b'\n') else {
        return Ok(None);
    };
    let value = serde_json::from_slice(&buf[..newline])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some((value, newline + 1)))
}
