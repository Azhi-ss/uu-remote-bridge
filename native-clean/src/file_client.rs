//! Viewer-side FILE_DATA_CHANNEL client.
//!
//! This module owns the request/response matching and the bounded block loops
//! for directory listing, upload and download. It uses the same protobuf
//! shapes as `file_host` but has no dependency on generated protocol code.

use crate::peer::Peer;
use flate2::read::ZlibDecoder;
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

const FILE_CHANNEL: usize = 3;
const BLOCK_SIZE: usize = 32 * 1024;
const MAX_BLOCK: usize = 256 * 1024;
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;
const OK: u64 = 1;
const MAX_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;
static NEXT_TASK: AtomicU64 = AtomicU64::new(100);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1 << 20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub kind: u64,
    pub name: String,
    pub size: u64,
    pub modified: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileInfo {
    rel_path: String,
    size: u64,
    modified: u64,
}

#[derive(Debug, Clone, Copy)]
struct TaskId {
    task: u64,
    index: u64,
}

pub async fn list(peer: &mut Peer, path: &str) -> Result<Vec<FileEntry>, String> {
    let id = TaskId {
        task: next_task(),
        index: 0,
    };
    let mut body = Vec::new();
    put_bytes(1, &task_id(id), &mut body);
    put_bytes(2, path.as_bytes(), &mut body);
    let ftp = request(peer, 10, body, 7).await?;
    check(field_varint(&ftp, 4).unwrap_or(0))?;
    let compressed = field_bytes(&ftp, 5);
    let entries: Vec<Vec<u8>> = if let Some(compressed) = compressed {
        let mut inflated = Vec::new();
        ZlibDecoder::new(compressed)
            .take(MAX_LIST_BYTES as u64 + 1)
            .read_to_end(&mut inflated)
            .map_err(|_| "invalid compressed directory listing")?;
        if inflated.len() > MAX_LIST_BYTES {
            return Err("directory listing exceeds 16 MiB".into());
        }
        all_field_bytes(&inflated, 1)
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
    } else {
        all_field_bytes(&ftp, 3)
            .into_iter()
            .map(ToOwned::to_owned)
            .collect()
    };
    entries
        .into_iter()
        .map(|entry| {
            Ok(FileEntry {
                kind: field_varint(&entry, 1).unwrap_or(0),
                name: text_field(&entry, 2),
                size: field_varint(&entry, 3).unwrap_or(0),
                modified: field_varint(&entry, 4).unwrap_or(0),
                path: text_field(&entry, 5),
            })
        })
        .collect()
}

