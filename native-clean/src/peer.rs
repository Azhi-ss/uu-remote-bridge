use crate::{api, config::Credentials, control, signal::Signal, terminal, wire};
use bytes::Bytes;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::mpsc;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    data_channel::{data_channel_init::RTCDataChannelInit, RTCDataChannel},
    ice_transport::ice_server::RTCIceServer,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    },
};

#[derive(Debug, Clone)]
pub struct Session {
    pub id: u32,
    pub name: String,
    pub shell: String,
    pub state: String,
}

pub struct Client {
    peer: Peer,
}

struct Peer {
    pc: Arc<RTCPeerConnection>,
    channels: Vec<Arc<RTCDataChannel>>,
    signal_task: tokio::task::JoinHandle<()>,
    incoming: mpsc::Receiver<(usize, Vec<u8>)>,
    buffered: VecDeque<(usize, Vec<u8>)>,
}

impl Client {
    pub async fn connect(device: &str) -> Result<Self, String> {
        let credentials = crate::config::load()?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| "cannot create HTTP client")?;
        let room = api::join(&http, &credentials, device).await?;
        let peer = Peer::connect(&credentials, device, &room).await?;
        Ok(Self { peer })
    }

    pub async fn sessions(&mut self) -> Result<Vec<Session>, String> {
        let request_id = 1;
        self.peer
            .send(4, wire::terminal_frame(wire::LIST_REQ, request_id, &[]))
            .await?;
        loop {
            let (channel, bytes) = self.peer.recv().await?;
            if channel != 4 {
                continue;
            }
            let header = wire::parse_header(&bytes)?;
            let payload = wire::frame_payload(&bytes, header)?;
            if header.kind == wire::LIST_RESP {
                let values: Vec<Value> =
                    serde_json::from_slice(payload).map_err(|_| "invalid terminal session list")?;
                return Ok(values.into_iter().filter_map(parse_session).collect());
            }
        }
    }

    pub async fn exec(&mut self, selector: &str, command: &str) -> Result<String, String> {
        let sessions = self.sessions().await?;
        let selected = sessions
            .iter()
            .find(|s| s.id.to_string() == selector || s.name == selector)
            .cloned()
            .ok_or_else(|| format!("terminal session not found: {selector}"))?;
        let open_payload = json!({"cols": 120, "rows": 39, "option": "none"}).to_string();
        self.peer
            .send(
                4,
                wire::terminal_frame(wire::OPEN, selected.id, open_payload.as_bytes()),
            )
            .await?;
        self.wait_open(selected.id).await?;
        let marker = format!(
            "__UU_CLEAN_{}_{}__",
            std::process::id(),
            terminal::now_nanos()
        );
        let command_bytes = terminal::command_for_shell(&selected.shell, command, &marker);
        self.peer
            .send(
                4,
                wire::terminal_frame(wire::STDIN, selected.id, &command_bytes),
            )
            .await?;
        let mut output = Vec::new();
        loop {
            let (channel, bytes) = self.peer.recv().await?;
            if channel != 4 {
                continue;
            }
            let header = wire::parse_header(&bytes)?;
            if header.kind != wire::STDOUT || header.session != selected.id {
                continue;
            }
            output.extend_from_slice(wire::frame_payload(&bytes, header)?);
            if let Some(index) = find_bytes(&output, marker.as_bytes()) {
                let _ = self
                    .peer
                    .send(4, wire::terminal_frame(wire::DETACH, selected.id, &[]))
                    .await;
                return Ok(String::from_utf8_lossy(&output[..index]).into_owned());
            }
            if output.len() > 2 * 1024 * 1024 {
                let _ = self
                    .peer
                    .send(4, wire::terminal_frame(wire::DETACH, selected.id, &[]))
                    .await;
                return Err("terminal output exceeds 2 MiB".into());
            }
        }
    }

    async fn wait_open(&mut self, id: u32) -> Result<(), String> {
        loop {
            let (channel, bytes) = self.peer.recv().await?;
            if channel != 4 {
                continue;
            }
            let header = wire::parse_header(&bytes)?;
            if header.kind != wire::OPEN_RESP || header.session != id {
                continue;
            }
            let value: Value = serde_json::from_slice(wire::frame_payload(&bytes, header)?)
                .map_err(|_| "invalid terminal open response")?;
            if value.get("code").and_then(Value::as_i64).unwrap_or(-1) != 0 {
                return Err("terminal open rejected".into());
            }
            return Ok(());
        }
    }
}

