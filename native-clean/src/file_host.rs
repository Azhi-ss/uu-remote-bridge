//! FILE_DATA_CHANNEL implementation.
//!
//! The wire format is the small protobuf RPC used by UU Remote. This module
//! keeps its own decoder/encoder so the clean-room client does not depend on
//! generated code or the GPL reference implementation.

use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use std::{
    collections::HashMap,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};
use tokio::sync::{mpsc, oneshot};

const FT_SUCCEEDED: u64 = 1;
const FT_PROTOCOL: u64 = 3;
const FT_PERMISSION: u64 = 4;
const FT_IO: u64 = 6;
const FT_CANCELED: u64 = 7;
const FT_NOT_FOUND: u64 = 9;
const FT_DIR: u64 = 0;
const FT_DIR_DRIVE: u64 = 3;
const FT_FILE: u64 = 4;
const FOS_DUPLICATE: u64 = 2;
const FOS_SKIP: u64 = 3;
const FOS_RESUME: u64 = 4;
const BLOCK_SIZE: usize = 32 * 1024;
const MAX_BLOCK: usize = 256 * 1024;
const MAX_ENTRIES: usize = 2048;
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

#[derive(Clone)]
pub struct FileHost {
    state: Arc<State>,
}

struct State {
    sequence: std::sync::atomic::AtomicU64,
    uploads: Mutex<HashMap<u64, UploadTask>>,
    pending: Mutex<HashMap<TransferKey, oneshot::Sender<Vec<u8>>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TransferKey {
    case_number: u32,
    task: u64,
    index: u64,
    block: u64,
}

struct UploadTask {
    target: PathBuf,
    folder: String,
    strategy: u64,
    files: Vec<FileInfo>,
    open: HashMap<u64, OpenUpload>,
}

struct OpenUpload {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileInfo {
    rel_path: String,
    size: u64,
    modified: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TaskId {
    task: u64,
    index: u64,
}

impl FileHost {
    pub fn new() -> Self {
        Self {
            state: Arc::new(State {
                sequence: std::sync::atomic::AtomicU64::new(1),
                uploads: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Consume one complete protobuf envelope from FILE_DATA_CHANNEL.
    pub async fn handle(&self, bytes: &[u8], out: mpsc::UnboundedSender<Vec<u8>>) {
        if let Some(response) = field_bytes(bytes, 22) {
            self.handle_response(response);
        }
        let Some(request) = field_bytes(bytes, 21) else {
            return;
        };
        let request_id = field_bytes(request, 1)
            .and_then(|header| field_varint(header, 1))
            .unwrap_or(0);
        let Some(ftp) = field_bytes(request, 8) else {
            return;
        };
        let Some((case_number, body)) = first_message_field(ftp) else {
            return;
        };
        match case_number {
            1 => self.receive_request(request_id, body, &out),
            2 => self.handle_send_request(request_id, body, out).await,
            3 => self.file_ask(request_id, body, &out),
            4 => self.file_block(request_id, body, &out),
            5..=8 => self.file_operation(request_id, case_number, body, &out),
            9 => self.complete(request_id, body, &out),
            10 => self.read_dir(request_id, body, &out),
            11 => self.file_exist(request_id, body, &out),
            12 => self.respond(request_id, 5, result_body(FT_SUCCEEDED, ""), &out),
            _ => self.respond(
                request_id,
                5,
                result_body(FT_PROTOCOL, "unsupported request"),
                &out,
            ),
        }
    }

    fn handle_response(&self, response: &[u8]) {
        let Some(ftp) = field_bytes(response, 4) else {
            return;
        };
        let Some((case_number, body)) = first_message_field(ftp) else {
            return;
        };
        let id = field_bytes(body, 1).map(TaskId::parse).unwrap_or_default();
        let block = if case_number == 4 {
            field_varint(body, 2).unwrap_or(0)
        } else {
            0
        };
        let key = TransferKey {
            case_number,
            task: id.task,
            index: id.index,
            block,
        };
        if let Some(sender) = self
            .state
            .pending
            .lock()
            .expect("pending mutex")
            .remove(&key)
        {
            let _ = sender.send(ftp.to_vec());
        }
    }

    fn respond(
        &self,
        request_id: u64,
        case_number: u32,
        body: Vec<u8>,
        out: &mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let mut ftp = Vec::new();
        put_bytes(case_number, &body, &mut ftp);
        let mut response = Vec::new();
        let mut header = Vec::new();
        put_varint(1, request_id, &mut header);
        put_bytes(1, &header, &mut response);
        put_bytes(4, &ftp, &mut response);
        let sequence = self
            .state
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = out.send(envelope(sequence, 22, &response));
    }

    fn read_dir(&self, request_id: u64, body: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
        let task = field_bytes(body, 1).unwrap_or_default();
        let path = text_field(body, 2);
        let mut directory = Vec::new();
        put_bytes(1, task, &mut directory);
        put_bytes(2, path.as_bytes(), &mut directory);
        match list_directory(&path) {
            Ok(entries) => {
                let mut listing = Vec::new();
                for entry in &entries {
                    put_bytes(1, entry, &mut listing);
                }
                let compressed = compress(&listing);
                if listing.len() + compressed.len() < 200_000 {
                    for entry in entries {
                        put_bytes(3, &entry, &mut directory);
                    }
                }
                put_varint(4, FT_SUCCEEDED, &mut directory);
                put_bytes(5, &compressed, &mut directory);
            }
            Err(code) => put_varint(4, code, &mut directory),
        }
        self.respond(request_id, 7, directory, out);
    }

    fn receive_request(&self, request_id: u64, body: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
        let id = field_bytes(body, 1).map(TaskId::parse).unwrap_or_default();
        let target = normalize(&text_field(body, 2));
        let folder = text_field(body, 5);
        let strategy = field_varint(body, 4).unwrap_or(FOS_DUPLICATE);
        let files = parse_files(all_field_bytes(body, 3), field_bytes(body, 6));
        let code = if target.is_dir() {
            if folder.is_empty() {
                FT_SUCCEEDED
            } else if let Some(folder_path) = join_relative(&target, &folder) {
                std::fs::create_dir_all(folder_path)
                    .map(|_| FT_SUCCEEDED)
                    .unwrap_or(FT_PERMISSION)
            } else {
                FT_PROTOCOL
            }
        } else {
            FT_NOT_FOUND
        };
        if code == FT_SUCCEEDED {
            self.state.uploads.lock().expect("uploads mutex").insert(
                id.task,
                UploadTask {
                    target,
                    folder,
                    strategy,
                    files: files.clone(),
                    open: HashMap::new(),
                },
            );
        }
        let mut response = Vec::new();
        put_bytes(1, &task_id(id), &mut response);
        for file in files {
            put_bytes(2, &file_info(&file), &mut response);
        }
        put_varint(3, code, &mut response);
        self.respond(request_id, 1, response, out);
    }

    fn file_ask(&self, request_id: u64, body: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
        let id = field_bytes(body, 1).map(TaskId::parse).unwrap_or_default();
        let size = field_varint(body, 3).unwrap_or(0);
        let mut skip = false;
        let mut code = FT_SUCCEEDED;
        let mut resume = 0;
        let mut uploads = self.state.uploads.lock().expect("uploads mutex");
        match uploads.get_mut(&id.task) {
            Some(task) => match upload_path(task, id.index) {
                Some(mut path) => {
                    let existing = std::fs::metadata(&path).ok().filter(|meta| meta.is_file());
                    if existing.is_some() && task.strategy == FOS_SKIP {
                        skip = true;
                    } else {
                        if task.strategy == FOS_RESUME {
                            resume = existing.map(|meta| meta.len()).unwrap_or(0).min(size);
                        } else if existing.is_some() && task.strategy == FOS_DUPLICATE {
                            path = unique_path(&path);
                        }
                        let result = path
                            .parent()
                            .map(std::fs::create_dir_all)
                            .unwrap_or_else(|| Ok(()))
                            .and_then(|_| {
                                let mut options = std::fs::OpenOptions::new();
                                options.create(true).write(true);
                                if resume == 0 {
                                    options.truncate(true);
                                }
                                let mut file = options.open(&path)?;
                                file.seek(SeekFrom::Start(resume))?;
                                Ok(file)
                            });
                        match result {
                            Ok(file) => {
                                task.open.insert(
                                    id.index,
                                    OpenUpload {
                                        file,
                                        path,
                                        written: resume,
                                    },
                                );
                            }
                            Err(error) => code = io_code(&error),
                        }
                    }
                }
                None => code = FT_PROTOCOL,
            },
            None => code = FT_PROTOCOL,
        }
        drop(uploads);
        let mut response = Vec::new();
        put_bytes(1, &task_id(id), &mut response);
        put_varint(2, u64::from(skip), &mut response);
        put_varint(3, code, &mut response);
        put_varint(4, resume, &mut response);
        self.respond(request_id, 3, response, out);
    }

    fn file_block(&self, request_id: u64, body: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
        let id = field_bytes(body, 1).map(TaskId::parse).unwrap_or_default();
        let block = field_varint(body, 2).unwrap_or(0);
        let data = field_bytes(body, 3).unwrap_or_default();
        let mut code = FT_SUCCEEDED;
        if data.len() > MAX_BLOCK {
            code = FT_PROTOCOL;
        } else {
            let mut uploads = self.state.uploads.lock().expect("uploads mutex");
            match uploads
                .get_mut(&id.task)
                .and_then(|task| task.open.get_mut(&id.index))
            {
                Some(open) => {
                    if open.written.saturating_add(data.len() as u64) > MAX_FILE_BYTES {
                        code = FT_IO;
                    } else if let Err(error) = open.file.write_all(data) {
                        code = io_code(&error);
                    } else {
                        open.written += data.len() as u64;
                    }
                }
                None => code = FT_PROTOCOL,
            }
        }
        let mut response = Vec::new();
        put_bytes(1, &task_id(id), &mut response);
        put_varint(2, block, &mut response);
        put_varint(3, code, &mut response);
        put_varint(4, data.len() as u64, &mut response);
        self.respond(request_id, 4, response, out);
    }

    fn complete(&self, request_id: u64, body: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
        let id = field_bytes(body, 1).map(TaskId::parse).unwrap_or_default();
        let viewer_error = field_varint(body, 2).unwrap_or(FT_SUCCEEDED);
        let mut code = FT_SUCCEEDED;
        let mut uploads = self.state.uploads.lock().expect("uploads mutex");
        if let Some(task) = uploads.get_mut(&id.task) {
            if let Some(open) = task.open.remove(&id.index) {
                let expected = id
                    .checked_index()
                    .and_then(|index| task.files.get(index))
                    .map(|file| file.size)
                    .unwrap_or(open.written);
                if viewer_error != 0 && viewer_error != FT_SUCCEEDED {
                    let _ = std::fs::remove_file(&open.path);
                    code = viewer_error;
                } else if open.written != expected {
                    code = FT_IO;
                }
            }
            if task.open.is_empty() && id.index as usize >= task.files.len() {
                uploads.remove(&id.task);
            }
        } else {
            code = FT_PROTOCOL;
        }
        let mut response = Vec::new();
        put_bytes(1, &task_id(id), &mut response);
        put_varint(2, code, &mut response);
        self.respond(request_id, 6, response, out);
    }

    async fn handle_send_request(
        &self,
        request_id: u64,
        body: &[u8],
        out: mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let id = field_bytes(body, 1).map(TaskId::parse).unwrap_or_default();
        let path = normalize(&text_field(body, 2));
        let result = collect_download(&path);
        let mut response = Vec::new();
        put_bytes(1, &task_id(id), &mut response);
        match result {
            Ok((folder, files)) => {
                put_varint(2, FT_SUCCEEDED, &mut response);
                let infos = files.iter().map(|item| item.0.clone()).collect::<Vec<_>>();
                for file in &infos {
                    put_bytes(3, &file_info(file), &mut response);
                }
                put_bytes(4, folder.as_bytes(), &mut response);
                put_bytes(5, &file_list(&infos), &mut response);
                self.respond(request_id, 2, response, &out);
                let host = self.clone();
                tokio::spawn(async move {
                    let _ = host.download_files(id.task, files, out).await;
                });
            }
            Err(code) => {
                put_varint(2, code, &mut response);
                self.respond(request_id, 2, response, &out);
            }
        }
    }

    async fn download_files(
        &self,
        task: u64,
        files: Vec<(FileInfo, PathBuf)>,
        out: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<(), u64> {
        for (index, (info, path)) in files.iter().enumerate() {
            let index = index as u64 + 1;
            let id = TaskId { task, index };
            let mut ask = Vec::new();
            put_bytes(1, &task_id(id), &mut ask);
            put_varint(2, info.modified, &mut ask);
            put_varint(3, info.size, &mut ask);
            let confirm = self
                .request(
                    3,
                    ask,
                    TransferKey {
                        case_number: 3,
                        task,
                        index,
                        block: 0,
                    },
                    &out,
                )
                .await
                .ok_or(FT_CANCELED)?;
            let Some((_, confirm_body)) = first_message_field(&confirm) else {
                return Err(FT_PROTOCOL);
            };
            let code = field_varint(confirm_body, 3).unwrap_or(FT_PROTOCOL);
            if code != FT_SUCCEEDED {
                return Err(code);
            }
            if field_varint(confirm_body, 2).unwrap_or(0) != 0 {
                continue;
            }
            let resume = field_varint(confirm_body, 4).unwrap_or(0).min(info.size);
            let mut file = std::fs::File::open(path).map_err(|error| io_code(&error))?;
            file.seek(SeekFrom::Start(resume))
                .map_err(|error| io_code(&error))?;
            let mut block = resume / BLOCK_SIZE as u64;
            loop {
                let mut chunk = vec![0; BLOCK_SIZE];
                let read = file.read(&mut chunk).map_err(|error| io_code(&error))?;
                if read == 0 {
                    break;
                }
                chunk.truncate(read);
                let mut request = Vec::new();
                put_bytes(1, &task_id(id), &mut request);
                put_varint(2, block, &mut request);
                put_bytes(3, &chunk, &mut request);
                let ack = self
                    .request(
                        4,
                        request,
                        TransferKey {
                            case_number: 4,
                            task,
                            index,
                            block,
                        },
                        &out,
                    )
                    .await
                    .ok_or(FT_CANCELED)?;
                let Some((_, ack_body)) = first_message_field(&ack) else {
                    return Err(FT_PROTOCOL);
                };
                let ack_code = field_varint(ack_body, 3).unwrap_or(FT_PROTOCOL);
                if ack_code != FT_SUCCEEDED {
                    return Err(ack_code);
                }
                block += 1;
            }
            let mut done = Vec::new();
            put_bytes(1, &task_id(id), &mut done);
            put_varint(2, FT_SUCCEEDED, &mut done);
            let reply = self
                .request(
                    9,
                    done,
                    TransferKey {
                        case_number: 6,
                        task,
                        index,
                        block: 0,
                    },
                    &out,
                )
                .await
                .ok_or(FT_CANCELED)?;
            let Some((_, reply_body)) = first_message_field(&reply) else {
                return Err(FT_PROTOCOL);
            };
            let reply_code = field_varint(reply_body, 2).unwrap_or(FT_PROTOCOL);
            if reply_code != FT_SUCCEEDED {
                return Err(reply_code);
            }
        }
        let id = TaskId {
            task,
            index: u64::MAX,
        };
        let mut body = Vec::new();
        put_bytes(1, &task_id(id), &mut body);
        put_varint(2, FT_SUCCEEDED, &mut body);
        self.send_ftp_request(9, body, &out);
        Ok(())
    }

    fn send_ftp_request(
        &self,
        case_number: u32,
        body: Vec<u8>,
        out: &mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let mut ftp = Vec::new();
        put_bytes(case_number, &body, &mut ftp);
        let mut request = Vec::new();
        let mut header = Vec::new();
        let request_id = self
            .state
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + (1 << 32);
        put_varint(1, request_id, &mut header);
        put_bytes(1, &header, &mut request);
        put_bytes(8, &ftp, &mut request);
        let sequence = self
            .state
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _ = out.send(envelope(sequence, 21, &request));
    }

    async fn request(
        &self,
        case_number: u32,
        body: Vec<u8>,
        key: TransferKey,
        out: &mpsc::UnboundedSender<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        let (sender, receiver) = oneshot::channel();
        self.state
            .pending
            .lock()
            .expect("pending mutex")
            .insert(key, sender);
        let mut ftp = Vec::new();
        put_bytes(case_number, &body, &mut ftp);
        let mut request = Vec::new();
        let mut header = Vec::new();
        let request_id = self
            .state
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + (1 << 32);
        put_varint(1, request_id, &mut header);
        put_bytes(1, &header, &mut request);
        put_bytes(8, &ftp, &mut request);
        let sequence = self
            .state
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if out.send(envelope(sequence, 21, &request)).is_err() {
            self.state
                .pending
                .lock()
                .expect("pending mutex")
                .remove(&key);
            return None;
        }
        match tokio::time::timeout(std::time::Duration::from_secs(30), receiver).await {
            Ok(Ok(value)) => Some(value),
            _ => {
                self.state
                    .pending
                    .lock()
                    .expect("pending mutex")
                    .remove(&key);
                None
            }
        }
    }

    fn file_operation(
        &self,
        request_id: u64,
        case_number: u32,
        body: &[u8],
        out: &mpsc::UnboundedSender<Vec<u8>>,
    ) {
        let (raw, path) = if case_number == 8 {
            let raw = text_field(body, 2);
            (raw.clone(), normalize(&raw))
        } else {
            let raw = text_field(body, 1);
            (raw.clone(), normalize(&raw))
        };
        let result = match case_number {
            5 => {
                let name = text_field(body, 2);
                if safe_component(&name) {
                    std::fs::rename(&path, path.with_file_name(name))
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "unsafe name",
                    ))
                }
            }
            6 => std::fs::create_dir_all(&path),
            7 => std::fs::remove_file(&path),
            8 => match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
                Ok(_) => std::fs::remove_file(&path),
                Err(error) => Err(error),
            },
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsupported",
            )),
        };
        let code = result
            .as_ref()
            .map(|_| FT_SUCCEEDED)
            .unwrap_or_else(|error| io_code(error));
        if case_number == 8 {
            let id = field_bytes(body, 1).map(TaskId::parse).unwrap_or_default();
            let mut response = Vec::new();
            put_bytes(1, &task_id(id), &mut response);
            put_varint(2, code, &mut response);
            self.respond(request_id, 6, response, out);
        } else {
            self.respond(request_id, 5, result_body(code, &raw), out);
        }
    }

