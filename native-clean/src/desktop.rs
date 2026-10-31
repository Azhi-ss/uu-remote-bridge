//! Optional desktop capture and input injection.
//!
//! The protocol layer is always compiled, but the operating-system backends
//! are behind the `desktop` feature and an explicit runtime flag. This keeps
//! the default terminal host permission-free.

#[cfg(feature = "desktop")]
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
#[cfg(feature = "desktop")]
use std::time::Duration;
use tokio::task::JoinHandle;
#[cfg(feature = "desktop")]
use webrtc::media::Sample;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
#[cfg(any(feature = "desktop", test))]
use webrtc::{
    api::media_engine::MIME_TYPE_H264, rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
};

pub struct VideoTrack {
    pub track: Arc<TrackLocalStaticSample>,
    pub task: JoinHandle<()>,
}

/// Start capture only when the caller explicitly requested desktop support.
pub async fn start_video(enabled: bool) -> Result<Option<VideoTrack>, String> {
    if !enabled {
        return Ok(None);
    }
    #[cfg(feature = "desktop")]
    {
        return start_native_video().await.map(Some);
    }
    #[cfg(not(feature = "desktop"))]
    {
        Err("desktop support requires rebuilding with --features desktop".into())
    }
}

/// Apply one JSON control event. The clean-room host accepts the same stable
/// action names as the viewer protocol and ignores unrelated control frames.
pub fn apply_input(value: &Value) -> Result<bool, String> {
    if value.get("heartbeat").is_some() {
        return Ok(true);
    }
    #[cfg(feature = "desktop")]
    {
        return apply_native_input(value);
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = value;
        Err("desktop input requires rebuilding with --features desktop".into())
    }
}

/// Receive and render the remote H.264 stream as a true-colour ANSI terminal
/// view. This intentionally stays in the CLI: it is useful for smoke tests and
/// headless machines while leaving a GUI viewer as a separate layer.
pub async fn view_cli(device: &str, columns: usize) -> Result<(), String> {
    receive_cli(device, columns, None).await
}

pub async fn snapshot(device: &str, path: &str) -> Result<(), String> {
    receive_cli(device, 100, Some(path)).await
}

