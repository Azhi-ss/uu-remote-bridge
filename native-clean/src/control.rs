//! Small protobuf control envelope used by the headless host/viewer handshake.
//!
//! The desktop input/video messages remain optional. The only mandatory control
//! exchange is Echo: it proves the peer is alive without claiming clipboard,
//! capture, PAM, or input capabilities that this build does not implement.

use crate::wire;

pub fn echo_request(sequence: u64) -> Vec<u8> {
    let args = format!(r#"{{"seq":{sequence}}}"#);
    let mut action = Vec::with_capacity(args.len() + 4);
    action.extend_from_slice(&[8, 1, 18, args.len() as u8]);
    action.extend_from_slice(args.as_bytes());
    wire::envelope(sequence, 3, &action)
}

pub fn echo_response(request_sequence: u64) -> Vec<u8> {
    let args = format!(r#"{{"seq":{request_sequence}}}"#);
    let mut action = Vec::with_capacity(args.len() + 16);
    action.extend_from_slice(&[8, 1, 18, args.len() as u8]);
    action.extend_from_slice(args.as_bytes());
    // Verified baseline feature versions: simple action and system metrics only.
    action.extend_from_slice(&[34, 8, 8, 4, 16, 2, 24, 2, 48, 2]);
    wire::envelope(request_sequence, 3, &action)
}

pub fn response_for(bytes: &[u8]) -> Option<Vec<u8>> {
    let top_fields = fields(bytes)?;
    let sequence = top_fields
        .iter()
        .find(|(number, wire_type, _)| *number == 1 && *wire_type == 0)
        .and_then(|(_, _, value)| decode_varint(value))
        .unwrap_or(0);
    let action = top_fields
        .iter()
        .find(|(number, wire_type, _)| *number == 3 && *wire_type == 2)
        .map(|(_, _, value)| value)?;
    let action_fields = fields(action)?;
    let is_echo = action_fields.iter().any(|(number, wire_type, value)| {
        *number == 1 && *wire_type == 0 && decode_varint(value) == Some(1)
    });
    is_echo.then(|| echo_response(sequence))
}

fn fields(mut bytes: &[u8]) -> Option<Vec<(u32, u8, Vec<u8>)>> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let (key, used) = read_varint(bytes)?;
        bytes = &bytes[used..];
        let number = u32::try_from(key >> 3).ok()?;
        let wire_type = (key & 7) as u8;
        let (value, consumed) = match wire_type {
            0 => {
                let (value, used) = read_varint(bytes)?;
                (value.to_le_bytes().to_vec(), used)
            }
            2 => {
                let (length, used) = read_varint(bytes)?;
                let length = usize::try_from(length).ok()?;
                let end = used.checked_add(length)?;
                (bytes.get(used..end)?.to_vec(), end)
            }
            _ => return None,
        };
        if consumed > bytes.len() {
            return None;
        }
        bytes = &bytes[consumed..];
        result.push((number, wire_type, value));
    }
    Some(result)
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

fn decode_varint(bytes: &[u8]) -> Option<u64> {
    read_varint(bytes).map(|(value, _)| value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_only_the_verified_action_shape() {
        let request = echo_request(7);
        let response = response_for(&request).unwrap();
        assert!(response
            .windows(b"{\"seq\":7}".len())
            .any(|window| window == b"{\"seq\":7}"));
        assert!(response_for(&wire::envelope(7, 3, &[])).is_none());
    }
}