    fn file_exist(&self, request_id: u64, body: &[u8], out: &mpsc::UnboundedSender<Vec<u8>>) {
        let raw = text_field(body, 1);
        let directory = normalize(&raw);
        let mut response = Vec::new();
        put_bytes(1, raw.as_bytes(), &mut response);
        for name in all_field_bytes(body, 2) {
            let name = String::from_utf8_lossy(name);
            let mut item = Vec::new();
            put_bytes(1, name.as_bytes(), &mut item);
            put_varint(
                2,
                u64::from(directory.join(name.as_ref()).exists()),
                &mut item,
            );
            put_varint(3, 0, &mut item);
            put_bytes(2, &item, &mut response);
        }
        self.respond(request_id, 8, response, out);
    }
}

impl TaskId {
    fn parse(bytes: &[u8]) -> Self {
        Self {
            task: field_varint(bytes, 1).unwrap_or(0),
            index: field_varint(bytes, 2).unwrap_or(0),
        }
    }
    fn checked_index(self) -> Option<usize> {
        self.index
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
    }
}

fn result_body(code: u64, message: &str) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(1, code, &mut body);
    if !message.is_empty() {
        put_bytes(2, message.as_bytes(), &mut body);
    }
    body
}

fn task_id(id: TaskId) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(1, id.task, &mut body);
    put_varint(2, id.index, &mut body);
    body
}

fn file_info(file: &FileInfo) -> Vec<u8> {
    let mut body = Vec::new();
    put_bytes(1, file.rel_path.as_bytes(), &mut body);
    put_varint(2, file.size, &mut body);
    put_varint(3, file.modified, &mut body);
    body
}

fn parse_file_info(bytes: &[u8]) -> Option<FileInfo> {
    Some(FileInfo {
        rel_path: text_field(bytes, 1),
        size: field_varint(bytes, 2).unwrap_or(0),
        modified: field_varint(bytes, 3).unwrap_or(0),
    })
}

fn parse_files(inline: Vec<&[u8]>, compressed: Option<&[u8]>) -> Vec<FileInfo> {
    let mut files = inline
        .into_iter()
        .filter_map(parse_file_info)
        .collect::<Vec<_>>();
    if files.is_empty() {
        if let Some(data) = compressed {
            let mut inflated = Vec::new();
            let decoder = ZlibDecoder::new(data);
            if decoder
                .take(MAX_LIST_BYTES as u64 + 1)
                .read_to_end(&mut inflated)
                .is_ok()
                && inflated.len() <= MAX_LIST_BYTES
            {
                files = all_field_bytes(&inflated, 1)
                    .into_iter()
                    .filter_map(parse_file_info)
                    .collect();
            }
        }
    }
    files
}

fn file_list(files: &[FileInfo]) -> Vec<u8> {
    let mut list = Vec::new();
    for file in files {
        put_bytes(1, &file_info(file), &mut list);
    }
    compress(&list)
}

fn compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    if encoder.write_all(data).is_err() {
        return Vec::new();
    }
    encoder.finish().unwrap_or_default()
}