async fn receive_cli(device: &str, columns: usize, snapshot: Option<&str>) -> Result<(), String> {
    #[cfg(feature = "desktop")]
    {
        use openh264::{
            decoder::{Decoder, DecoderConfig, Flush},
            formats::YUVSource,
            OpenH264API,
        };
        use std::io::{self, Write};

        use crate::viewer_terminal::{InputState, TerminalView, Viewport};
        use crossterm::event::{Event, EventStream};
        use futures_util::StreamExt;

        let mut decoder = Decoder::with_api_config(
            OpenH264API::from_source(),
            DecoderConfig::default()
                .flush_after_decode(Flush::NoFlush)
                .debug(std::env::var_os("UU_NATIVE_H264_DEBUG").is_some()),
        )
        .map_err(|error| format!("cannot initialize H.264 decoder: {error}"))?;
        if snapshot.is_none() {
            TerminalView::check()?;
        }
        let mut client = crate::peer::Client::connect_with_options(device, true).await?;
        let terminal = if snapshot.is_none() {
            match TerminalView::enter() {
                Ok(terminal) => Some(terminal),
                Err(error) => {
                    client.close().await;
                    return Err(error);
                }
            }
        } else {
            None
        };
        let mut stdout = io::stdout();
        let mut events = snapshot.is_none().then(EventStream::new);
        let mut input = InputState::default();
        let mut terminal_size = crossterm::terminal::size().unwrap_or((100, 40));
        let mut viewport = None;
        let mut dimensions = None;
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|e| e.to_string())?;
        let termination = async {
            #[cfg(unix)]
            {
                terminate.recv().await;
            }
            #[cfg(not(unix))]
            {
                std::future::pending::<()>().await;
            }
        };
        tokio::pin!(termination);
        let mut shown = 0u64;
        let mut decoded = 0u64;
        let mut units = 0u64;
        let mut sent = 0u64;
        let mut held_keys = std::collections::BTreeSet::new();
        let mut last_render = std::time::Instant::now() - Duration::from_secs(1);
        let mut video_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let result = async { loop {
            tokio::select! {
                _ = &mut interrupt => break Ok(()),
                _ = &mut termination => break Ok(()),
                _ = tokio::time::sleep_until(video_deadline) => break Err("no desktop video received for 15 seconds".into()),
                event = async { events.as_mut().unwrap().next().await }, if events.is_some() => {
                    let event = event.ok_or("terminal input closed")?.map_err(|e| e.to_string())?;
                    if let Event::Resize(width, height) = event {
                        terminal_size = (width,height);
                        viewport = dimensions.map(|(w,h)| Viewport::fit(w,h,columns,terminal_size));
                        stdout.write_all(b"\x1b[2J").map_err(|e| e.to_string())?;
                    }
                    let batch = input.handle(event, viewport);
                    if batch.exit { break Ok(()); }
                    if viewport.is_some() {
                        for event in batch.events {
                            if event["action"] == "kbd_press" {
                                if let Some(key) = event["key"].as_u64() { held_keys.insert(key); }
                            }
                            tokio::time::timeout(Duration::from_secs(2),client.send_input(&event))
                                .await.map_err(|_| "desktop input send timed out")??;
                            if event["action"] == "kbd_release" {
                                if let Some(key) = event["key"].as_u64() { held_keys.remove(&key); }
                            }
                            sent += 1;
                        }
                    }
                }
                packet = client.next_video_packet() => {
                    let packet = packet?;
                    video_deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                    units += 1;
                    let frame = match decoder.decode(&packet) {
                        Ok(Some(frame)) => frame,
                        Ok(None) => continue,
                        Err(error) => {
                            if std::env::var_os("UU_NATIVE_DEBUG").is_some() { eprintln!("H264 decode: {error}"); }
                            continue;
                        }
                    };
                    decoded += 1;
                    let (width, height) = frame.dimensions();
                    if let Some(path) = snapshot {
                        let mut rgb = vec![0u8; width * height * 3];
                        frame.write_rgb8(&mut rgb);
                        let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
                        write!(file, "P6\n{width} {height}\n255\n").map_err(|e| e.to_string())?;
                        file.write_all(&rgb).map_err(|e| e.to_string())?;
                        eprintln!("desktop snapshot: {width}x{height} -> {path}");
                        break Ok(());
                    }
                    if last_render.elapsed() >= Duration::from_millis(100) {
                        let view = Viewport::fit(width,height,columns,terminal_size);
                        if viewport != Some(view) { stdout.write_all(b"\x1b[2J").map_err(|e| e.to_string())?; }
                        viewport = Some(view);
                        dimensions = Some((width,height));
                        let rgb = preview_rgb(&frame,view.columns,view.rows * 2);
                        render_ansi(&mut stdout, &rgb, view.columns, view.rows * 2)?;
                        shown += 1;
                        last_render = std::time::Instant::now();
                    }
                }
            }
        }
        }.await;
        // Release a held drag/modifier before closing the transport, even on input/video error.
        let mut releases = input.release_all();
        releases.extend(
            held_keys
                .into_iter()
                .map(|key| serde_json::json!({"action":"kbd_release","key":key})),
        );
        for event in releases {
            let _ =
                tokio::time::timeout(Duration::from_millis(250), client.send_input(&event)).await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(events);
        drop(terminal);
        client.close().await;
        if snapshot.is_none() {
            eprintln!("desktop viewer: {shown} frames rendered, {decoded}/{units} access units decoded, {sent} input events sent");
        }
        return result;
    }
    #[cfg(not(feature = "desktop"))]
    {
        let _ = (device, columns, snapshot);
        Err("desktop viewer requires rebuilding with --features desktop".into())
    }
}

