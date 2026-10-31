use serde::Deserialize;
use std::{env, fs, path::PathBuf};

#[derive(Clone, Deserialize, Default)]
pub struct Credentials {
    pub user_id: String,
    pub device_id: String,
    pub client_id: String,
    pub system_id: String,
    pub token: String,
    pub csrf: String,
    #[serde(default)]
    pub sign_key: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials { .. }")
    }
}

pub fn load() -> Result<Credentials, String> {
    let file = env::var_os("UU_CONFIG")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/uuremote/config.json"))
        })
        .ok_or("cannot locate UU config")?;
    let text = fs::read_to_string(&file)
        .map_err(|_| format!("cannot read UU config: {}", file.display()))?;
    let mut value: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "invalid UU config JSON")?;
    let mut credentials: Credentials =
        serde_json::from_value(value.take()).map_err(|_| "invalid UU credential fields")?;
    override_field(&mut credentials.user_id, "UU_USER_ID");
    override_field(&mut credentials.device_id, "UU_DEVICE_ID");
    override_field(&mut credentials.client_id, "UU_CLIENT_ID");
    override_field(&mut credentials.system_id, "UU_SYSTEM_ID");
    override_field(&mut credentials.token, "UU_TOKEN");
    override_field(&mut credentials.csrf, "UU_CSRF");
    override_field(&mut credentials.sign_key, "UU_SIGN_KEY");
    if credentials.sign_key.is_empty() {
        credentials.sign_key = "alWiSzXZTLu3WfFnw13uBru3".into();
    }
    let missing = [
        ("user_id", credentials.user_id.as_str()),
        ("device_id", credentials.device_id.as_str()),
        ("client_id", credentials.client_id.as_str()),
        ("system_id", credentials.system_id.as_str()),
        ("token", credentials.token.as_str()),
        ("csrf", credentials.csrf.as_str()),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.is_empty().then_some(name))
    .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(credentials)
    } else {
        Err(format!(
            "missing credential settings: {}",
            missing.join(", ")
        ))
    }
}

fn override_field(field: &mut String, name: &str) {
    if let Ok(value) = env::var(name) {
        if !value.is_empty() {
            *field = value;
        }
    }
}