fn collect_download(root: &Path) -> Result<(String, Vec<(FileInfo, PathBuf)>), u64> {
    let meta = std::fs::metadata(root).map_err(|error| io_code(&error))?;
    let name = root
        .file_name()
        .map(|part| part.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !meta.is_dir() {
        if meta.len() > MAX_FILE_BYTES {
            return Err(FT_IO);
        }
        return Ok((
            String::new(),
            vec![(
                FileInfo {
                    rel_path: name,
                    size: meta.len(),
                    modified: modified(&meta),
                },
                root.to_path_buf(),
            )],
        ));
    }
    let mut result = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|error| io_code(&error))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = match std::fs::metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                if meta.len() > MAX_FILE_BYTES {
                    return Err(FT_IO);
                }
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                result.push((
                    FileInfo {
                        rel_path: rel,
                        size: meta.len(),
                        modified: modified(&meta),
                    },
                    path,
                ));
            }
        }
    }
    Ok((name, result))
}

fn upload_path(task: &UploadTask, index: u64) -> Option<PathBuf> {
    let file = task.files.get(index.checked_sub(1)? as usize)?;
    let mut relative = file.rel_path.replace('\\', "/");
    if !task.folder.is_empty() && !relative.starts_with(&format!("{}/", task.folder)) {
        relative = format!("{}/{}", task.folder, relative);
    }
    join_relative(&task.target, &relative)
}

