use crate::{auth, config::Credentials};
use reqwest::Client;
use serde::{Deserialize, Deserializer};

#[derive(Clone, Deserialize)]
pub struct Room {
    pub token: String,
    #[serde(
        default,
        alias = "signaling_server",
        deserialize_with = "deserialize_signaling_list"
    )]
    pub signaling_list: Vec<String>,
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
        return Err(format!("UU room creation rejected (code {code})"));
    }
    let room = serde_json::from_value(
        value
            .get("data")
            .cloned()
            .ok_or("UU room response has no data")?,
    )
    .map_err(|_| "UU room response has no usable room")?;
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
    let body = r#"{"force_join":false}"#;
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
        return Err(format!("UU room request rejected (code {code})"));
    }
    let room = serde_json::from_value(
        value
            .get("data")
            .cloned()
            .ok_or("UU room response has no data")?,
    )
    .map_err(|_| "UU room response has no usable room")?;
    Ok(room)
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
        assert_eq!(host.signaling_list, vec!["wss://a"]);
        assert_eq!(path_segment("device/id?x"), "device%2Fid%3Fx");
    }
}