pub async fn upload(peer: &mut Peer, local: &str, remote: &str) -> Result<(), String> {
    let local_path = Path::new(local);
    let (folder, directory) = if local_path.is_dir() {
        let folder = local_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or("upload directory has no name")?
            .to_owned();
        (folder, remote.trim_end_matches('/').to_owned())
    } else {
        let (directory, name) = split_remote(remote).ok_or("remote path has no file name")?;
        (String::new(), format!("{directory}\0{name}"))
    };
    let (directory, single_name) = if let Some((directory, name)) = directory.split_once('\0') {
        (directory.to_owned(), Some(name.to_owned()))
    } else {
        (directory, None)
    };
    let files = collect_upload(local_path, single_name.as_deref())?;
    let total = files
        .iter()
        .try_fold(0u64, |sum, file| sum.checked_add(file.0.size))
        .ok_or("upload size overflow")?;
    if total > MAX_DOWNLOAD_BYTES {
        return Err("upload exceeds 8 GiB safety limit".into());
    }
    let task = TaskId {
        task: next_task(),
        index: 0,
    };
    let mut receive = Vec::new();
    put_bytes(1, &task_id(task), &mut receive);
    put_bytes(2, directory.as_bytes(), &mut receive);
    for (info, _) in &files {
        put_bytes(3, &file_info(info), &mut receive);
    }
    put_varint(4, 2, &mut receive); // duplicate strategy
    if !folder.is_empty() {
        put_bytes(5, folder.as_bytes(), &mut receive);
    }
    put_bytes(
        6,
        &file_list(&files.iter().map(|file| file.0.clone()).collect::<Vec<_>>()),
        &mut receive,
    );
    let response = request(peer, 1, receive, 1).await?;
    check(field_varint(&response, 3).unwrap_or(0))?;
    for (index, (info, path)) in files.iter().enumerate() {
        let id = TaskId {
            task: task.task,
            index: index as u64 + 1,
        };
        let confirm = request(
            peer,
            3,
            {
                let mut ask = Vec::new();
                put_bytes(1, &task_id(id), &mut ask);
                put_varint(3, info.size, &mut ask);
                ask
            },
            3,
        )
        .await?;
        check(field_varint(&confirm, 3).unwrap_or(0))?;
        if field_varint(&confirm, 2).unwrap_or(0) != 0 {
            continue;
        }
        let resume = field_varint(&confirm, 4).unwrap_or(0).min(info.size);
        let mut file = std::fs::File::open(path)
            .map_err(|error| format!("cannot open upload file: {error}"))?;
        file.seek(SeekFrom::Start(resume))
            .map_err(|error| format!("cannot seek upload file: {error}"))?;
        let mut block = resume / BLOCK_SIZE as u64;
        let mut sent = resume;
        loop {
            let mut data = vec![0; BLOCK_SIZE];
            let read = file
                .read(&mut data)
                .map_err(|error| format!("upload read failed: {error}"))?;
            if read == 0 {
                break;
            }
            data.truncate(read);
            let mut block_body = Vec::new();
            put_bytes(1, &task_id(id), &mut block_body);
            put_varint(2, block, &mut block_body);
            put_bytes(3, &data, &mut block_body);
            let ack = request(peer, 4, block_body, 4).await?;
            check(field_varint(&ack, 3).unwrap_or(0))?;
            if field_varint(&ack, 2).unwrap_or(0) != block
                || field_varint(&ack, 4).unwrap_or(0) != read as u64
            {
                return Err(format!("invalid upload block acknowledgement: block={:?} bytes={:?} expected={block}/{read}", field_varint(&ack, 2), field_varint(&ack, 4)));
            }
            sent += read as u64;
            block += 1;
        }
        if sent != info.size {
            return Err("upload source changed while reading".into());
        }
        let mut done = Vec::new();
        put_bytes(1, &task_id(id), &mut done);
        put_varint(2, OK, &mut done);
        let result = request(peer, 9, done, 6).await?;
        check(field_varint(&result, 2).unwrap_or(0))?;
    }
    Ok(())
}

