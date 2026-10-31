# Clean-room architecture

The rewrite is split into protocol, transport, and capability layers:

- `auth.rs` and `api.rs` sign requests and create/join rooms. They redact
  credentials from `Debug` output and never print tokens, cookies, or room URLs.
- `signal.rs` implements Engine.IO 4 / Socket.IO signaling, binary attachment
  substitution, heartbeats, and SOAC events.
- `peer.rs` is the viewer WebRTC peer. It creates one peer connection per UU
  session and keeps the five data channels separate.
- `host.rs` is the host WebRTC peer. It accepts offers and routes each incoming
  channel to a capability service.
- `wire.rs` and `control.rs` contain the independent protobuf/terminal framing
  code. `terminal_host.rs` owns host processes and session IDs. `file_host.rs`
  and `file_client.rs` implement the FILE_DATA_CHANNEL RPC, including zlib
  directory/file lists, upload/download block acknowledgements, resume and
  duplicate strategies, and filesystem operations.
- `desktop.rs` is an optional capability boundary. With `--features desktop`
  and `host --desktop`, the host captures the primary display with `scrap`,
  encodes H.264 with OpenH264, publishes a WebRTC video track, and maps the
  control-channel keyboard/mouse events to `enigo`. The viewer creates a
  recv-only transceiver and exposes incoming RTP payloads to the client API.

The default capability set is headless. It does not request desktop capture or
input permissions, and the host only advertises terminal/file capabilities.
Desktop support is opt-in at both build time and runtime; Screen Recording and
Accessibility permissions can therefore be granted only to the process that
needs them. Clipboard, PAM, audio and GUI integration remain outside this
clean-room core.

The original GPL repository is kept outside this crate as a behavioral
reference. No source file, generated binding, asset, or dependency is copied
into this MIT implementation.