#[cfg(feature = "desktop")]
fn render_ansi<W: std::io::Write>(
    output: &mut W,
    rgb: &[u8],
    width: usize,
    height: usize,
) -> Result<(), String> {
    let columns = width;
    let pixel_rows = height;
    output
        .write_all(b"\x1b[H")
        .map_err(|error| error.to_string())?;
    for row in (0..pixel_rows).step_by(2) {
        for column in 0..columns {
            let x = column * width / columns;
            let top = rgb_at(rgb, width, x, row * height / pixel_rows);
            let bottom = rgb_at(
                rgb,
                width,
                x,
                ((row + 1) * height / pixel_rows).min(height - 1),
            );
            write!(
                output,
                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀",
                top.0, top.1, top.2, bottom.0, bottom.1, bottom.2
            )
            .map_err(|error| error.to_string())?;
        }
        output
            .write_all(b"\x1b[0m\r\n")
            .map_err(|error| error.to_string())?;
    }
    let help = "Ctrl+] exit | Ctrl+G, key: Win+key | mouse / paste";
    write!(output, "\x1b[0m\x1b[2K{}", &help[..help.len().min(columns)])
        .map_err(|e| e.to_string())?;
    output.flush().map_err(|error| error.to_string())
}

#[cfg(feature = "desktop")]
fn rgb_at(rgb: &[u8], width: usize, x: usize, y: usize) -> (u8, u8, u8) {
    let index = (y * width + x) * 3;
    (rgb[index], rgb[index + 1], rgb[index + 2])
}

#[cfg(any(feature = "desktop", test))]
fn h264_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_H264.to_owned(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
            .into(),
        rtcp_feedback: Vec::new(),
    }
}

#[cfg(feature = "desktop")]
async fn start_native_video() -> Result<VideoTrack, String> {
    use openh264::{
        encoder::Encoder,
        formats::{BgraSliceU8, YUVBuffer},
    };
    use scrap::{Capturer, Display};
    use std::io::ErrorKind;
    use tokio::sync::mpsc;

    let display =
        Display::primary().map_err(|error| format!("cannot open primary display: {error}"))?;
    let width = display.width();
    let height = display.height();
    if width < 2 || height < 2 || width % 2 != 0 || height % 2 != 0 {
        return Err(format!(
            "unsupported capture size {width}x{height}; width and height must be even"
        ));
    }
    let track = Arc::new(TrackLocalStaticSample::new(
        h264_capability(),
        "desktop".into(),
        "uu-clean".into(),
    ));
    let (frame_tx, mut frame_rx) = mpsc::channel::<Vec<u8>>(2);
    std::thread::Builder::new()
        .name("uu-desktop-capture".into())
        .spawn(move || {
            let Ok(mut capturer) = Capturer::new(display) else {
                return;
            };
            let stride_sleep = Duration::from_millis(16);
            loop {
                match capturer.frame() {
                    Ok(frame) => {
                        let stride = frame.len() / height;
                        let mut packed = Vec::with_capacity(width * height * 4);
                        for row in 0..height {
                            let start = row * stride;
                            let end = start.saturating_add(width * 4).min(frame.len());
                            if end.saturating_sub(start) != width * 4 {
                                packed.clear();
                                break;
                            }
                            packed.extend_from_slice(&frame[start..end]);
                        }
                        if packed.len() == width * height * 4
                            && frame_tx.blocking_send(packed).is_err()
                        {
                            break;
                        }
                    }
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(stride_sleep)
                    }
                    Err(_) => break,
                }
            }
        })
        .map_err(|error| format!("cannot start capture thread: {error}"))?;
    let track_for_task = track.clone();
    let task = tokio::spawn(async move {
        let Ok(mut encoder) = Encoder::new() else {
            return;
        };
        while let Some(frame) = frame_rx.recv().await {
            let data = {
                let source = BgraSliceU8::new(&frame, (width, height));
                let yuv = YUVBuffer::from_bgra8_source(source);
                let Ok(bitstream) = encoder.encode(&yuv) else {
                    continue;
                };
                bitstream.to_vec()
            };
            if data.is_empty() {
                continue;
            }
            let sample = Sample {
                data: Bytes::from(data),
                duration: Duration::from_millis(33),
                ..Default::default()
            };
            let _ = track_for_task.write_sample(&sample).await;
        }
    });
    Ok(VideoTrack { track, task })
}

