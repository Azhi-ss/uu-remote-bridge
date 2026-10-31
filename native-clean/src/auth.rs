use crate::config::Credentials;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue};
use sha2::Sha256;
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

type HmacSha256 = Hmac<Sha256>;
pub const API_BASE: &str = "https://api.nrd.nie.163.com";

pub fn headers(
    credentials: &Credentials,
    method: &str,
    path: &str,
    body: &str,
) -> Result<HeaderMap, String> {
    let mut params = BTreeMap::from([
        ("x-param-chn", "gwqd".to_owned()),
        ("x-param-cnt", "CN".to_owned()),
        ("x-param-lang", "zh-CN".to_owned()),
        ("x-param-pkgn", "com.netease.gameviewer".to_owned()),
        ("x-param-plat", "1".to_owned()),
        ("x-param-vc", "2770".to_owned()),
        ("x-param-vn", "4.42.0.2770".to_owned()),
        ("x-param-ts", now().to_string()),
    ]);
    for (key, value) in [
        ("x-param-client-id", &credentials.client_id),
        ("x-param-device-id", &credentials.device_id),
        ("x-param-system-id", &credentials.system_id),
        ("x-param-user-id", &credentials.user_id),
    ] {
        if !value.is_empty() {
            params.insert(key, value.clone());
        }
    }
    let canonical = params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut signer = HmacSha256::new_from_slice(credentials.sign_key.as_bytes())
        .map_err(|_| "invalid signing key")?;
    signer.update(format!("{}{}{}{}", method.to_uppercase(), path, canonical, body).as_bytes());
    let signature = hex::encode(signer.finalize().into_bytes());
    let mut result = HeaderMap::new();
    result.insert("content-type", HeaderValue::from_static("application/json"));
    result.insert("accept", HeaderValue::from_static("application/json"));
    result.insert(
        "user-agent",
        HeaderValue::from_static("UURemote/4.42.0 (Windows NT 10.0; Win64; x64)"),
    );
    result.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", credentials.token))
            .map_err(|_| "invalid token header")?,
    );
    result.insert(
        "x-csrf-token",
        HeaderValue::from_str(&credentials.csrf).map_err(|_| "invalid csrf header")?,
    );
    result.insert(
        "cookie",
        HeaderValue::from_str(&format!(
            "nrd_access_token={}; nrd_csrf_access_token={}",
            credentials.token, credentials.csrf
        ))
        .map_err(|_| "invalid cookie header")?,
    );
    for (key, value) in params {
        result.insert(
            reqwest::header::HeaderName::from_bytes(key.as_bytes())
                .map_err(|_| "invalid API header")?,
            HeaderValue::from_str(&value).map_err(|_| "invalid API header")?,
        );
    }
    result.insert(
        "x-param-sign",
        HeaderValue::from_str(&signature).map_err(|_| "invalid API signature")?,
    );
    Ok(result)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
