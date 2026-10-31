# uu-remote-clean

This is an independent clean-room rewrite of the UU Remote host/viewer
protocol core. It does not include source from `uu-remote-linux` or NetEase
binaries. The reference implementation was used only to compare public wire
behavior and is not a dependency.

The default build contains the account API, signaling, WebRTC data channels,
host-side Echo negotiation, terminal sessions, and a bounded directory RPC.
Desktop capture, input injection, audio, GTK and PAM are separate optional
modules; they are not enabled by default and do not grant OS permissions
during build or startup.

The implementation is intentionally small at the boundary: protocol errors
are reported to the caller, while credentials and remote payloads are never
printed. The local config is read from `UU_CONFIG` or
`~/.config/uuremote/config.json`.

```sh
cargo test
cargo run -- sessions aeawsy72kyarx253
cargo run -- exec aeawsy72kyarx253 session1 whoami
cargo run -- host
```

`host` creates a UU room for the local account and accepts incoming SOAC
offers. `sessions` and `exec` are viewer commands. Credentials are read from
`UU_CONFIG` or `~/.config/uuremote/config.json`; the file is never committed or
printed.