#[cfg(feature = "desktop")]
fn apply_native_input(value: &Value) -> Result<bool, String> {
    use enigo::{Axis, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|error| format!("cannot initialize input backend: {error}"))?;
    let result = match action {
        "text_input" => enigo.text(
            value
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        "mouse_move_absolute" => enigo.move_mouse(
            number(value, "abs_x"),
            number(value, "abs_y"),
            Coordinate::Abs,
        ),
        "mouse_move_relative" => enigo.move_mouse(
            number(value, "delta_x"),
            number(value, "delta_y"),
            Coordinate::Rel,
        ),
        "mouse_scroll" => enigo.scroll(number(value, "delta_y"), Axis::Vertical),
        "mouse_click" => enigo.button(mouse_button(value), Direction::Click),
        "mouse_press" => enigo.button(mouse_button(value), Direction::Press),
        "mouse_release" => enigo.button(mouse_button(value), Direction::Release),
        "kbd_click" => enigo.key(key_value(value), Direction::Click),
        "kbd_press" => enigo.key(key_value(value), Direction::Press),
        "kbd_release" => enigo.key(key_value(value), Direction::Release),
        _ => return Ok(false),
    };
    result
        .map(|_| true)
        .map_err(|error| format!("input action failed: {error}"))
}

#[cfg(feature = "desktop")]
fn number(value: &Value, key: &str) -> i32 {
    value
        .get(key)
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

#[cfg(feature = "desktop")]
fn mouse_button(value: &Value) -> enigo::Button {
    match value
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left")
        .to_ascii_lowercase()
        .as_str()
    {
        "right" | "r" => enigo::Button::Right,
        "middle" | "m" => enigo::Button::Middle,
        _ => enigo::Button::Left,
    }
}

#[cfg(feature = "desktop")]
fn key_value(value: &Value) -> enigo::Key {
    use enigo::Key;
    let key = value.get("key").and_then(Value::as_str).unwrap_or_default();
    match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Return,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "left" | "arrowleft" => Key::LeftArrow,
        "right" | "arrowright" => Key::RightArrow,
        "up" | "arrowup" => Key::UpArrow,
        "down" | "arrowdown" => Key::DownArrow,
        _ => key.chars().next().map(Key::Unicode).unwrap_or(Key::Space),
    }
}

/// A tiny protocol-level input parser test that does not require desktop permissions.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_is_accepted_without_native_backend() {
        assert!(apply_input(&serde_json::json!({"heartbeat": "kbd_heartbeat"})).unwrap());
    }

    #[test]
    fn codec_advertises_h264() {
        assert_eq!(h264_capability().mime_type, "video/H264");
    }
}

#[cfg(feature = "desktop")]
fn preview_rgb(
    frame: &impl openh264::formats::YUVSource,
    target_width: usize,
    target_height: usize,
) -> Vec<u8> {
    let (width, height) = frame.dimensions();
    let (ys, us, vs) = frame.strides();
    let mut rgb = Vec::with_capacity(target_width * target_height * 3);
    for row in 0..target_height {
        let y = row * height / target_height;
        for col in 0..target_width {
            let x = col * width / target_width;
            let luma = i32::from(frame.y()[y * ys + x]) - 16;
            let u = i32::from(frame.u()[(y / 2) * us + x / 2]) - 128;
            let v = i32::from(frame.v()[(y / 2) * vs + x / 2]) - 128;
            for component in [
                298 * luma + 409 * v,
                298 * luma - 100 * u - 208 * v,
                298 * luma + 516 * u,
            ] {
                rgb.push(((component + 128) >> 8).clamp(0, 255) as u8);
            }
        }
    }
    rgb
}
