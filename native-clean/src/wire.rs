use serde_json::json;

pub const CHANNEL_NAMES: [&str; 5] = [
    "CONTROL_DATA_CHANNEL",
    "TEXT_DATA_CHANNEL",
    "STREAMER_DATA_CHANNEL",
    "FILE_DATA_CHANNEL",
    "BINARY_DATA_CHANNEL",
];

pub fn uuid() -> String {
    let bytes: [u8; 16] = rand::random();
    let text = hex::encode(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &text[..8],
        &text[8..12],
        &text[12..16],
        &text[16..20],
        &text[20..]
    )
}

pub fn varint(mut value: u64, output: &mut Vec<u8>) {
    while value >= 0x80 {
        output.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}

pub fn field_varint(field: u32, value: u64, output: &mut Vec<u8>) {
    varint(u64::from(field) << 3, output);
    varint(value, output);
}

pub fn field_bytes(field: u32, value: &[u8], output: &mut Vec<u8>) {
    varint((u64::from(field) << 3) | 2, output);
    varint(value.len() as u64, output);
    output.extend_from_slice(value);
}

pub fn envelope(sequence: u64, case_number: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    field_varint(1, sequence, &mut out);
    field_bytes(case_number, body, &mut out);
    out
}

pub fn terminal_control(sequence: u64) -> Vec<u8> {
    let mut body = Vec::new();
    field_bytes(1, &[], &mut body);
    envelope(sequence, 28, &body)
}

pub fn connect_options(device: &str) -> Vec<u8> {
    connect_options_for(device, false)
}

pub fn connect_options_for(device: &str, desktop: bool) -> Vec<u8> {
    let mut out = Vec::new();
    field_varint(1, if desktop { 1 } else { 8 }, &mut out);
    field_varint(2, u64::MAX, &mut out);
    field_varint(8, 3, &mut out);
    field_bytes(9, device.as_bytes(), &mut out);
    field_varint(10, 1, &mut out);
    field_bytes(12, b"4.42.0", &mut out);
    out
}

pub fn control_header(app_id: &str) -> serde_json::Value {
    control_header_for(app_id, false)
}

pub fn control_header_for(app_id: &str, desktop: bool) -> serde_json::Value {
    json!({
        "control_id": app_id,
        "app_data": {"_placeholder": true, "num": 0},
        "streamer_data": json!({
            "control_id": app_id,
            "device_capability": {
                "display_info": [],
                "ice_id": "",
                "video_codec_capability": if desktop { vec!["video/H264"] } else { Vec::<&str>::new() }
            }
        }).to_string()
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub kind: u8,
    pub session: u32,
    pub payload_len: usize,
}

pub const OPEN: u8 = 1;
pub const STDIN: u8 = 2;
pub const RESIZE: u8 = 3;
pub const CLOSE: u8 = 4;
pub const STDOUT: u8 = 5;
pub const EXIT: u8 = 6;
pub const ERROR: u8 = 8;
pub const LIST_REQ: u8 = 9;
pub const LIST_RESP: u8 = 10;
pub const KILL_REQ: u8 = 11;
pub const KILL_RESP: u8 = 12;
pub const OPEN_RESP: u8 = 13;
pub const DETACH: u8 = 16;

pub fn terminal_frame(kind: u8, session: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    out.extend_from_slice(b"TERM\x01");
    out.extend_from_slice(&[kind, 0, 0]);
    out.extend_from_slice(&session.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn parse_header(bytes: &[u8]) -> Result<FrameHeader, String> {
    if bytes.len() < 16 || &bytes[..5] != b"TERM\x01" || !(1..=16).contains(&bytes[5]) {
        return Err("invalid terminal frame".into());
    }
    let session = u32::from_le_bytes(
        bytes[8..12]
            .try_into()
            .map_err(|_| "invalid terminal session")?,
    );
    let payload_len = u32::from_le_bytes(
        bytes[12..16]
            .try_into()
            .map_err(|_| "invalid terminal length")?,
    ) as usize;
    if payload_len > 1024 * 1024 {
        return Err("terminal payload exceeds 1 MiB".into());
    }
    Ok(FrameHeader {
        kind: bytes[5],
        session,
        payload_len,
    })
}

pub fn frame_payload(bytes: &[u8], header: FrameHeader) -> Result<&[u8], String> {
    if bytes.len() != 16 + header.payload_len {
        return Err("truncated terminal frame".into());
    }
    Ok(&bytes[16..])
}

pub fn find_varint(bytes: &[u8], wanted: u32) -> Option<u64> {
    let mut at = 0;
    while at < bytes.len() {
        let (key, used) = read_varint(&bytes[at..])?;
        at += used;
        let field = (key >> 3) as u32;
        let wire = key & 7;
        match wire {
            0 => {
                let (value, used) = read_varint(&bytes[at..])?;
                at += used;
                if field == wanted {
                    return Some(value);
                }
            }
            2 => {
                let (length, used) = read_varint(&bytes[at..])?;
                at += used + length as usize;
            }
            _ => return None,
        }
    }
    None
}

fn read_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
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
    fn terminal_frames_round_trip_header() {
        let frame = terminal_frame(STDIN, 7, b"whoami\r\n");
        let header = parse_header(&frame).unwrap();
        assert_eq!(
            header,
            FrameHeader {
                kind: STDIN,
                session: 7,
                payload_len: 8
            }
        );
        assert_eq!(frame_payload(&frame, header).unwrap(), b"whoami\r\n");
    }

    #[test]
    fn protobuf_varints_are_stable() {
        let body = envelope(42, 28, b"x");
        assert_eq!(find_varint(&body, 1), Some(42));
    }
}