impl Peer {
    async fn connect(
        _credentials: &Credentials,
        device: &str,
        room: &api::Room,
    ) -> Result<Self, String> {
        let app_id = wire::uuid();
        let requested_ice = hex::encode(rand::random::<[u8; 16]>());
        let mut signal = Signal::connect(
            &room.signaling_list,
            &room.token,
            Duration::from_millis(room.ws_connect_timeout_ms.max(15_000)),
        )
        .await?;
        let ack = signal
            .control(
                wire::control_header(&app_id),
                wire::connect_options(device),
                Duration::from_secs(20),
            )
            .await?;
        let remote_client = ack
            .get("client_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or("control acknowledgement has no peer route")?
            .to_owned();
        let ice_id = ack
            .get("ice_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or(&requested_ice)
            .to_owned();
        let configuration = rtc_configuration(&ack)?;
        let (candidate_tx, mut candidate_rx) = mpsc::channel(128);
        let (data_tx, data_rx) = mpsc::channel(256);
        let mut engine = MediaEngine::default();
        let registry = register_default_interceptors(
            webrtc::interceptor::registry::Registry::new(),
            &mut engine,
        )
        .map_err(|_| "cannot initialize WebRTC interceptors")?;
        let api = APIBuilder::new()
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .build();
        let pc = Arc::new(
            api.new_peer_connection(configuration)
                .await
                .map_err(|_| "cannot create WebRTC peer")?,
        );
        install_ice_callback(
            &pc,
            candidate_tx,
            app_id.clone(),
            ice_id.clone(),
            remote_client.clone(),
        );
        let ready = Arc::new(AtomicBool::new(false));
        let mut channels = Vec::new();
        for (index, label) in wire::CHANNEL_NAMES.iter().enumerate() {
            let channel = pc
                .create_data_channel(
                    label,
                    Some(RTCDataChannelInit {
                        ordered: Some(true),
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|_| "cannot create data channel")?;
            let state = ready.clone();
            let channel_for_open = channel.clone();
            channel.on_open(Box::new(move || {
                let state = state.clone();
                let channel = channel_for_open.clone();
                Box::pin(async move {
                    if index == 0 {
                        let _ = channel.send(&Bytes::from(control::echo_request(1))).await;
                        state.store(true, Ordering::Release);
                    }
                })
            }));
            let tx = data_tx.clone();
            channel.on_message(Box::new(move |message| {
                let tx = tx.clone();
                Box::pin(async move {
                    let _ = tx.send((index, message.data.to_vec())).await;
                })
            }));
            channels.push(channel);
        }
        let offer = pc
            .create_offer(None)
            .await
            .map_err(|_| "cannot create WebRTC offer")?;
        let mut sdp = offer.sdp.clone();
        pc.set_local_description(offer)
            .await
            .map_err(|_| "cannot set local WebRTC offer")?;
        if !sdp.contains("a=x-uuremote-quic") {
            if let Some(index) = sdp.find("a=group:BUNDLE") {
                sdp.insert_str(index, "a=x-uuremote-quic\r\n");
            }
        }
        let soac = json!({"client_id": remote_client, "data": {"type": "offer", "sdp": sdp, "ice_id": ice_id, "app_control_id": app_id, "ice_network_type": 3}});
        signal.send_soac(soac).await?;
        let pc_for_task = pc.clone();
        let route_app = app_id.clone();
        let route_ice = ice_id.clone();
        let route_client = ack
            .get("client_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let signal_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    candidate = candidate_rx.recv() => {
                        if let Some(candidate) = candidate { let _ = signal.send_soac(candidate).await; } else { break; }
                    }
                    event = signal.next_event() => {
                        let Ok(event) = event else { break; };
                        let body = event.get(1).unwrap_or(&Value::Null);
                        if !route_matches(&event, &route_app, &route_ice, &route_client) { continue; }
                        match body.get("type").and_then(Value::as_str) {
                            Some("answer") => if let Some(sdp) = body.get("sdp").and_then(Value::as_str) {
                                if let Ok(answer) = RTCSessionDescription::answer(sdp.to_owned()) { let _ = pc_for_task.set_remote_description(answer).await; }
                            },
                            Some("candidate") => if let Some(candidate) = body.get("candidate") {
                                if let Ok(candidate) = serde_json::from_value(candidate.clone()) { let _ = pc_for_task.add_ice_candidate(candidate).await; }
                            },
                            _ => {}
                        }
                    }
                }
            }
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !ready.load(Ordering::Acquire) {
            if tokio::time::Instant::now() >= deadline {
                signal_task.abort();
                return Err("terminal control channel did not open".into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(Self {
            pc,
            channels,
            signal_task,
            incoming: data_rx,
            buffered: VecDeque::new(),
        })
    }

    async fn send(&self, index: usize, bytes: Vec<u8>) -> Result<(), String> {
        let channel = self
            .channels
            .get(index)
            .ok_or("unknown WebRTC data channel")?;
        channel
            .send(&Bytes::from(bytes))
            .await
            .map(|_| ())
            .map_err(|_| "WebRTC data channel send failed".into())
    }

    async fn recv(&mut self) -> Result<(usize, Vec<u8>), String> {
        if let Some(item) = self.buffered.pop_front() {
            return Ok(item);
        }
        self.incoming
            .recv()
            .await
            .ok_or_else(|| "WebRTC data channel closed".into())
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.signal_task.abort();
        let pc = self.pc.clone();
        tokio::spawn(async move {
            let _ = pc.close().await;
        });
    }
}

fn install_ice_callback(
    pc: &Arc<RTCPeerConnection>,
    tx: mpsc::Sender<Value>,
    app_id: String,
    ice_id: String,
    remote: String,
) {
    let weak = Arc::downgrade(pc);
    pc.on_ice_candidate(Box::new(move |candidate| {
        let tx = tx.clone(); let app_id = app_id.clone(); let ice_id = ice_id.clone(); let remote = remote.clone(); let weak = weak.clone();
        Box::pin(async move {
            let Some(candidate) = candidate else { return; };
            let Ok(mut value) = candidate.to_json() else { return; };
            if value.sdp_mid.is_none() {
                if let Some(pc) = weak.upgrade() {
                    if let Some(description) = pc.local_description().await {
                        value.sdp_mid = description.sdp.lines().find_map(|line| line.strip_prefix("a=mid:").map(str::to_owned));
                        value.sdp_mline_index = Some(0);
                    }
                }
            }
            let _ = tx.send(json!({"client_id": remote, "data": {"type": "candidate", "candidate": value, "ice_id": ice_id, "app_control_id": app_id, "ice_network_type": 3}})).await;
        })
    }));
}

fn route_matches(event: &Value, app_id: &str, ice_id: &str, client_id: &str) -> bool {
    let body = event.get(1).unwrap_or(event);
    body.get("ice_id")
        .and_then(Value::as_str)
        .is_none_or(|value| value == ice_id)
        && body
            .get("app_control_id")
            .and_then(Value::as_str)
            .is_none_or(|value| value == app_id)
        && event
            .get("client_id")
            .and_then(Value::as_str)
            .is_none_or(|value| value == client_id)
}

fn rtc_configuration(value: &Value) -> Result<RTCConfiguration, String> {
    let ice_servers = value
        .get("iceServers")
        .and_then(Value::as_array)
        .ok_or("control acknowledgement has no ICE servers")?
        .iter()
        .map(|item| {
            let urls = match item.get("urls") {
                Some(Value::String(url)) => vec![url.clone()],
                Some(Value::Array(urls)) => urls
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                _ => return Err("invalid ICE server URLs".into()),
            };
            Ok(RTCIceServer {
                urls,
                username: item
                    .get("username")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                credential: item
                    .get("credential")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .into(),
                ..Default::default()
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(RTCConfiguration {
        ice_servers,
        ..Default::default()
    })
}

fn parse_session(value: Value) -> Option<Session> {
    Some(Session {
        id: value.get("session_id")?.as_u64()?.try_into().ok()?,
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("--")
            .to_owned(),
        shell: value
            .get("shell")
            .and_then(Value::as_str)
            .unwrap_or("powershell")
            .to_owned(),
        state: value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("--")
            .to_owned(),
    })
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