pub async fn download(peer: &mut Peer, remote: &str, local: &str) -> Result<(), String> {
    let task = TaskId {
        task: next_task(),
        index: 0,
    };
    let mut body = Vec::new();
    put_bytes(1, &task_id(task), &mut body);
    put_bytes(2, remote.as_bytes(), &mut body);
    let response = request(peer, 2, body, 2).await?;
    check(field_varint(&response, 2).unwrap_or(0))?;
    let folder = text_field(&response, 4);
    let infos = if let Some(compressed) = field_bytes(&response, 5) {
        let mut inflated = Vec::new();
        ZlibDecoder::new(compressed)
            .take(MAX_LIST_BYTES as u64 + 1)
            .read_to_end(&mut inflated)
            .map_err(|_| "invalid compressed file list")?;
        all_field_bytes(&inflated, 1)
            .into_iter()
            .filter_map(parse_file_info)
            .collect::<Vec<_>>()
    } else {
        all_field_bytes(&response, 3)
            .into_iter()
            .filter_map(parse_file_info)
            .collect::<Vec<_>>()
    };
    if folder.is_empty() && infos.len() != 1 {
        return Err("remote download without a folder must contain exactly one file".into());
    }
    if infos.is_empty()
        || infos
            .iter()
            .try_fold(0u64, |sum, info| sum.checked_add(info.size))
            .unwrap_or(MAX_DOWNLOAD_BYTES + 1)
            > MAX_DOWNLOAD_BYTES
    {
        return Err("download list is empty or exceeds 8 GiB".into());
    }
    let target = PathBuf::from(local);
    let stage = target.with_file_name(format!(".uu-download-{}", crate::wire::uuid()));
    let _cleanup = DownloadStage(stage.clone());
    std::fs::create_dir_all(&stage)
        .map_err(|error| format!("cannot create download staging directory: {error}"))?;
    let mut open: HashMap<u64, (std::fs::File, PathBuf, u64)> = HashMap::new();
    let mut completed = 0usize;
    let mut finished_paths: HashMap<u64, PathBuf> = HashMap::new();
    let result = loop {
        let message = next_request(peer).await?;
        let rpc = field_bytes(&message, 21).ok_or("invalid host file request")?;
        let ftp = field_bytes(rpc, 8).ok_or("invalid host file request body")?;
        let request_id = field_bytes(rpc, 1)
            .and_then(|header| field_varint(header, 1))
            .unwrap_or(0);
        let (case_number, request_body) =
            first_message_field(ftp).ok_or("invalid host file case")?;
        let id = field_bytes(request_body, 1)
            .map(TaskId::parse)
            .ok_or("host file request has no task id")?;
        if id.task != task.task {
            return Err(format!(
                "host requested a different download task: got={} expected={}",
                id.task, task.task
            ));
        }
        match case_number {
            3 => {
                let index = id
                    .index
                    .checked_sub(1)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or("invalid file index")?;
                let info = infos.get(index).ok_or("host file index out of range")?;
                let destination =
                    safe_join(&stage, &info.rel_path).ok_or("unsafe relative download path")?;
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|_| "cannot create download directory")?;
                }
                let file = std::fs::File::create(&destination)
                    .map_err(|_| "cannot create downloaded file")?;
                open.insert(id.index, (file, destination, 0));
                let mut reply = Vec::new();
                put_bytes(1, &task_id(id), &mut reply);
                put_varint(2, 0, &mut reply);
                put_varint(3, OK, &mut reply);
                put_varint(4, 0, &mut reply);
                send_response(peer, request_id, 3, reply).await?;
            }
            4 => {
                let block = field_varint(request_body, 2).unwrap_or(0);
                let data = field_bytes(request_body, 3).unwrap_or_default();
                let entry = open
                    .get_mut(&id.index)
                    .ok_or("block arrived before file confirmation")?;
                if data.len() > MAX_BLOCK {
                    return Err("download block exceeds 256 KiB".into());
                }
                let expected = infos
                    .get(id.index.saturating_sub(1) as usize)
                    .ok_or("invalid download index")?
                    .size;
                if entry.2.saturating_add(data.len() as u64) > expected {
                    return Err("download exceeds declared file size".into());
                }
                entry
                    .0
                    .write_all(data)
                    .map_err(|_| "cannot write downloaded block")?;
                entry.2 = entry.2.saturating_add(data.len() as u64);
                let mut reply = Vec::new();
                put_bytes(1, &task_id(id), &mut reply);
                put_varint(2, block, &mut reply);
                put_varint(3, OK, &mut reply);
                put_varint(4, data.len() as u64, &mut reply);
                send_response(peer, request_id, 4, reply).await?;
            }
            9 => {
                if id.index == u64::MAX {
                    let code = field_varint(request_body, 2).unwrap_or(0);
                    if code != OK || !open.is_empty() || completed != infos.len() {
                        return Err("host ended an incomplete download".into());
                    }
                    if !folder.is_empty() {
                        if target.exists() {
                            return Err("download destination already exists".into());
                        }
                        std::fs::rename(&stage, &target)
                            .map_err(|_| "cannot finalize downloaded directory")?;
                    } else {
                        let path = finished_paths.get(&1).ok_or("download has no first file")?;
                        if target.exists() {
                            std::fs::remove_file(&target)
                                .map_err(|_| "cannot replace download destination")?;
                        }
                        std::fs::rename(path, &target)
                            .map_err(|_| "cannot finalize downloaded file")?;
                        let _ = std::fs::remove_dir_all(&stage);
                    }
                    break Ok(());
                }
                check(field_varint(request_body, 2).unwrap_or(0))?;
                let expected = id
                    .index
                    .checked_sub(1)
                    .and_then(|index| usize::try_from(index).ok())
                    .and_then(|index| infos.get(index))
                    .map(|info| info.size)
                    .ok_or("invalid completion index")?;
                let (_, path, written) = open
                    .remove(&id.index)
                    .ok_or("file completion without open file")?;
                if written != expected {
                    return Err("downloaded file size does not match metadata".into());
                }
                completed += 1;
                finished_paths.insert(id.index, path);
                let mut reply = Vec::new();
                put_bytes(1, &task_id(id), &mut reply);
                put_varint(2, OK, &mut reply);
                send_response(peer, request_id, 6, reply).await?;
            }
            _ => return Err(format!("unsupported host file request case {case_number}")),
        }
    };
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result
}

