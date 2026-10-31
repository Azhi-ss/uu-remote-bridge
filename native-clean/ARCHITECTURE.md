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
  code. `terminal_host.rs` owns host processes and session IDs; `file_host.rs`
  implements the bounded directory RPC.

The default capability set is headless. It does not claim desktop capture,
input injection, clipboard, PAM, audio, or GUI support. Those capabilities can
be added behind the `desktop` feature without changing authentication or the
terminal/file protocol.

The original GPL repository is kept outside this crate as a behavioral
reference. No source file, generated binding, asset, or dependency is copied
into this MIT implementation.
