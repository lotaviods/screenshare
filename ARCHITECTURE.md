# Sharescreen Architecture

Discord-like voice + screen sharing desktop application. Rust owns the native
functionality, the media pipeline, and the GPUI desktop UI; WebRTC carries
real-time media.

## Goals

- Cross-platform: Linux (Wayland + X11, Hyprland/KDE/GNOME/Sway) and Windows 10/11.
- Low-latency screen sharing with system audio and microphone audio.
- Self-hostable signaling, STUN/TURN, and optional SFU.
- Configurable, modular, extensible to an SFU without a rewrite.

## High-level architecture

```
┌────────────────────────────── Desktop app (GPUI) ──────────────────────────────┐
│                                                                                │
│  GPUI UI (crates/ui)  ── commands/events ──►  backend actor (Tokio thread)      │
│                                                  │                             │
│              ┌───────────────────────────┼───────────────────────────┐         │
│              ▼                           ▼                           ▼         │
│        capture crate               audio crate                webrtc crate     │
│   (portal/PipeWire, GDI)  (mic + system loopback)   (peer connections)    │
│              │                           │                           │         │
│              └───────────────►  native encode   ◄──────────────────┘         │
│                                          │                                     │
│                                    signaling crate                             │
│                              (WebSocket client to server)                      │
└──────────────────────────────────────────┬─────────────────────────────────────┘
                                           │ WSS
                                 ┌─────────▼─────────┐
                                 │  signaling server  │  Rust + axum
                                 │  (rooms, relay)    │
                                 └─────────┬─────────┘
                    ┌──────────────────────┼──────────────────────┐
                    ▼                      ▼                      ▼
                STUN (coturn)          TURN (coturn)        optional SFU
            NAT discovery          relay fallback        (LiveKit / rheomesh)
```

Media flows peer-to-peer through WebRTC. The signaling server never proxies
media, only SDP/ICE/room state. TURN relays only when direct paths fail.

## Workspace layout

```
sharescreen/
├── crates/
│   ├── core/        # domain model + traits (no OS/media deps)
│   ├── config/      # config + preferences persistence + validation
│   ├── capture/     # ScreenCapture trait impls (portal/PipeWire, WGC)
│   ├── audio/       # AudioCapture trait impls (mic + system loopback)
│   ├── webrtc/      # peer connection wrapper, track wiring, renegotiation
│   ├── signaling/   # client-side WebSocket signaling
│   ├── call/        # call session: signaling + WebRTC mesh orchestration
│   ├── media/       # remote track decode and playback
│   ├── network/     # ICE/STUN/TURN config, candidate handling
│   └── ui/          # GPUI desktop UI + backend actor
├── server/          # axum signaling server (independent of the app)
├── deploy/          # Docker Compose, Dockerfiles, .env.example
├── vendor/          # patched crates (xattr ENOATTR fix, ffmpeg-sys-next D3D)
└── Cargo.toml       # workspace
```

Dependency direction is one-way: `core` is the lowest layer; `config` depends on
`core`; capture/audio/webrtc/media/call depend on `core`; `ui` composes
everything. No crate named `core` collides with the builtin `core` crate because
packages are `sharescreen-core`, etc.

The UI runs on the GPUI main thread. All capture, audio, WebRTC, and signaling
work runs in a backend actor on a dedicated Tokio thread, communicating through
`Command`/`UiEvent` channels, so the UI never blocks on media or network I/O.
