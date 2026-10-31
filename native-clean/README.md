# uu-remote-clean

This is an independent clean-room rewrite of the UU Remote host/viewer
protocol core. It does not include source from `uu-remote-linux` or NetEase
binaries. The reference implementation was used only to compare public wire
behavior and is not a dependency.

The default build contains the account API, signaling, WebRTC data channels,
host-side Echo negotiation, terminal sessions, and the complete file-transfer
RPC: compressed directory listings, upload/download blocks, resume/duplicate
handling, and basic file operations. Desktop capture and input injection are
separate optional modules; they are not enabled by default and do not request
OS permissions during build or startup.

The implementation is intentionally small at the boundary: protocol errors
are reported to the caller, while credentials and remote payloads are never
printed. The local config is read from `UU_CONFIG` or
`~/.config/uuremote/config.json`.

```sh
cargo test
cargo run -- sessions aeawsy72kyarx253
cargo run -- exec aeawsy72kyarx253 session1 whoami
cargo run -- files-list aeawsy72kyarx253 /tmp
cargo run -- files-upload aeawsy72kyarx253 ./local.txt /tmp/local.txt
cargo run -- files-download aeawsy72kyarx253 /tmp/local.txt ./copy.txt
cargo run -- host

# Optional desktop host (macOS Screen Recording + Accessibility, or the
# corresponding Windows capture/input permissions):
cargo run --features desktop -- host --desktop
```

`host` creates a UU room for the local account and accepts incoming SOAC
offers. `sessions`, `exec`, and the `files-*` commands are viewer commands.
`input DEVICE JSON` sends a desktop input event over the control channel when
the remote host was built with the `desktop` feature. Credentials are read
from `UU_CONFIG` or `~/.config/uuremote/config.json`; the file is never
committed or printed.
