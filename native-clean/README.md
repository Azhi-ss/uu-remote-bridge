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
cargo run --features desktop -- desktop-view aeawsy72kyarx253 100
```

`host` creates a UU room for the local account and accepts incoming SOAC
offers. `sessions`, `exec`, `desktop-view`, and the `files-*` commands are
viewer commands. `desktop-view` decodes the remote H.264 stream and renders a
true-colour ANSI preview in the terminal. `input DEVICE JSON` sends a desktop
input event over the desktop control channel. The official Windows host is
supported; a Rust host must be built with the `desktop` feature. Credentials are read
from `UU_CONFIG` or `~/.config/uuremote/config.json`; the file is never
committed or printed.

## Terminal desktop viewer

Build the desktop-enabled executable once:

```sh
cargo build --release --features desktop
../bin/uu-native-remote desktop-view DEVICE_ID 100
../bin/uu-native-remote desktop-snapshot DEVICE_ID ./desktop.ppm
```

The viewer uses H.264 over WebRTC and draws a true-colour ANSI preview in the
current terminal. It maintains the image aspect ratio, limits terminal output
to 10 frames/second, and restores the cursor and alternate screen on Ctrl-C
or error. `desktop-snapshot` writes a full-resolution PPM image. The official
Windows UU host works directly; the Rust host requires `host --desktop`.
This CLI does not play audio or forward local terminal keystrokes automatically.

Send one JSON event or an array of events with `input`. Windows uses numeric
virtual-key codes; absolute mouse coordinates are normalized to 0..1:

```sh
../bin/uu-native-remote input DEVICE_ID '{"action":"mouse_move_absolute","abs_x":0.5,"abs_y":0.5}'
../bin/uu-native-remote input DEVICE_ID '[{"action":"kbd_press","key":27},{"action":"kbd_release","key":27}]'
../bin/uu-native-remote input DEVICE_ID '{"action":"text_input","content":"hello"}'
```

Each CLI invocation opens its own connection. Finish `desktop-view` before
opening a separate `input` or file command. By default an occupied device
returns UU code 2002. `UU_NATIVE_FORCE_JOIN=1` explicitly requests takeover;
it is never enabled automatically. `UU_NATIVE_DEBUG=1` enables protocol-shape
and connection diagnostics without logging tokens or file contents.

The file commands use the dedicated file capture mode. Incoming RPCs from
CONTROL, TEXT and FILE are routed to the same file client; detached SCTP
readers accept the official host's larger blocks up to 256 KiB. Downloads use
a temporary staging directory, check declared sizes, and remove staging on
errors. `files-remove DEVICE_ID PATH` removes a remote path; if a host reports
an error after deletion, it verifies the resulting directory listing.

See [the SK live test record](LIVE-TEST-2026-10-31.md) for tested behavior and limits.
