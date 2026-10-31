//! Minimal headless FILE_DATA_CHANNEL service.
//!
//! The implementation covers the read-directory RPC and safe basic operations
//! needed by the tools client. Transfers are bounded and paths are normalized
//! before any filesystem access. Desktop capture and input are unrelated to
//! this module.

use std::{path::PathBuf, sync::Arc, time::SystemTime};
use tokio::sync::mpsc;

const OK: u64 = 1;
const NOT_FOUND: u64 = 9;
const MAX_ENTRIES: usize = 2048;

#[derive(Clone, Default)]
pub struct FileHost {
    sequence: Arc<std::sync::atomic::AtomicU64>,
}

impl FileHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn handle(&self, bytes: &[u8], out: mpsc::UnboundedSender<Vec<u8>>) {
        let Some(request) = field_bytes(bytes, 21) else {
            return;
        };
        let Some(ftp) = field_bytes(request, 8) else {
            return;
        };
        let request_id = field_bytes(request, 1)
            .and_then(|header| field_varint(header, 1))
            .unwrap_or(0);
        let Some((case, body)) = first_message_field(ftp) else {
            return;
        };
        match case {
            10 => {
                let task = field_bytes(body, 1).unwrap_or_default().to_vec();
                let path =
                    String::from_utf8_lossy(field_bytes(body, 2).unwrap_or_default()).into_owned();
                let path_for_worker = path.clone();
                let entries = tokio::task::spawn_blocking(move || list_directory(&path_for_worker))
                    .await
                    .ok()
                    .flatten();
                let response = directory_response(request_id, &task, &path, entries);
                let sequence = self
                    .sequence
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let _ = out.send(envelope(sequence, 22, &response));
            }
            _ => {}
        }
    }
}

fn directory_response(
    request_id: u64,
    task: &[u8],
    path: &str,
    entries: Option<Vec<Vec<u8>>>,
) -> Vec<u8> {
    let mut directory = Vec::new();
    put_bytes(1, task, &mut directory);
    put_bytes(2, path.as_bytes(), &mut directory);
    match entries {
        Some(entries) => {
            for entry in entries {
                put_bytes(3, &entry, &mut directory);
            }
            put_varint(4, OK, &mut directory);
        }
        None => put_varint(4, NOT_FOUND, &mut directory),
    }
    let mut ftp = Vec::new();
    put_bytes(7, &directory, &mut ftp);
    let mut response = Vec::new();
    let mut header = Vec::new();
    put_varint(1, request_id, &mut header);
    put_bytes(1, &header, &mut response);
    put_bytes(4, &ftp, &mut response);
    response
}

fn list_directory(raw: &str) -> Option<Vec<Vec<u8>>> {
    let root = normalize(raw);
    let entries = std::fs::read_dir(&root).ok()?;
    let mut result = entries
        .filter_map(Result::ok)
        .take(MAX_ENTRIES)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let full = entry.path().to_string_lossy().into_owned();
            let kind = if metadata.is_dir() { 0 } else { 4 };
            let mut value = Vec::new();
            put_varint(1, kind, &mut value);
            put_bytes(2, name.as_bytes(), &mut value);
            if metadata.is_file() {
                put_varint(3, metadata.len(), &mut value);
            }
            put_varint(4, modified(&metadata), &mut value);
            put_bytes(5, full.as_bytes(), &mut value);
            Some((metadata.is_dir(), name.to_ascii_lowercase(), value))
        })
        .collect::<Vec<_>>();
    result.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    Some(result.into_iter().map(|(_, _, value)| value).collect())
}

fn normalize(raw: &str) -> PathBuf {
    let replaced = raw.replace('\\', "/");
    let expanded = if replaced == "~" {
        home()
    } else if let Some(rest) = replaced.strip_prefix("~/") {
        home().join(rest)
    } else if replaced.starts_with('/') {
        PathBuf::from(replaced)
    } else {
        PathBuf::from("/").join(replaced)
    };
    let mut clean = PathBuf::new();
    for part in expanded.components() {
        match part {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            component => clean.push(component.as_os_str()),
        }
    }
    if clean.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        clean
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}
fn modified(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|time| time.as_secs())
        .unwrap_or(0)
}

