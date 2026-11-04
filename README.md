# Sharescreen

Discord-like voice calls and high-quality screen sharing with system audio, in
Rust + Tauri. Linux (Wayland and X11) and Windows.

Status: **early foundation**. The workspace compiles; `core` and `config` are
implemented and tested. Capture, audio, WebRTC, signaling, Tauri UI, and
infrastructure are the next increments.

## Layout

- `crates/core` — domain model + trait boundaries (no OS/Tauri/GStreamer deps).
- `crates/config` — configuration + persistent preferences + validation.
- `ARCHITECTURE.md` — full design decisions and roadmap.

## Build and test

```sh
cargo build
cargo test
```

## Develop and bundle (Tauri)

First run:

```sh
make setup    # pnpm install (frontend deps + Tauri CLI)
```

Then:

```sh
make dev      # Tauri dev with hot reload (needs a Wayland/X11 session)
make bundle   # production bundle (AppImage on Linux)
```

`make help` lists all targets. Linux needs the Tauri system deps:
`webkit2gtk-4.1`, `javascriptcoregtk-4.1`, `gtk3`, `libsoup3`, `gdk-pixbuf`.
On Arch: `pacman -S webkit2gtk-4.1 gtk3 libsoup3`.

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
pnpm --dir frontend install
pnpm --dir frontend tauri dev
```

To create the portable Windows ZIP, run this from PowerShell on a Windows
x86_64 machine with the prerequisites above:

```powershell
.\scripts\build-windows-portable.ps1
```

It creates a ZIP with the app and GStreamer runtime in `dist/`. Recipients
extract it and run `sharescreen-tauri.exe`. Windows WebView2 Runtime is still
required (normally present on Windows 11). The recipient does not need the
source checkout, Rust, or Node.js.

For Linux, use the existing `make bundle` target documented above.

When running a developer build, put
`%GSTREAMER_1_0_ROOT_MSVC_X86_64%\bin` on `PATH`. The portable package sets up
the bundled runtime automatically.

Icons are regenerated from `frontend/src-tauri/app-icon.png` via `make icons`.

## Roadmap

See `ARCHITECTURE.md` for the full plan. Next steps, in order:

1. Linux Wayland capture (XDG Desktop Portal + PipeWire + GStreamer).
2. Windows capture (Windows.Graphics.Capture / DXGI).
3. Microphone + system audio capture.
4. WebRTC media transport (mesh).
5. Signaling server (axum) + client.
6. Tauri commands/events + React/TypeScript frontend.
7. Docker Compose infrastructure.
