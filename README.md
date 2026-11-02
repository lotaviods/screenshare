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