async fn request(
    peer: &mut Peer,
    case_number: u32,
    body: Vec<u8>,
    response_case: u32,
) -> Result<Vec<u8>, String> {
    let request_id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    let mut ftp = Vec::new();
    put_bytes(case_number, &body, &mut ftp);
    let mut rpc = Vec::new();
    let mut header = Vec::new();
    put_varint(1, request_id, &mut header);
    put_bytes(1, &header, &mut rpc);
    put_bytes(8, &ftp, &mut rpc);
    let mut envelope = Vec::new();
    put_bytes(21, &rpc, &mut envelope);
    peer.send(0, envelope).await?;
    loop {
        let message = next_file(peer).await?;
        let response = field_bytes(&message, 22).ok_or("invalid file response")?;
        let response_id = field_bytes(response, 1)
            .and_then(|header| field_varint(header, 1))
            .unwrap_or(0);
        if response_id != request_id {
            continue;
        }
        let ftp = field_bytes(response, 4).ok_or("file response has no FTP body")?;
        let (case, body) = first_message_field(ftp).ok_or("file response has no case")?;
        if case != response_case {
            return Err(format!(
                "expected file response case {response_case}, got {case}"
            ));
        }
        return Ok(body.to_vec());
    }
}

async fn next_file(peer: &mut Peer) -> Result<Vec<u8>, String> {
    loop {
        let (channel, bytes) = peer.recv().await?;
        if channel == FILE_CHANNEL {
            return Ok(bytes);
        }
    }
}
async fn next_request(peer: &mut Peer) -> Result<Vec<u8>, String> {
    loop {
        let bytes = next_file(peer).await?;
        if field_bytes(&bytes, 21).is_some() {
            return Ok(bytes);
        }
    }
}

async fn send_response(
    peer: &mut Peer,
    request_id: u64,
    case_number: u32,
    body: Vec<u8>,
) -> Result<(), String> {
    let mut ftp = Vec::new();
    put_bytes(case_number, &body, &mut ftp);
    let mut response = Vec::new();
    let mut header = Vec::new();
    put_varint(1, request_id, &mut header);
    put_bytes(1, &header, &mut response);
    put_bytes(4, &ftp, &mut response);
    let mut envelope = Vec::new();
    put_bytes(22, &response, &mut envelope);
    peer.send(0, envelope).await
}

