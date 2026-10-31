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
use webrtc::{
    api::media_engine::MIME_TYPE_H264, rtp_transceiver::rtp_codec::RTCRtpCodecCapability,
    track::track_local::track_local_static_sample::TrackLocalStaticSample,
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