fn list_directory(raw: &str) -> Result<Vec<Vec<u8>>, u64> {
    if is_this_pc(raw) {
        let home = home();
        let mut result = Vec::new();
        if home.is_dir() {
            result.push(entry(
                FT_DIR,
                &home,
                home.file_name().and_then(|n| n.to_str()).unwrap_or("home"),
                0,
            ));
        }
        result.push(entry(FT_DIR_DRIVE, Path::new("/"), "/", 0));
        return Ok(result);
    }
    let root = normalize(raw);
    let entries = std::fs::read_dir(&root).map_err(|error| io_code(&error))?;
    let mut result = entries
        .filter_map(Result::ok)
        .take(MAX_ENTRIES)
        .filter_map(|item| {
            let meta = item.metadata().ok()?;
            let path = item.path();
            let name = item.file_name().to_string_lossy().into_owned();
            Some((
                meta.is_dir(),
                name.to_ascii_lowercase(),
                entry(
                    if meta.is_dir() { FT_DIR } else { FT_FILE },
                    &path,
                    &name,
                    meta.len(),
                ),
            ))
        })
        .collect::<Vec<_>>();
    result.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    Ok(result.into_iter().map(|(_, _, value)| value).collect())
}

fn entry(kind: u64, path: &Path, name: &str, size: u64) -> Vec<u8> {
    let mut body = Vec::new();
    put_varint(1, kind, &mut body);
    put_bytes(2, name.as_bytes(), &mut body);
    if kind == FT_FILE {
        put_varint(3, size, &mut body);
    }
    if let Ok(meta) = std::fs::metadata(path) {
        put_varint(4, modified(&meta), &mut body);
    }
    put_bytes(5, path.to_string_lossy().as_bytes(), &mut body);
    body
}