fn check(code: u64) -> Result<(), String> {
    if code == OK {
        Ok(())
    } else {
        Err(format!("file transfer error code {code}"))
    }
}
fn next_task() -> u64 {
    (crate::terminal::now_nanos() as u64 & 0x3fff_ffff) + NEXT_TASK.fetch_add(1, Ordering::Relaxed)
}
fn task_id(id: TaskId) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(1, id.task, &mut body);
    put_varint(2, id.index, &mut body);
    body
}
impl TaskId {
    fn parse(bytes: &[u8]) -> Self {
        Self {
            task: field_varint(bytes, 1).unwrap_or(0),
            index: field_varint(bytes, 2).unwrap_or(0),
        }
    }
}
fn parse_file_info(bytes: &[u8]) -> Option<FileInfo> {
    Some(FileInfo {
        rel_path: text_field(bytes, 1),
        size: field_varint(bytes, 2).unwrap_or(0),
        modified: field_varint(bytes, 3).unwrap_or(0),
    })
}

fn file_info(info: &FileInfo) -> Vec<u8> {
    let mut body = Vec::new();
    put_bytes(1, info.rel_path.as_bytes(), &mut body);
    put_varint(2, info.size, &mut body);
    put_varint(3, info.modified, &mut body);
    body
}

fn file_list(files: &[FileInfo]) -> Vec<u8> {
    let mut plain = Vec::new();
    for file in files {
        put_bytes(1, &file_info(file), &mut plain);
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    if encoder.write_all(&plain).is_err() {
        return Vec::new();
    }
    encoder.finish().unwrap_or_default()
}

fn collect_upload(
    root: &Path,
    single_name: Option<&str>,
) -> Result<Vec<(FileInfo, PathBuf)>, String> {
    let metadata =
        std::fs::metadata(root).map_err(|error| format!("cannot stat upload source: {error}"))?;
    if metadata.is_file() {
        let name = single_name.ok_or("remote path has no file name")?;
        return Ok(vec![(
            FileInfo {
                rel_path: name.to_owned(),
                size: metadata.len(),
                modified: modified(&metadata),
            },
            root.to_path_buf(),
        )]);
    }
    if !metadata.is_dir() {
        return Err("upload source must be a file or directory".into());
    }
    let mut result = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .map_err(|error| format!("cannot read upload directory: {error}"))?
            .flatten()
        {
            let path = entry.path();
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            result.push((
                FileInfo {
                    rel_path: relative,
                    size: metadata.len(),
                    modified: modified(&metadata),
                },
                path,
            ));
            if result.len() > 4096 {
                return Err("upload directory contains more than 4096 files".into());
            }
        }
    }
    Ok(result)
}

