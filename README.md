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
winget install gstreamerproject.gstreamer                              # GStreamer MSVC x86_64
```

The winget GStreamer package installs per-user and does not set its
environment variables, so point the build at it once (new terminal after):

```powershell
$gst = "$env:LOCALAPPDATA\Programs\gstreamer\1.0\msvc_x86_64"
[Environment]::SetEnvironmentVariable("GSTREAMER_1_0_ROOT_MSVC_X86_64", $gst, "User")
[Environment]::SetEnvironmentVariable("PKG_CONFIG_PATH", "$gst\lib\pkgconfig", "User")
[Environment]::SetEnvironmentVariable("Path", [Environment]::GetEnvironmentVariable("Path", "User") + ";$gst\bin", "User")
```

Then run the app:

```powershell
cargo run
```

The app loads the GStreamer DLLs from `%GSTREAMER_1_0_ROOT_MSVC_X86_64%\bin`,
so the `bin` directory must be on `PATH` when it starts.

## Roadmap

See `ARCHITECTURE.md` for the full plan. Next steps, in order:

1. Windows packaging and installer.
2. Signaling reconnection and optional room auth.
3. SFU support behind the mesh-first signaling layer.
4. Docker Compose infrastructure.
