use crate::{auth, config::Credentials};
use reqwest::Client;
use serde::{Deserialize, Deserializer};

#[derive(Clone, Deserialize)]
pub struct Room {
    pub token: String,
    #[serde(default, deserialize_with = "deserialize_signaling_list")]
    pub signaling_list: Vec<String>,
    // Recent UU responses include both fields; keeping them separate avoids
    // serde's duplicate-field error while preserving older server shapes.
    #[serde(default)]
    pub signaling_server: Option<String>,
    #[serde(default)]
    pub ws_connect_timeout_ms: u64,
}

fn deserialize_signaling_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    match value {
        serde_json::Value::String(value) if !value.is_empty() => Ok(vec![value]),
        serde_json::Value::Array(values) => Ok(values
            .into_iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect()),
        serde_json::Value::Null => Ok(Vec::new()),
        _ => Err(serde::de::Error::custom("invalid signaling list")),
    }
}

pub async fn create(client: &Client, credentials: &Credentials) -> Result<Room, String> {
    let path = "/api/v1/room/create";
    let body = "{}";
    let response = client
        .post(format!("{}{}", auth::API_BASE, path))
        .headers(auth::headers(credentials, "POST", path, body)?)
        .body(body.to_owned())
        .send()
        .await
        .map_err(|_| "UU room creation failed")?;
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "invalid UU room response")?;
    let code = value
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    if !status.is_success() || code != 0 {
        return Err(format_api_error(
            "UU room creation rejected",
            status,
            code,
            &value,
        ));
    }
    let data = value
        .get("data")
        .cloned()
        .ok_or("UU room response has no data")?;
    let mut room: Room = serde_json::from_value(data.clone()).map_err(|error| {
        let keys = data
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        format!("UU room response has no usable room (fields: {keys}; decode: {error})")
    })?;
    if room.signaling_list.is_empty() {
        if let Some(server) = room
            .signaling_server
            .take()
            .filter(|value| !value.is_empty())
        {
            room.signaling_list.push(server);
        }
    }
    Ok(room)
}

pub async fn join(
    client: &Client,
    credentials: &Credentials,
    device: &str,
) -> Result<Room, String> {
    if device.is_empty() || device == "." || device == ".." || device.contains('\0') {
        return Err("invalid device id".into());
    }
    let path = format!("/api/v1/room/join/by_device/{}", path_segment(device));
    // UU returns 2002 when another viewer owns the control slot. Keep the
    // safe default, but allow an explicit takeover for local diagnostics.
    let body = if std::env::var("UU_NATIVE_FORCE_JOIN").as_deref() == Ok("1") {
        r#"{"force_join":true}"#
    } else {
        r#"{"force_join":false}"#
    };
    let response = client
        .post(format!("{}{}", auth::API_BASE, path))
        .headers(auth::headers(credentials, "POST", &path, body)?)
        .body(body.to_owned())
        .send()
        .await
        .map_err(|_| "UU room request failed")?;
    let status = response.status();
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|_| "invalid UU room response")?;
    let code = value
        .get("code")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(-1);
    if !status.is_success() || code != 0 {
        return Err(format_api_error(
            "UU room request rejected",
            status,
            code,
            &value,
        ));
    }
    let data = value
        .get("data")
        .cloned()
        .ok_or("UU room response has no data")?;
    let mut room: Room = serde_json::from_value(data.clone()).map_err(|error| {
        let keys = data
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default();
        format!("UU room response has no usable room (fields: {keys}; decode: {error})")
    })?;
    if room.signaling_list.is_empty() {
        if let Some(server) = room
            .signaling_server
            .take()
            .filter(|value| !value.is_empty())
        {
            room.signaling_list.push(server);
        }
    }
    Ok(room)
}

fn format_api_error(
    prefix: &str,
    status: reqwest::StatusCode,
    code: i64,
    value: &serde_json::Value,
) -> String {
    let msg = value
        .get("msg")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(160)
        .collect::<String>();
    if msg.is_empty() {
        format!("{prefix} (http {} code {code})", status.as_u16())
    } else {
        format!("{prefix} (http {} code {code}): {msg}", status.as_u16())
    }
}

fn path_segment(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_join_and_host_room_shapes() {
        let join: Room =
            serde_json::from_value(serde_json::json!({"token":"t","signaling_list":["wss://a"]}))
                .unwrap();
        assert_eq!(join.signaling_list, vec!["wss://a"]);
        let host: Room =
            serde_json::from_value(serde_json::json!({"token":"t","signaling_server":"wss://a"}))
                .unwrap();
        assert_eq!(host.signaling_server.as_deref(), Some("wss://a"));
        let both: Room = serde_json::from_value(serde_json::json!({
            "token":"t",
            "signaling_server":"wss://fallback",
            "signaling_list":["wss://a"]
        }))
        .unwrap();
        assert_eq!(both.signaling_list, vec!["wss://a"]);
        assert_eq!(path_segment("device/id?x"), "device%2Fid%3Fx");
    }
}
