use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::{collections::VecDeque, time::Duration};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};

pub struct Signal {
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_ack: u64,
    queued: VecDeque<Value>,
}

impl Signal {
    pub async fn connect(urls: &[String], token: &str, timeout: Duration) -> Result<Self, String> {
        let mut last_error = "signaling connection failed".to_owned();
        for endpoint in urls {
            match tokio::time::timeout(timeout, Self::connect_one(endpoint, token, timeout)).await {
                Ok(Ok(signal)) => return Ok(signal),
                Ok(Err(error)) => last_error = error,
                Err(_) => last_error = "signaling connection timed out".to_owned(),
            }
        }
        Err(last_error)
    }

    async fn connect_one(endpoint: &str, token: &str, timeout: Duration) -> Result<Self, String> {
        let url = format!(
            "{}/socket.io/?transport=websocket&EIO=4",
            endpoint.trim_end_matches('/')
        );
        let mut request = url
            .into_client_request()
            .map_err(|_| "invalid signaling URL")?;
        request.headers_mut().insert(
            "X-NRD-AUTH",
            token.parse().map_err(|_| "invalid room token")?,
        );
        let (mut socket, _) = tokio::time::timeout(timeout, connect_async(request))
            .await
            .map_err(|_| "signaling connect timed out")?
            .map_err(|_| "signaling connect failed")?;
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) if text.starts_with('0') => {
                    serde_json::from_str::<Value>(&text[1..])
                        .map_err(|_| "invalid signaling handshake")?;
                    socket
                        .send(Message::Text("40".into()))
                        .await
                        .map_err(|_| "signaling handshake send failed")?;
                }
                Some(Ok(Message::Text(text))) if text.starts_with("40") => {
                    return Ok(Self {
                        socket,
                        next_ack: 10,
                        queued: VecDeque::new(),
                    });
                }
                Some(Ok(Message::Text(text))) if text.starts_with('2') => {
                    socket
                        .send(Message::Text(format!("3{}", &text[1..])))
                        .await
                        .map_err(|_| "signaling heartbeat failed")?;
                }
                Some(Ok(Message::Ping(bytes))) => socket
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| "signaling ping failed")?,
                _ => return Err("signaling handshake failed".into()),
            }
        }
    }

    pub async fn control(
        &mut self,
        header: Value,
        app_data: Vec<u8>,
        timeout: Duration,
    ) -> Result<Value, String> {
        self.next_ack += 1;
        let id = self.next_ack;
        self.send_text(format!(
            "451-{id}{}",
            serde_json::json!(["control", header])
        ))
        .await?;
        let mut attachment = Vec::with_capacity(app_data.len() + 1);
        attachment.push(4);
        attachment.extend_from_slice(&app_data);
        self.send_binary(attachment).await?;
        let result: Value = tokio::time::timeout(timeout, async {
            loop {
                match self.packet().await? {
                    Packet::Ack(number, value) if number == id => {
                        let array = value.as_array().ok_or("invalid control acknowledgement")?;
                        if array.first().and_then(Value::as_str) != Some("success") {
                            return Err::<Value, String>("remote control request rejected".into());
                        }
                        return array
                            .get(1)
                            .cloned()
                            .ok_or_else(|| "empty control acknowledgement".into());
                    }
                    Packet::Event(event) => {
                        if self.queued.len() >= 64 {
                            return Err("too many queued signaling events".into());
                        }
                        self.queued.push_back(event);
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| "control acknowledgement timed out")??;
        Ok(result)
    }

    pub async fn close(&mut self) {
        let _ = self.send_text("41".to_owned()).await;
        let _ = self.socket.close(None).await;
    }

    pub async fn send_soac(&mut self, value: Value) -> Result<(), String> {
        self.send_text(format!("42{}", serde_json::json!(["soac", value])))
            .await
    }

    /// Send a regular Socket.IO event and wait for its acknowledgement. Hosts use
    /// this for `room_info`; viewers use the binary `control` event above.
    pub async fn request(
        &mut self,
        name: &str,
        value: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, String> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return Err("invalid signaling event name".into());
        }
        self.next_ack += 1;
        let id = self.next_ack;
        let payload = match value {
            Some(value) => serde_json::json!([name, value]),
            None => serde_json::json!([name]),
        };
        self.send_text(format!("42{id}{payload}")).await?;
        tokio::time::timeout(timeout, async {
            loop {
                match self.packet().await? {
                    Packet::Ack(number, value) if number == id => {
                        return Ok::<Value, String>(value)
                    }
                    Packet::Event(event) => {
                        if self.queued.len() >= 64 {
                            return Err("too many queued signaling events".into());
                        }
                        self.queued.push_back(event);
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| "signaling request timed out")?
    }

    pub async fn next_event(&mut self) -> Result<Value, String> {
        if let Some(event) = self.queued.pop_front() {
            return Ok(event);
        }
        loop {
            if let Packet::Event(event) = self.packet().await? {
                return Ok(event);
            }
        }
    }

    async fn send_text(&mut self, text: String) -> Result<(), String> {
        self.socket
            .send(Message::Text(text.into()))
            .await
            .map_err(|_| "signaling send failed".into())
    }

    async fn send_binary(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        self.socket
            .send(Message::Binary(bytes.into()))
            .await
            .map_err(|_| "signaling attachment failed".into())
    }

    async fn packet(&mut self) -> Result<Packet, String> {
        loop {
            match self.socket.next().await {
                Some(Ok(Message::Text(text))) if text.starts_with('2') => {
                    self.send_text(format!("3{}", &text[1..])).await?;
                }
                Some(Ok(Message::Ping(bytes))) => self
                    .socket
                    .send(Message::Pong(bytes))
                    .await
                    .map_err(|_| "signaling ping failed")?,
                Some(Ok(Message::Text(text))) if text.starts_with("43") => {
                    let start = text.find('[').ok_or("invalid signaling ack")?;
                    let id = text[2..start]
                        .parse()
                        .map_err(|_| "invalid signaling ack id")?;
                    let value = serde_json::from_str(&text[start..])
                        .map_err(|_| "invalid signaling ack body")?;
                    return Ok(Packet::Ack(id, value));
                }
                Some(Ok(Message::Text(text))) if text.starts_with("42") => {
                    let start = text.find('[').ok_or("invalid signaling event")?;
                    return Ok(Packet::Event(
                        serde_json::from_str(&text[start..])
                            .map_err(|_| "invalid signaling event body")?,
                    ));
                }
                Some(Ok(Message::Text(text)))
                    if text.starts_with("45") || text.starts_with("46") =>
                {
                    let dash = text.find('-').ok_or("invalid binary signaling header")?;
                    let count: usize = text[2..dash]
                        .parse()
                        .map_err(|_| "invalid attachment count")?;
                    if count > 8 {
                        return Err("too many signaling attachments".into());
                    }
                    let start = text.find('[').ok_or("invalid binary signaling body")?;
                    let mut value: Value = serde_json::from_str(&text[start..])
                        .map_err(|_| "invalid binary signaling JSON")?;
                    let mut attachments = Vec::with_capacity(count);
                    while attachments.len() < count {
                        match self.socket.next().await {
                            Some(Ok(Message::Binary(bytes))) => {
                                let mut bytes = bytes.to_vec();
                                if bytes.first() == Some(&4) {
                                    bytes.remove(0);
                                }
                                if bytes.len() > 4 * 1024 * 1024 {
                                    return Err("signaling attachment too large".into());
                                }
                                attachments.push(STANDARD.encode(bytes));
                            }
                            Some(Ok(Message::Text(text))) if text.starts_with('2') => {
                                self.send_text(format!("3{}", &text[1..])).await?
                            }
                            _ => return Err("signaling attachment stream ended".into()),
                        }
                    }
                    replace_placeholders(&mut value, &attachments);
                    return Ok(if text.starts_with("46") {
                        let id = text[dash + 1..start]
                            .parse()
                            .map_err(|_| "invalid binary ack id")?;
                        Packet::Ack(id, value)
                    } else {
                        Packet::Event(value)
                    });
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err("signaling connection closed".into())
                }
                Some(Ok(_)) => {}
                Some(Err(_)) => return Err("signaling receive failed".into()),
            }
        }
    }
}

enum Packet {
    Ack(u64, Value),
    Event(Value),
}

fn replace_placeholders(value: &mut Value, attachments: &[String]) {
    match value {
        Value::Object(object) if object.get("_placeholder") == Some(&Value::Bool(true)) => {
            if let Some(index) = object
                .get("num")
                .and_then(Value::as_u64)
                .and_then(|n| usize::try_from(n).ok())
            {
                if let Some(data) = attachments.get(index) {
                    *value = Value::String(data.clone());
                }
            }
        }
        Value::Object(object) => object
            .values_mut()
            .for_each(|item| replace_placeholders(item, attachments)),
        Value::Array(array) => array
            .iter_mut()
            .for_each(|item| replace_placeholders(item, attachments)),
        _ => {}
    }
}
