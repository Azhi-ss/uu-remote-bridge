//! Headless UU Remote host.
//!
//! This is the host half of the clean-room implementation. It owns the cloud
//! room and accepts SOAC offers, while the terminal service is deliberately
//! independent of desktop capture and input permissions.

use crate::{
    api, config::Credentials, control, desktop, file_host::FileHost, signal::Signal,
    terminal_host::TerminalHost,
};
use bytes::Bytes;
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::mpsc;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    data_channel::RTCDataChannel,
    ice_transport::ice_candidate::RTCIceCandidateInit,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    },
};

pub struct Host {
    credentials: Credentials,
    terminal: TerminalHost,
    desktop_enabled: bool,
}

struct HostPeer {
    pc: Arc<RTCPeerConnection>,
    ice_id: String,
    _video_task: Option<tokio::task::JoinHandle<()>>,
}

impl Host {
    pub fn new(credentials: Credentials) -> Self {
        Self {
            credentials,
            terminal: TerminalHost::new(),
            desktop_enabled: false,
        }
    }

    pub fn new_with_options(credentials: Credentials, desktop_enabled: bool) -> Self {
        Self {
            credentials,
            terminal: TerminalHost::new(),
            desktop_enabled,
        }
    }

    pub async fn run(self) -> Result<(), String> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| "cannot create HTTP client")?;
        let room = api::create(&http, &self.credentials).await?;
        if room.signaling_list.is_empty() || room.token.is_empty() {
            return Err("UU room has no signaling endpoint".into());
        }
        let timeout = Duration::from_millis(room.ws_connect_timeout_ms.max(15_000));
        let mut signal = Signal::connect(&room.signaling_list, &room.token, timeout).await?;
        // The host publishes room metadata as soon as Socket.IO is ready. The
        // response is not needed for terminal operation, but sending the event
        // keeps this client compatible with the official room lifecycle.
        let _ = signal
            .request("room_info", None, Duration::from_secs(10))
            .await;

        let (candidate_tx, mut candidate_rx) = mpsc::channel::<(String, Value)>(128);
        let mut peers: HashMap<String, HostPeer> = HashMap::new();
        loop {
            tokio::select! {
                event = signal.next_event() => {
                    let event = event?;
                    self.handle_event(&mut signal, &mut peers, &candidate_tx, event).await?;
                }
                candidate = candidate_rx.recv() => {
                    let Some((_, candidate)) = candidate else { break; };
                    signal.send_soac(candidate).await?;
                }
            }
        }
        Ok(())
    }

    async fn handle_event(
        &self,
        signal: &mut Signal,
        peers: &mut HashMap<String, HostPeer>,
        candidate_tx: &mpsc::Sender<(String, Value)>,
        event: Value,
    ) -> Result<(), String> {
        let Some(name) = event.get(0).and_then(Value::as_str) else {
            return Ok(());
        };
        let body = event.get(1).cloned().unwrap_or(Value::Null);
        match name {
            "soac" => {
                let data = body.get("data").unwrap_or(&body);
                let msg_type = data.get("type").and_then(Value::as_str).unwrap_or("");
                let ice_id = data
                    .get("ice_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let app_control_id = data
                    .get("app_control_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let remote_client = body
                    .get("client_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if msg_type == "offer" {
                    let sdp = data
                        .get("sdp")
                        .and_then(Value::as_str)
                        .ok_or("offer has no SDP")?;
                    if ice_id.is_empty() {
                        return Err("offer has no ICE id".into());
                    }
                    if let Some(old) = peers.remove(&ice_id) {
                        let _ = old.pc.close().await;
                    }
                    let peer = HostPeer::accept_offer(
                        &self.terminal,
                        self.desktop_enabled,
                        sdp,
                        ice_id.clone(),
                        app_control_id.clone(),
                        remote_client.clone(),
                        candidate_tx.clone(),
                    )
                    .await?;
                    let answer = peer
                        .pc
                        .local_description()
                        .await
                        .ok_or("host has no local SDP")?;
                    signal.send_soac(json!({
                        "client_id": remote_client,
                        "data": { "type": "answer", "sdp": answer.sdp, "ice_id": ice_id, "app_control_id": app_control_id, "ice_network_type": 3 }
                    })).await?;
                    peers.insert(peer.ice_id.clone(), peer);
                } else if msg_type == "candidate" {
                    if let Some(peer) = peers.get(&ice_id) {
                        if let Some(candidate) = data.get("candidate") {
                            let candidate: RTCIceCandidateInit =
                                serde_json::from_value(candidate.clone())
                                    .map_err(|_| "invalid remote ICE candidate")?;
                            peer.pc
                                .add_ice_candidate(candidate)
                                .await
                                .map_err(|_| "cannot add remote ICE candidate")?;
                        }
                    }
                }
            }
            "released" | "left" => {
                if let Some(ice_id) = body.get("ice_id").and_then(Value::as_str) {
                    if let Some(peer) = peers.remove(ice_id) {
                        let _ = peer.pc.close().await;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl HostPeer {
    async fn accept_offer(
        terminal: &TerminalHost,
        desktop_enabled: bool,
        sdp: &str,
        ice_id: String,
        app_control_id: String,
        remote_client: String,
        candidate_tx: mpsc::Sender<(String, Value)>,
    ) -> Result<Self, String> {
        let mut engine = MediaEngine::default();
        engine
            .register_default_codecs()
            .map_err(|e| format!("cannot register host codecs: {e}"))?;
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
            api.new_peer_connection(RTCConfiguration::default())
                .await
                .map_err(|_| "cannot create host WebRTC peer")?,
        );
        install_ice_callback(
            &pc,
            candidate_tx,
            ice_id.clone(),
            app_control_id,
            remote_client,
        );
        install_data_channels(&pc, terminal.clone(), FileHost::new(), desktop_enabled);
        let mut video_task = None;
        let viewer_requested_video = sdp.lines().any(|line| line.starts_with("m=video"));
        if let Some(video) = desktop::start_video(desktop_enabled && viewer_requested_video).await?
        {
            pc.add_track(video.track.clone())
                .await
                .map_err(|_| "cannot add desktop video track")?;
            video_task = Some(video.task);
        }
        let offer =
            RTCSessionDescription::offer(sdp.to_owned()).map_err(|_| "invalid remote offer")?;
        pc.set_remote_description(offer)
            .await
            .map_err(|_| "cannot set remote offer")?;
        let mut gathered = pc.gathering_complete_promise().await;
        let answer = pc
            .create_answer(None)
            .await
            .map_err(|_| "cannot create host answer")?;
        pc.set_local_description(answer)
            .await
            .map_err(|_| "cannot set host answer")?;
        let _ = tokio::time::timeout(Duration::from_secs(8), gathered.recv()).await;
        Ok(Self {
            pc,
            ice_id,
            _video_task: video_task,
        })
    }
}

fn install_data_channels(
    pc: &Arc<RTCPeerConnection>,
    terminal: TerminalHost,
    files: FileHost,
    desktop_enabled: bool,
) {
    pc.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let send_channel = channel.clone();
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if send_channel.send(&Bytes::from(bytes)).await.is_err() {
                    break;
                }
            }
        });
        let terminal = terminal.clone();
        let files = files.clone();
        let label = channel.label().to_owned();
        channel.on_message(Box::new(move |message| {
            let terminal = terminal.clone();
            let files = files.clone();
            let tx = tx.clone();
            let label = label.clone();
            Box::pin(async move {
                if label == "BINARY_DATA_CHANNEL" {
                    terminal.handle(&message.data, tx).await;
                } else if label == "FILE_DATA_CHANNEL" {
                    files.handle(&message.data, tx).await;
                } else if label == "CONTROL_DATA_CHANNEL" {
                    if crate::file_client::is_file_message(&message.data) {
                        files.handle(&message.data, tx).await;
                    } else if let Some(response) = control::response_for(&message.data) {
                        let _ = tx.send(response);
                    } else if desktop_enabled {
                        if let Ok(value) = serde_json::from_slice::<Value>(&message.data) {
                            let _ = desktop::apply_input(&value);
                        }
                    }
                }
            })
        }));
        Box::pin(async {})
    }));
}