fn envelope(sequence: u64, kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    put_varint(1, sequence, &mut output);
    put_bytes(kind, payload, &mut output);
    output
}

fn first_message_field(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (key, used) = read_varint(&bytes[offset..])?;
        offset += used;
        let number = u32::try_from(key >> 3).ok()?;
        match key & 7 {
            0 => {
                let (_, used) = read_varint(&bytes[offset..])?;
                offset += used;
            }
            2 => {
                let (length, used) = read_varint(&bytes[offset..])?;
                offset += used;
                let length = usize::try_from(length).ok()?;
                let end = offset.checked_add(length)?;
                let value = bytes.get(offset..end)?;
                return Some((number, value));
            }
            _ => return None,
        }
    }
    None
}

fn field_bytes<'a>(bytes: &'a [u8], wanted: u32) -> Option<&'a [u8]> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (key, used) = read_varint(&bytes[offset..])?;
        offset += used;
        let number = u32::try_from(key >> 3).ok()?;
        match key & 7 {
            0 => {
                let (_, used) = read_varint(&bytes[offset..])?;
                offset += used;
            }
            2 => {
                let (length, used) = read_varint(&bytes[offset..])?;
                offset += used;
                let length = usize::try_from(length).ok()?;
                let end = offset.checked_add(length)?;
                let value = bytes.get(offset..end)?;
                offset = end;
                if number == wanted {
                    return Some(value);
                }
            }
            _ => return None,
        }
    }
    None
}

fn field_varint(bytes: &[u8], wanted: u32) -> Option<u64> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (key, used) = read_varint(&bytes[offset..])?;
        offset += used;
        let number = u32::try_from(key >> 3).ok()?;
        if key & 7 != 0 {
            if key & 7 == 2 {
                let (length, used) = read_varint(&bytes[offset..])?;
                offset = offset.checked_add(used + usize::try_from(length).ok()?)?;
                continue;
            }
            return None;
        }
        let (value, used) = read_varint(&bytes[offset..])?;
        offset += used;
        if number == wanted {
            return Some(value);
        }
    }
    None
}

fn put_varint(field: u32, value: u64, output: &mut Vec<u8>) {
    let mut key = u64::from(field) << 3;
    while key >= 0x80 {
        output.push((key as u8 & 0x7f) | 0x80);
        key >>= 7;
    }
    output.push(key as u8);
    let mut value = value;
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

fn put_bytes(field: u32, value: &[u8], output: &mut Vec<u8>) {
    let mut key = (u64::from(field) << 3) | 2;
    while key >= 0x80 {
        output.push((key as u8 & 0x7f) | 0x80);
        key >>= 7;
    }
    output.push(key as u8);
    let mut length = value.len() as u64;
    while length >= 0x80 {
        output.push((length as u8 & 0x7f) | 0x80);
        length >>= 7;
    }
    output.push(length as u8);
    output.extend_from_slice(value);
}

fn read_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0;
    for (index, byte) in bytes.iter().copied().enumerate().take(10) {
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, index + 1));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_home_and_parent_segments() {
        assert!(normalize("~/../").is_absolute());
        assert_eq!(normalize("/tmp/../var"), std::path::Path::new("/var"));
    }

    #[tokio::test]
    async fn returns_a_bounded_directory_rpc() {
        let host = FileHost::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut request = Vec::new();
        let mut header = Vec::new();
        put_varint(1, 4, &mut header);
        put_bytes(1, &header, &mut request);
        let mut ftp = Vec::new();
        put_bytes(1, b"task", &mut ftp);
        put_bytes(2, b"/tmp", &mut ftp);
        let mut request_case = Vec::new();
        put_bytes(10, &ftp, &mut request_case);
        put_bytes(8, &request_case, &mut request);
        let mut envelope = Vec::new();
        put_bytes(21, &request, &mut envelope);
        host.handle(&envelope, tx).await;
        let response = rx.recv().await.unwrap();
        assert!(field_bytes(&response, 22).is_some());
    }
}
