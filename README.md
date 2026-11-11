# Sharescreen

Discord-like voice calls and high-quality screen sharing with system audio, in
Rust with a native [GPUI](https://gpui.rs/) desktop UI. Linux (Wayland and X11)
and Windows.

Status: **early foundation**. The workspace compiles. Domain model, config,
capture, audio, WebRTC, signaling, call orchestration, remote media decode, and
the GPUI desktop UI are implemented. Windows packaging, reconnection, and room
auth remain.

## Layout

- `crates/core` — domain model + trait boundaries (no OS/GStreamer deps).
- `crates/config` — configuration + persistent preferences + validation.
- `crates/capture` — screen capture backends.
- `crates/audio` — microphone + system loopback capture.
- `crates/webrtc` — peer connections and track wiring.
- `crates/call` — call session (signaling + WebRTC mesh).
- `crates/media` — remote media decode and playback.
- `crates/signaling` — client-side WebSocket signaling.
- `crates/network` — ICE / STUN / TURN configuration.
- `crates/ui` — GPUI desktop UI (binary `sharescreen`).
- `server/` — axum signaling server.
- `ARCHITECTURE.md` — full design decisions and roadmap.

## Build and test

```sh
cargo build --workspace
cargo test --workspace
```

## Develop

```sh
make run      # build and run the desktop app (needs a Wayland/X11 session)
make build    # release build
```

`make help` lists all targets.

Linux needs the GPUI and GStreamer system dependencies: a Vulkan driver,
Wayland or X11 client libraries, `xkbcommon`, `fontconfig`, `clang`, `cmake`,
and GStreamer 1.22+ with the base, good, bad, and ugly plugin sets.
On Arch: `pacman -S vulkan-icd-loader wayland libxkbcommon fontconfig clang cmake gstreamer gst-plugins-{base,good,bad,ugly}`.

### Windows

Prerequisites (PowerShell, no `make` needed):

```powershell
winget install Rustlang.Rustup Microsoft.VisualStudio.2022.BuildTools  # pick "Desktop development with C++"
```

The build downloads FFmpeg's shared build and links against it. No system
FFmpeg is needed. Run the portable build script:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\build-windows.ps1
```

The script stages the app in `dist\sharescreen-windows` with the FFmpeg DLLs
beside `sharescreen.exe`. The app needs no FFmpeg on the target machine.

To run the app directly from the source tree, set `FFMPEG_DIR` to the
extracted FFmpeg root first:

```powershell
$env:FFMPEG_DIR = "C:\path\to\ffmpeg-master-latest-win64-lgpl-shared"
cargo run
```

## Roadmap

See `ARCHITECTURE.md` for the full plan. Next steps, in order:

1. Windows packaging and installer.
2. Signaling reconnection and optional room auth.
3. SFU support behind the mesh-first signaling layer.
4. Docker Compose infrastructure.