fn install_ice_callback(
    pc: &Arc<RTCPeerConnection>,
    tx: mpsc::Sender<(String, Value)>,
    ice_id: String,
    app_control_id: String,
    remote_client: String,
) {
    pc.on_ice_candidate(Box::new(move |candidate| {
        let tx = tx.clone();
        let ice_id = ice_id.clone();
        let app_control_id = app_control_id.clone();
        let remote_client = remote_client.clone();
        Box::pin(async move {
            let Some(candidate) = candidate else { return; };
            let Ok(candidate) = candidate.to_json() else { return; };
            let message = json!({
                "client_id": remote_client,
                "data": { "type": "candidate", "candidate": candidate, "ice_id": ice_id, "app_control_id": app_control_id, "ice_network_type": 3 }
            });
            let _ = tx.send((ice_id, message)).await;
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio::time::{sleep, timeout};
    use webrtc::{
        api::{
            interceptor_registry::register_default_interceptors, media_engine::MediaEngine,
            APIBuilder,
        },
        data_channel::{data_channel_init::RTCDataChannelInit, RTCDataChannel},
        peer_connection::configuration::RTCConfiguration,
    };

    async fn test_api() -> webrtc::api::API {
        let mut engine = MediaEngine::default();
        engine.register_default_codecs().unwrap();
        let registry = register_default_interceptors(
            webrtc::interceptor::registry::Registry::new(),
            &mut engine,
        )
        .unwrap();
        APIBuilder::new()
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .build()
    }

    #[tokio::test]
    async fn loopback_terminal_channel_accepts_a_host_session() {
        let viewer_pc = Arc::new(
            test_api()
                .await
                .new_peer_connection(RTCConfiguration::default())
                .await
                .unwrap(),
        );
        let (received_tx, mut received_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let viewer_channel = viewer_pc
            .create_data_channel(
                "BINARY_DATA_CHANNEL",
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let control_channel = viewer_pc
            .create_data_channel(
                "CONTROL_DATA_CHANNEL",
                Some(RTCDataChannelInit {
                    ordered: Some(true),
                    ..Default::default()
                }),
            )
            .await
            .unwrap();
        let receive_channel: Arc<RTCDataChannel> = viewer_channel.clone();
        receive_channel.on_message(Box::new(move |message| {
            let received_tx = received_tx.clone();
            Box::pin(async move {
                let _ = received_tx.send(message.data.to_vec());
            })
        }));
        let control_receive = control_channel.clone();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        control_receive.on_message(Box::new(move |message| {
            let control_tx = control_tx.clone();
            Box::pin(async move {
                let _ = control_tx.send(message.data.to_vec());
            })
        }));
        let offer = viewer_pc.create_offer(None).await.unwrap();
        let mut gathering = viewer_pc.gathering_complete_promise().await;
        viewer_pc.set_local_description(offer).await.unwrap();
        let _ = timeout(Duration::from_secs(5), gathering.recv()).await;
        let (candidate_tx, mut candidate_rx) = mpsc::channel(32);
        let host = HostPeer::accept_offer(
            &TerminalHost::new(),
            false,
            &viewer_pc.local_description().await.unwrap().sdp,
            "loopback-ice".into(),
            "loopback-control".into(),
            "loopback-client".into(),
            candidate_tx,
        )
        .await
        .unwrap();
        let host_sdp = host.pc.local_description().await.unwrap();
        viewer_pc
            .set_remote_description(RTCSessionDescription::answer(host_sdp.sdp).unwrap())
            .await
            .unwrap();
        tokio::spawn(async move { while candidate_rx.recv().await.is_some() {} });
        timeout(Duration::from_secs(5), async {
            while viewer_channel.ready_state()
                != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        timeout(Duration::from_secs(5), async {
            while control_channel.ready_state()
                != webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
            {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        viewer_channel
            .send(&Bytes::from(crate::wire::terminal_frame(
                crate::wire::LIST_REQ,
                77,
                &[],
            )))
            .await
            .unwrap();
        let response = timeout(Duration::from_secs(5), received_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let header = crate::wire::parse_header(&response).unwrap();
        assert_eq!(header.kind, crate::wire::LIST_RESP);
        assert_eq!(header.session, 77);
        control_channel
            .send(&Bytes::from(crate::control::echo_request(9)))
            .await
            .unwrap();
        let response = timeout(Duration::from_secs(5), control_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(response
            .windows(b"{\"seq\":9}".len())
            .any(|window| window == b"{\"seq\":9}"));
        viewer_pc.close().await.unwrap();
        host.pc.close().await.unwrap();
    }
}