fn normalize(raw: &str) -> PathBuf {
    let replaced = raw.replace('\\', "/");
    let replaced = match replaced.split_once(':') {
        Some((drive, rest))
            if drive.len() == 1 && drive.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            rest.to_owned()
        }
        _ => replaced,
    };
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
    for component in expanded.components() {
        match component {
            std::path::Component::ParentDir => {
                clean.pop();
            }
            std::path::Component::CurDir => {}
            other => clean.push(other.as_os_str()),
        }
    }
    if clean.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        clean
    }
}

fn safe_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/') && !name.contains('\\')
}

fn safe_relative(path: &str) -> Option<PathBuf> {
    let mut relative = PathBuf::new();
    for component in path.replace('\\', "/").split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." || !safe_component(component) {
            return None;
        }
        relative.push(component);
    }
    Some(relative)
}

fn join_relative(base: &Path, relative: &str) -> Option<PathBuf> {
    Some(base.join(safe_relative(relative)?))
}

fn unique_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("file");
    let extension = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default();
    for index in 1..10_000 {
        let candidate = path.with_file_name(format!("{stem} ({index}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    path.to_path_buf()
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn is_this_pc(raw: &str) -> bool {
    let path = raw.trim().replace('\\', "/");
    path.is_empty()
        || path == ":/"
        || (path.len() <= 2
            && path.ends_with(':')
            && path[..path.len() - 1]
                .chars()
                .all(|c| c.is_ascii_alphabetic()))
}

fn modified(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|time| time.as_secs())
        .unwrap_or(0)
}

fn io_code(error: &std::io::Error) -> u64 {
    match error.kind() {
        std::io::ErrorKind::NotFound => FT_NOT_FOUND,
        std::io::ErrorKind::PermissionDenied => FT_PERMISSION,
        std::io::ErrorKind::InvalidInput => FT_PROTOCOL,
        _ => FT_IO,
    }
}

fn envelope(sequence: u64, kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    put_varint(1, sequence, &mut output);
    put_bytes(kind, payload, &mut output);
    output
}

fn text_field(bytes: &[u8], field: u32) -> String {
    String::from_utf8_lossy(field_bytes(bytes, field).unwrap_or_default()).into_owned()
}

fn all_field_bytes<'a>(bytes: &'a [u8], wanted: u32) -> Vec<&'a [u8]> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let Some((key, used)) = read_varint(&bytes[offset..]) else {
            break;
        };
        offset += used;
        let number = (key >> 3) as u32;
        match key & 7 {
            0 => {
                let Some((_, used)) = read_varint(&bytes[offset..]) else {
                    break;
                };
                offset += used;
            }
            2 => {
                let Some((length, used)) = read_varint(&bytes[offset..]) else {
                    break;
                };
                offset += used;
                let Ok(length) = usize::try_from(length) else {
                    break;
                };
                let Some(end) = offset.checked_add(length) else {
                    break;
                };
                let Some(value) = bytes.get(offset..end) else {
                    break;
                };
                if number == wanted {
                    result.push(value);
                }
                offset = end;
            }
            _ => break,
        }
    }
    result
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
    all_field_bytes(bytes, wanted).into_iter().next()
}