fn modified(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|time| time.as_secs())
        .unwrap_or(0)
}
fn split_remote(remote: &str) -> Option<(String, String)> {
    let clean = remote.replace('\\', "/");
    let index = clean.rfind('/');
    let (dir, name) = match index {
        Some(index) if index > 0 => (clean[..index].to_owned(), clean[index + 1..].to_owned()),
        Some(_) => ("/".into(), clean[1..].to_owned()),
        None => ("~/".into(), clean),
    };
    if name.is_empty() || name == "." || name == ".." {
        None
    } else {
        Some((dir, name))
    }
}
fn safe_join(base: &Path, relative: &str) -> Option<PathBuf> {
    let mut path = base.to_path_buf();
    for part in relative.replace('\\', "/").split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') {
            return None;
        }
        path.push(part);
    }
    Some(path)
}
fn text_field(bytes: &[u8], wanted: u32) -> String {
    String::from_utf8_lossy(field_bytes(bytes, wanted).unwrap_or_default()).into_owned()
}
fn all_field_bytes<'a>(bytes: &'a [u8], wanted: u32) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let Some((key, used)) = read_varint(&bytes[offset..]) else {
            break;
        };
        offset += used;
        let field = (key >> 3) as u32;
        match key & 7 {
            0 => {
                let Some((_, used)) = read_varint(&bytes[offset..]) else {
                    break;
                };
                offset += used;
            }
            2 => {
                let Some((len, used)) = read_varint(&bytes[offset..]) else {
                    break;
                };
                offset += used;
                let Ok(len) = usize::try_from(len) else { break };
                let Some(end) = offset.checked_add(len) else {
                    break;
                };
                let Some(value) = bytes.get(offset..end) else {
                    break;
                };
                if field == wanted {
                    out.push(value);
                }
                offset = end;
            }
            _ => break,
        }
    }
    out
}
fn field_bytes<'a>(bytes: &'a [u8], wanted: u32) -> Option<&'a [u8]> {
    all_field_bytes(bytes, wanted).into_iter().next()
}
fn field_varint(bytes: &[u8], wanted: u32) -> Option<u64> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (key, used) = read_varint(&bytes[offset..])?;
        offset += used;
        let field = (key >> 3) as u32;
        match key & 7 {
            0 => {
                let (value, used) = read_varint(&bytes[offset..])?;
                offset += used;
                if field == wanted {
                    return Some(value);
                }
            }
            2 => {
                let (len, used) = read_varint(&bytes[offset..])?;
                offset = offset.checked_add(used + usize::try_from(len).ok()?)?;
            }
            _ => return None,
        }
    }
    None
}
fn first_message_field(bytes: &[u8]) -> Option<(u32, &[u8])> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (key, used) = read_varint(&bytes[offset..])?;
        offset += used;
        let field = (key >> 3) as u32;
        if key & 7 == 2 {
            let (len, used) = read_varint(&bytes[offset..])?;
            offset += used;
            let end = offset.checked_add(usize::try_from(len).ok()?)?;
            return Some((field, bytes.get(offset..end)?));
        }
        if key & 7 == 0 {
            let (_, used) = read_varint(&bytes[offset..])?;
            offset += used;
        } else {
            return None;
        }
    }
    None
}
fn put_varint(field: u32, value: u64, out: &mut Vec<u8>) {
    varint(u64::from(field) << 3, out);
    varint(value, out);
}
fn put_bytes(field: u32, value: &[u8], out: &mut Vec<u8>) {
    varint((u64::from(field) << 3) | 2, out);
    varint(value.len() as u64, out);
    out.extend_from_slice(value);
}
fn varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
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
    fn remote_paths_are_split_without_traversal() {
        assert_eq!(
            split_remote("C:\\Users\\Public\\x.bin"),
            Some(("C:/Users/Public".into(), "x.bin".into()))
        );
        assert_eq!(split_remote("x.bin"), Some(("~/".into(), "x.bin".into())));
        assert!(safe_join(Path::new("/tmp"), "../x").is_none());
    }
}

pub(crate) fn is_file_message(bytes: &[u8]) -> bool {
    field_bytes(bytes, 22)
        .and_then(|rpc| field_bytes(rpc, 4))
        .is_some()
        || field_bytes(bytes, 21)
            .and_then(|rpc| field_bytes(rpc, 8))
            .is_some()
}

pub async fn remove(peer: &mut Peer, path: &str) -> Result<(), String> {
    let mut body = Vec::new();
    put_bytes(
        1,
        &task_id(TaskId {
            task: next_task(),
            index: 0,
        }),
        &mut body,
    );
    put_bytes(2, path.as_bytes(), &mut body);
    let response = request(peer, 8, body, 6).await?;
    let code = field_varint(&response, 2).unwrap_or(0);
    if code != OK {
        // Some official hosts report a failure after deleting the entry.
        // Confirm the resulting directory state before reporting success.
        if let Some((parent, name)) = split_remote(path) {
            if !list(peer, &parent)
                .await?
                .iter()
                .any(|entry| entry.name == name)
            {
                return Ok(());
            }
        }
    }
    check(code)
}

struct DownloadStage(PathBuf);
impl Drop for DownloadStage {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
