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
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
#[cfg(feature = "desktop")]
use webrtc::rtp_transceiver::{
    rtp_transceiver_direction::RTCRtpTransceiverDirection, RTCRtpTransceiverInit,
};
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
    },
    data_channel::{data_channel_init::RTCDataChannelInit, RTCDataChannel},
    ice_transport::ice_credential_type::RTCIceCredentialType,
    ice_transport::ice_server::RTCIceServer,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
        RTCPeerConnection,
    },
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters},
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

pub(crate) struct Peer {
    pc: Arc<RTCPeerConnection>,
    channels: Vec<Arc<RTCDataChannel>>,
    signal_task: tokio::task::JoinHandle<()>,
    shutdown: mpsc::Sender<()>,
    incoming: mpsc::Receiver<(usize, Vec<u8>)>,
    buffered: VecDeque<(usize, Vec<u8>)>,
    #[cfg_attr(not(feature = "desktop"), allow(dead_code))]
    video_packets: mpsc::Receiver<Vec<u8>>,
}

impl Client {
    pub async fn close(&mut self) {
        let _ = self.peer.shutdown.send(()).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.peer.pc.close()).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.peer.signal_task).await;
    }

    pub async fn finish<T>(mut self, result: Result<T, String>) -> Result<T, String> {
        self.close().await;
        result
    }

    pub async fn connect(device: &str) -> Result<Self, String> {
        Self::connect_with_options(device, false).await
    }

    pub async fn connect_with_options(device: &str, desktop_enabled: bool) -> Result<Self, String> {
        Self::connect_capture(device, if desktop_enabled { 1 } else { 8 }).await
    }

    pub async fn connect_files(device: &str) -> Result<Self, String> {
        Self::connect_capture(device, 5).await
    }

    async fn connect_capture(device: &str, capture_type: u64) -> Result<Self, String> {
        let credentials = crate::config::load()?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| "cannot create HTTP client")?;
        let room = api::join(&http, &credentials, device).await?;
        let peer = Peer::connect(&credentials, device, &room, capture_type).await?;
        Ok(Self { peer })
    }

    #[cfg(feature = "desktop")]
    pub async fn next_video_packet(&mut self) -> Result<Vec<u8>, String> {
        self.peer
            .video_packets
            .recv()
            .await
            .ok_or_else(|| "desktop video stream closed".into())
    }

    pub async fn send_input(&self, value: &Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(value).map_err(|_| "invalid desktop input JSON")?;
        self.peer.send(0, bytes).await
    }

    pub async fn files_remove(&mut self, path: &str) -> Result<(), String> {
        crate::file_client::remove(&mut self.peer, path).await
    }

    pub async fn files_list(
        &mut self,
        path: &str,
    ) -> Result<Vec<crate::file_client::FileEntry>, String> {
        crate::file_client::list(&mut self.peer, path).await
    }

    pub async fn files_upload(&mut self, local: &str, remote: &str) -> Result<(), String> {
        crate::file_client::upload(&mut self.peer, local, remote).await
    }

    pub async fn files_download(&mut self, remote: &str, local: &str) -> Result<(), String> {
        crate::file_client::download(&mut self.peer, remote, local).await
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
        capture_type: u64,
    ) -> Result<Self, String> {
        let desktop_enabled = capture_type == 1;
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
                wire::control_header_for(&app_id, desktop_enabled),
                wire::connect_options_capture(device, capture_type),
                Duration::from_secs(20),
            )
            .await?;
        debug_log(&format!(
            "control ack keys={:?} ice_servers={} client_id_present={} app_control_id_present={}",
            ack.as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default(),
            ack.get("iceServers")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            ack.get("client_id").and_then(Value::as_str).is_some(),
            ack.get("app_control_id").and_then(Value::as_str).is_some(),
        ));
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
        // Register the codecs the UU Windows host advertises.  A bare
        // MediaEngine can create SCTP, but it produces an offer without the
        // media capabilities that the host uses to accept the session.
        engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: "video/H264".into(),
                        clock_rate: 90_000,
                        channels: 0,
                        sdp_fmtp_line:
                            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                                .into(),
                        rtcp_feedback: vec![
                            webrtc::rtp_transceiver::RTCPFeedback {
                                typ: "nack".into(),
                                parameter: String::new(),
                            },
                            webrtc::rtp_transceiver::RTCPFeedback {
                                typ: "nack".into(),
                                parameter: "pli".into(),
                            },
                            webrtc::rtp_transceiver::RTCPFeedback {
                                typ: "ccm".into(),
                                parameter: "fir".into(),
                            },
                        ],
                        ..Default::default()
                    },
                    payload_type: 102,
                    ..Default::default()
                },
                RTPCodecType::Video,
            )
            .map_err(|error| format!("cannot register H264 codec: {error}"))?;
        engine
            .register_codec(
                RTCRtpCodecParameters {
                    capability: RTCRtpCodecCapability {
                        mime_type: "audio/opus".into(),
                        clock_rate: 48_000,
                        channels: 2,
                        sdp_fmtp_line: "minptime=10;useinbandfec=1".into(),
                        ..Default::default()
                    },
                    payload_type: 111,
                    ..Default::default()
                },
                RTPCodecType::Audio,
            )
            .map_err(|error| format!("cannot register Opus codec: {error}"))?;
        let registry = register_default_interceptors(
            webrtc::interceptor::registry::Registry::new(),
            &mut engine,
        )
        .map_err(|error| format!("cannot initialize WebRTC interceptors: {error}"))?;
        let mut settings = webrtc::api::setting_engine::SettingEngine::default();
        settings.detach_data_channels();
        let api = APIBuilder::new()
            .with_setting_engine(settings)
            .with_media_engine(engine)
            .with_interceptor_registry(registry)
            .build();
        let pc = Arc::new(
            api.new_peer_connection(configuration)
                .await
                .map_err(|error| format!("cannot create WebRTC peer: {error}"))?,
        );
        if std::env::var_os("UU_NATIVE_DEBUG").is_some() {
            pc.on_peer_connection_state_change(Box::new(|state| {
                eprintln!("uu-native peer state: {state:?}");
                Box::pin(async {})
            }));
        }
        #[cfg(feature = "desktop")]
        if desktop_enabled {
            pc.add_transceiver_from_kind(
                RTPCodecType::Video,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: Vec::new(),
                }),
            )
            .await
            .map_err(|error| format!("cannot create desktop video transceiver: {error}"))?;
            pc.add_transceiver_from_kind(
                RTPCodecType::Audio,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Recvonly,
                    send_encodings: Vec::new(),
                }),
            )
            .await
            .map_err(|error| format!("cannot create desktop audio transceiver: {error}"))?;
        }
        let (video_tx, video_packets) = mpsc::channel(64);
        let media_pc = Arc::downgrade(&pc);
        pc.on_track(Box::new(move |track, _, _| {
            let video_tx = video_tx.clone();
            let media_pc = media_pc.clone();
            Box::pin(async move {
                debug_log(&format!(
                    "remote track kind={:?} mime={} payload_type={}",
                    track.kind(),
                    track.codec().capability.mime_type,
                    track.payload_type()
                ));
                tokio::spawn(async move {
                    if track.kind() != RTPCodecType::Video { return; }
                    if let Some(pc) = media_pc.upgrade() {
                        let _ = pc.write_rtcp(&[Box::new(webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication {
                            sender_ssrc: 0, media_ssrc: track.ssrc(),
                        })]).await;
                    }
                    let mut samples = webrtc::media::io::sample_builder::SampleBuilder::new(
                        2048,
                        crate::media::H264Depacketizer::default(),
                        90_000,
                    ).with_max_time_delay(Duration::from_millis(500));
                    let mut packet_count = 0;
                    let mut sample_count = 0;
                    let mut last_seq: Option<u16> = None;
                    let mut gaps = 0u64;
                    let mut keyframes = tokio::time::interval(Duration::from_secs(2));
                    loop {
                        let packet = tokio::select! {
                            packet = track.read_rtp() => match packet { Ok((packet, _)) => packet, Err(_) => break },
                            _ = keyframes.tick() => {
                                if let Some(pc) = media_pc.upgrade() {
                                    let _ = pc.write_rtcp(&[Box::new(webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication {
                                        sender_ssrc: 0, media_ssrc: track.ssrc(),
                                    })]).await;
                                }
                                continue;
                            }
                        };
                        packet_count += 1;
                        if let Some(last) = last_seq { if packet.header.sequence_number != last.wrapping_add(1) { gaps += 1; } }
                        last_seq = Some(packet.header.sequence_number);
                        if packet_count <= 5 { debug_log(&format!("RTP bytes={} marker={} nal={}", packet.payload.len(), packet.header.marker, packet.payload.first().unwrap_or(&0) & 31)); }
                        if packet.payload.len() <= 2 { debug_log(&format!("short RTP len={} nal={}", packet.payload.len(), packet.payload.first().unwrap_or(&0) & 31)); }
                        samples.push(packet);
                        while let Some(sample) = samples.pop() {
                            sample_count += 1;
                            if sample_count < 15 {
                                let nals: Vec<_> = sample.data.windows(5).filter_map(|w| (w[..4] == [0,0,0,1]).then_some(w[4] & 31)).collect();
                                debug_log(&format!("H264 sample bytes={} nals={nals:?} rtp_gaps={gaps}", sample.data.len()));
                            }
                            if video_tx.send(sample.data.to_vec()).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            })
        }));
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
            let weak_channel = Arc::downgrade(&channel);
            let tx = data_tx.clone();
            channel.on_open(Box::new(move || {
                let state = state.clone();
                let weak_channel = weak_channel.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let Some(channel) = weak_channel.upgrade() else {
                        return;
                    };
                    let Ok(raw) = channel.detach().await else {
                        return;
                    };
                    debug_log(&format!("data channel opened index={index}"));
                    if index == 0 {
                        let _ = channel.send(&Bytes::from(control::echo_request(1))).await;
                        if desktop_enabled {
                            let _ = channel
                                .send(&Bytes::from(wire::request_video_track()))
                                .await;
                        }
                        state.store(true, Ordering::Release);
                    }
                    tokio::spawn(async move {
                        // Detaching avoids webrtc 0.10's fixed 65535-byte read buffer.
                        let mut buffer = vec![0; 262144];
                        loop {
                            let count = match raw.read_data_channel(&mut buffer).await {
                                Ok((0, _)) => break,
                                Ok((count, _)) => count,
                                Err(error) => {
                                    debug_log(&format!("data channel read failed: {error}"));
                                    break;
                                }
                            };
                            let bytes = &buffer[..count];
                            if index == 0 {
                                if let Some(reply) = control::response_for(bytes) {
                                    if let Some(channel) = weak_channel.upgrade() {
                                        let _ = channel.send(&Bytes::from(reply)).await;
                                    }
                                }
                            }
                            let route = if [0, 1].contains(&index)
                                && crate::file_client::is_file_message(bytes)
                            {
                                3
                            } else {
                                index
                            };
                            debug_log(&format!(
                                "data channel message index={index} bytes={count} route={route}"
                            ));
                            if route == 3 || route == 4 {
                                if tx.send((route, bytes.to_vec())).await.is_err() {
                                    break;
                                }
                            } else {
                                let _ = tx.try_send((route, bytes.to_vec()));
                            }
                        }
                    });
                })
            }));
            channels.push(channel);
        }
        let offer = pc
            .create_offer(None)
            .await
            .map_err(|error| format!("cannot create WebRTC offer: {error}"))?;
        let mut sdp = offer.sdp.clone();
        pc.set_local_description(offer)
            .await
            .map_err(|error| format!("cannot set local WebRTC offer: {error}"))?;
        if !sdp.contains("a=x-uuremote-quic") {
            if let Some(index) = sdp.find("a=group:BUNDLE") {
                sdp.insert_str(index, "a=x-uuremote-quic\r\n");
            }
        }
        sdp = sdp.replace(
            "a=sctp-port:5000\r\n",
            "a=sctp-port:5000\r\na=max-message-size:262144\r\n",
        );
        if let Ok(delay) = std::env::var("UU_NATIVE_OFFER_DELAY_MS") {
            if let Ok(delay) = delay.parse::<u64>() {
                tokio::time::sleep(Duration::from_millis(delay.min(10_000))).await;
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
        let (shutdown, mut shutdown_rx) = mpsc::channel(1);
        let signal_task = tokio::spawn(async move {
            let mut answered = false;
            let mut early_candidates = Vec::new();
            loop {
                tokio::select! {
                    _ = shutdown_rx.recv() => { signal.close().await; break; }
                    candidate = candidate_rx.recv() => {
                        if let Some(candidate) = candidate { let _ = signal.send_soac(candidate).await; } else { break; }
                    }
                    event = signal.next_event() => {
                        let Ok(event) = event else { break; };
                        let body = signal_body(&event);
                        let msg_type = body.get("type").and_then(Value::as_str).unwrap_or("-");
                        debug_log(&format!("signaling event type={msg_type} route_match={}", route_matches(&event, &route_app, &route_ice, &route_client)));
                        if !route_matches(&event, &route_app, &route_ice, &route_client) { continue; }
                        match body.get("type").and_then(Value::as_str) {
                            Some("answer") if !answered => {
                                let result = async {
                                    let sdp = answer_sdp(body)?;
                                    let answer = RTCSessionDescription::answer(sdp).map_err(|e| e.to_string())?;
                                    pc_for_task.set_remote_description(answer).await.map_err(|e| e.to_string())?;
                                    for candidate in early_candidates.drain(..) { pc_for_task.add_ice_candidate(candidate).await.map_err(|e| e.to_string())?; }
                                    Ok::<(), String>(())
                                }.await;
                                match result {
                                    Ok(()) => answered = true,
                                    Err(error) => { debug_log(&format!("answer rejected: {error}")); break; }
                                }
                            },
                            Some("candidate") => if let Some(candidate) = body.get("candidate") {
                                let parsed = if let Some(text) = candidate.as_str() {
                                    Ok(webrtc::ice_transport::ice_candidate::RTCIceCandidateInit {
                                        candidate: text.to_owned(),
                                        sdp_mid: body.get("sdp_mid").and_then(Value::as_str).map(str::to_owned),
                                        sdp_mline_index: body.get("sdp_mline_index").and_then(Value::as_u64).map(|v| v as u16),
                                        ..Default::default()
                                    })
                                } else { serde_json::from_value(candidate.clone()) };
                                if let Ok(candidate) = parsed {
                                    if answered { let _ = pc_for_task.add_ice_candidate(candidate).await; }
                                    else if early_candidates.len() < 256 { early_candidates.push(candidate); }
                                }
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
                let _ = shutdown.send(()).await;
                let _ = pc.close().await;
                let _ = signal_task.await;
                return Err("remote control channel did not open within 30 seconds".into());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok(Self {
            pc,
            channels,
            signal_task,
            shutdown,
            incoming: data_rx,
            buffered: VecDeque::new(),
            video_packets,
        })
    }

    pub(crate) async fn send(&self, index: usize, bytes: Vec<u8>) -> Result<(), String> {
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

    pub(crate) async fn recv(&mut self) -> Result<(usize, Vec<u8>), String> {
        if let Some(item) = self.buffered.pop_front() {
            return Ok(item);
        }
        tokio::time::timeout(Duration::from_secs(30), self.incoming.recv())
            .await
            .map_err(|_| "remote data response timed out after 30 seconds")?
            .ok_or_else(|| "WebRTC data channel closed".into())
    }
}

fn debug_log(message: &str) {
    if std::env::var_os("UU_NATIVE_DEBUG").is_some() {
        eprintln!("uu-native: {message}");
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

fn signal_body(event: &Value) -> &Value {
    let envelope = event.get(1).unwrap_or(event);
    envelope.get("data").unwrap_or(envelope)
}

fn route_matches(event: &Value, app_id: &str, ice_id: &str, client_id: &str) -> bool {
    let envelope = event.get(1).unwrap_or(event);
    let body = signal_body(event);
    body.get("ice_id")
        .and_then(Value::as_str)
        .is_none_or(|value| value == ice_id)
        && body
            .get("app_control_id")
            .and_then(Value::as_str)
            .is_none_or(|value| value == app_id)
        && envelope
            .get("client_id")
            .and_then(Value::as_str)
            .is_none_or(|value| value == client_id)
}

fn answer_sdp(body: &Value) -> Result<String, String> {
    if let Some(sdp) = body.get("sdp").and_then(Value::as_str) {
        return Ok(sdp.to_owned());
    }
    use base64::Engine;
    use std::io::Read;
    let encoded = body
        .get("gzip_sdp")
        .and_then(Value::as_str)
        .ok_or("answer has no SDP")?;
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "invalid compressed answer")?;
    let mut sdp = String::new();
    flate2::read::GzDecoder::new(compressed.as_slice())
        .take(1024 * 1024 + 1)
        .read_to_string(&mut sdp)
        .map_err(|_| "invalid compressed SDP")?;
    if sdp.len() > 1024 * 1024 {
        return Err("SDP exceeds 1 MiB".into());
    }
    Ok(sdp)
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
                credential_type: RTCIceCredentialType::Password,
                ..Default::default()
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(RTCConfiguration {
        ice_servers,
        ice_transport_policy: if value.get("force_relay").and_then(Value::as_bool) == Some(true) {
            webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy::Relay
        } else {
            webrtc::peer_connection::policy::ice_transport_policy::RTCIceTransportPolicy::All
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_signaling_filters_all_three_session_identifiers() {
        let event = json!(["soac", {"client_id":"host", "data":{"type":"answer", "ice_id":"ice", "app_control_id":"app", "sdp":"sdp"}}]);
        assert_eq!(signal_body(&event)["type"], "answer");
        assert!(route_matches(&event, "app", "ice", "host"));
        assert!(!route_matches(&event, "other", "ice", "host"));
        assert!(!route_matches(&event, "app", "other", "host"));
        assert!(!route_matches(&event, "app", "ice", "other"));
    }

    #[tokio::test]
    async fn turn_password_credentials_are_accepted_by_webrtc() {
        let config = rtc_configuration(&json!({"iceServers":[{"urls":"turn:relay.example.invalid:3478", "username":"test", "credential":"test"}], "force_relay":true})).unwrap();
        assert_eq!(
            config.ice_servers[0].credential_type,
            RTCIceCredentialType::Password
        );
        let pc = APIBuilder::new()
            .build()
            .new_peer_connection(config)
            .await
            .unwrap();
        pc.close().await.unwrap();
    }

    #[test]
    fn accepts_compressed_answers() {
        use base64::Engine;
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(b"v=0\r\n").unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(gzip.finish().unwrap());
        assert_eq!(answer_sdp(&json!({"gzip_sdp":encoded})).unwrap(), "v=0\r\n");
    }
}