fn field_varint(bytes: &[u8], wanted: u32) -> Option<u64> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (key, used) = read_varint(&bytes[offset..])?;
        offset += used;
        let number = u32::try_from(key >> 3).ok()?;
        match key & 7 {
            0 => {
                let (value, used) = read_varint(&bytes[offset..])?;
                offset += used;
                if number == wanted {
                    return Some(value);
                }
            }
            2 => {
                let (length, used) = read_varint(&bytes[offset..])?;
                offset = offset.checked_add(used + usize::try_from(length).ok()?)?;
            }
            _ => return None,
        }
    }
    None
}

fn put_varint(field: u32, value: u64, output: &mut Vec<u8>) {
    varint(u64::from(field) << 3, output);
    varint(value, output);
}
fn put_bytes(field: u32, value: &[u8], output: &mut Vec<u8>) {
    varint((u64::from(field) << 3) | 2, output);
    varint(value.len() as u64, output);
    output.extend_from_slice(value);
}
fn varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
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

    fn request(case_number: u32, body: &[u8]) -> Vec<u8> {
        let mut ftp = Vec::new();
        put_bytes(case_number, body, &mut ftp);
        let mut rpc = Vec::new();
        let mut header = Vec::new();
        put_varint(1, 7, &mut header);
        put_bytes(1, &header, &mut rpc);
        put_bytes(8, &ftp, &mut rpc);
        let mut envelope = Vec::new();
        put_bytes(21, &rpc, &mut envelope);
        envelope
    }

    fn response_case(bytes: &[u8]) -> u32 {
        first_message_field(field_bytes(field_bytes(bytes, 22).unwrap(), 4).unwrap())
            .unwrap()
            .0
    }

    #[test]
    fn normalizes_windows_and_parent_segments() {
        assert_eq!(
            normalize("C:\\..\\home\\a\\.\\b"),
            PathBuf::from("/home/a/b")
        );
        assert_eq!(normalize("/tmp/../var"), PathBuf::from("/var"));
        assert!(join_relative(Path::new("/tmp"), "a/b").is_some());
        assert!(join_relative(Path::new("/tmp"), "../escape").is_none());
    }

    #[tokio::test]
    async fn directory_rpc_is_compressed_and_bounded() {
        let host = FileHost::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut body = Vec::new();
        put_bytes(1, &task_id(TaskId { task: 4, index: 0 }), &mut body);
        put_bytes(2, b"/tmp", &mut body);
        host.handle(&request(10, &body), tx).await;
        let response = rx.recv().await.unwrap();
        assert_eq!(response_case(&response), 7);
        let ftp = field_bytes(field_bytes(&response, 22).unwrap(), 4).unwrap();
        let directory = field_bytes(ftp, 7).unwrap();
        assert!(field_bytes(directory, 5).is_some());
    }

    #[tokio::test]
    async fn upload_writes_blocks_and_confirms() {
        let root = std::env::temp_dir().join(format!("uu-clean-upload-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let host = FileHost::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = TaskId { task: 9, index: 1 };
        let mut info = Vec::new();
        put_bytes(1, b"x.bin", &mut info);
        put_varint(2, 6, &mut info);
        let mut receive = Vec::new();
        put_bytes(1, &task_id(id), &mut receive);
        put_bytes(2, root.to_string_lossy().as_bytes(), &mut receive);
        put_bytes(3, &info, &mut receive);
        host.handle(&request(1, &receive), tx.clone()).await;
        let mut ask = Vec::new();
        put_bytes(1, &task_id(id), &mut ask);
        put_varint(3, 6, &mut ask);
        host.handle(&request(3, &ask), tx.clone()).await;
        for (block, data) in [(0, b"abc".as_slice()), (1, b"def".as_slice())] {
            let mut body = Vec::new();
            put_bytes(1, &task_id(id), &mut body);
            put_varint(2, block, &mut body);
            put_bytes(3, data, &mut body);
            host.handle(&request(4, &body), tx.clone()).await;
        }
        let mut done = Vec::new();
        put_bytes(1, &task_id(id), &mut done);
        put_varint(2, FT_SUCCEEDED, &mut done);
        host.handle(&request(9, &done), tx).await;
        while rx.try_recv().is_ok() {}
        assert_eq!(std::fs::read(root.join("x.bin")).unwrap(), b"abcdef");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_emits_confirmed_blocks_and_task_end() {
        let root = std::env::temp_dir().join(format!("uu-clean-download-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("x.txt"), b"hello world").unwrap();
        let host = FileHost::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = TaskId { task: 22, index: 0 };
        let mut body = Vec::new();
        put_bytes(1, &task_id(id), &mut body);
        put_bytes(
            2,
            root.join("x.txt").to_string_lossy().as_bytes(),
            &mut body,
        );
        host.handle(&request(2, &body), tx.clone()).await;
        let initial = rx.recv().await.unwrap();
        assert_eq!(response_case(&initial), 2);
        let ask = rx.recv().await.unwrap();
        let ask_rpc = field_bytes(&ask, 21).unwrap();
        let ask_id = field_bytes(ask_rpc, 1)
            .and_then(|header| field_varint(header, 1))
            .unwrap();
        let ask_ftp = field_bytes(ask_rpc, 8).unwrap();
        let (_, ask_body) = first_message_field(ask_ftp).unwrap();
        let file_id = field_bytes(ask_body, 1).unwrap().to_vec();
        let mut confirm = Vec::new();
        put_bytes(1, &file_id, &mut confirm);
        put_varint(2, 0, &mut confirm);
        put_varint(3, FT_SUCCEEDED, &mut confirm);
        put_varint(4, 0, &mut confirm);
        host.handle(&response(ask_id, 3, &confirm), tx.clone())
            .await;
        let block = rx.recv().await.unwrap();
        let block_rpc = field_bytes(&block, 21).unwrap();
        let block_id = field_bytes(block_rpc, 1)
            .and_then(|header| field_varint(header, 1))
            .unwrap();
        let block_ftp = field_bytes(block_rpc, 8).unwrap();
        let (_, block_body) = first_message_field(block_ftp).unwrap();
        let block_number = field_varint(block_body, 2).unwrap();
        let data = field_bytes(block_body, 3).unwrap();
        assert_eq!(data, b"hello world");
        let mut block_ack = Vec::new();
        put_bytes(1, &file_id, &mut block_ack);
        put_varint(2, block_number, &mut block_ack);
        put_varint(3, FT_SUCCEEDED, &mut block_ack);
        put_varint(4, data.len() as u64, &mut block_ack);
        host.handle(&response(block_id, 4, &block_ack), tx.clone())
            .await;
        let done = rx.recv().await.unwrap();
        let done_rpc = field_bytes(&done, 21).unwrap();
        let done_id = field_bytes(done_rpc, 1)
            .and_then(|header| field_varint(header, 1))
            .unwrap();
        let done_ftp = field_bytes(done_rpc, 8).unwrap();
        let (_, _done_body) = first_message_field(done_ftp).unwrap();
        let mut complete = Vec::new();
        put_bytes(1, &file_id, &mut complete);
        put_varint(2, FT_SUCCEEDED, &mut complete);
        host.handle(&response(done_id, 6, &complete), tx.clone())
            .await;
        let final_message = rx.recv().await.unwrap();
        assert_eq!(field_bytes(&final_message, 21).is_some(), true);
        let _ = std::fs::remove_dir_all(root);
    }

    fn response(request_id: u64, case_number: u32, body: &[u8]) -> Vec<u8> {
        let mut ftp = Vec::new();
        put_bytes(case_number, body, &mut ftp);
        let mut rpc = Vec::new();
        let mut header = Vec::new();
        put_varint(1, request_id, &mut header);
        put_bytes(1, &header, &mut rpc);
        put_bytes(4, &ftp, &mut rpc);
        let mut envelope = Vec::new();
        put_bytes(22, &rpc, &mut envelope);
        envelope
    }

    #[test]
    fn file_list_round_trips_through_zlib() {
        let files = vec![FileInfo {
            rel_path: "a.txt".into(),
            size: 3,
            modified: 9,
        }];
        assert_eq!(parse_files(Vec::new(), Some(&file_list(&files))), files);
    }
}
