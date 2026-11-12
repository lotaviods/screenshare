# Sharescreen Architecture

Discord-like voice + screen sharing desktop application. Rust owns the native
functionality, the media pipeline, and the GPUI desktop UI; WebRTC carries
real-time media.

## Goals

- Cross-platform: Linux (Wayland + X11, Hyprland/KDE/GNOME/Sway) and Windows 10/11.
- Low-latency screen sharing with system audio and microphone audio.
- Self-hostable signaling, STUN/TURN, and optional SFU.
- Configurable, modular, extensible to an SFU without a rewrite.

## Non-goals

- Chat/messaging. This is calls + screen sharing only, for now.
- MCU (mixing) — selective forwarding, not transcoding, if an SFU is added.
- iOS/Android/macOS in the first release.

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

## Key decisions

### WebRTC library: `webrtc` crate (webrtc-rs) v0.21

The `webrtc` crate is the recommended Rust client stack. It exposes an async
`RTCPeerConnection` (Tokio), maps cleanly to the browser model the UI and
server expect, and includes media track support (VP8/VP9/H264/H265, Opus),
ICE, DTLS-SRTP, NACK, TWCC, and bandwidth estimation.

- **Why not `str0m`**: str0m is a Sans-I/O core with no capture, encode, decode,
  render, or TURN client. It is an excellent fit for an SFU/server later, not
  for a desktop client that must capture and encode media. We keep signaling
  and media transport decoupled so an SFU built on `str0m`/`rheomesh`/LiveKit
  can replace the mesh later without touching capture/audio.
- Pin the exact 0.21 minor version: the 0.21 line is pre-1.0 and may break API
  on a minor bump.

### Linux capture: XDG Desktop Portal + PipeWire + native FFmpeg

Wayland capture is not compositor-specific. The standard path is
`org.freedesktop.portal.ScreenCast` + PipeWire, which Hyprland, KDE Plasma,
GNOME, and Sway all implement via their portal backends.

- `ashpd` crate: typed zbus wrapper for the ScreenCast portal. Creates a
  session, lets the user pick monitor/window (compositor-native permission
  dialog), starts the stream, and returns the PipeWire node id + remote fd.
- `pipewire` crate: consumes the captured frames.
- **Native GPU path** (`crates/native-media`): the compositor exports DMA-BUFs,
  FFmpeg imports them into VAAPI surfaces, `scale_vaapi` converts RGB to NV12
  on the GPU, and `h264_vaapi`/`vp8_vaapi`/`vp9_vaapi` encode. No pixel touches
  the CPU.
- The portal also covers X11 sessions, so there is no separate X11 backend.

The portal permission dialog is handled by the compositor; the app only observes
the result and surfaces `PermissionDenied` if the user declines.

### Windows capture

- GDI capture: `BitBlt` for monitors and the whole desktop, `PrintWindow` for
  windows. Monitor enumeration via `EnumDisplayMonitors`, window list via
  `EnumWindows`.
- Frames are copied to a BGRA DIB and encoded by FFmpeg's H.264/VP8/VP9
  encoder.
- WGC/DXGI and hardware Media Foundation encode are future upgrades; the GDI
  path is the first native Windows implementation.
- WASAPI for microphone capture; WASAPI loopback for system audio.

The `ScreenCapture` / `AudioCapture` traits hide all of this behind one
interface per platform; nothing platform-specific leaks into `core`, `webrtc`,
or the UI.

### Audio

Two independent local sources plus remote audio:

1. Microphone — `libpulse-binding` (Linux), WASAPI capture endpoint (Windows).
2. System audio — per-application PulseAudio/PipeWire streams (Linux), WASAPI
   process loopback excluding the Sharescreen process tree (Windows).

Both platforms record directly and encode with FFmpeg's libopus. Volume and
mute apply as software gain before encoding.

Both encode to Opus independently and are sent as separate WebRTC tracks so a
remote user can mute screen audio without muting the mic. Volume, mute, echo
cancellation, and noise suppression apply per source. Device disconnection is
surfaced as an event and the track is re-negotiated, not torn down blindly.

### Signaling: axum + WebSocket

The signaling server is a separate Rust crate (`server/`) using axum + tokio.
Responsibilities: room create/join, participant roster, SDP offer/answer relay,
ICE candidate relay, optional room password/token, and ephemeral TURN
credential provisioning. It never touches media.

### TURN/STUN: coturn

coturn provides STUN for discovery and TURN for relay. TURN credentials are
provisioned per-session by the signaling server (HMAC or REST auth) rather than
baked into the client, so long-lived secrets never sit in the desktop app.

### Configuration store

User preferences are a TOML file (`~/.config/sharescreen/config.toml`) via the
`directories` crate, with compiled-in defaults written on first launch. SQLite
is reserved for server-side room metadata only if in-memory proves insufficient
— not for desktop preferences.

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

## Media pipeline

Capture → encode → sample queue → WebRTC track. No raw frames cross the UI
boundary; only decoded JPEG frames for display.

```
portal/pipewire (Linux), GDI (Windows)  ──►  native encoder  ──►  EncodedSample
                                                                      │
                                                                      ▼
                                                              mpsc channel (bounded)
                                                                      │
                                                                  webrtc crate
                                                           TrackLocalStaticSample
                                                                      │ RTP
                                                                      ▼
                                                                  UDP sockets
```

`EncodedSample { kind, codec, data, timestamp_micros, keyframe }` is the shared
unit between capture/audio backends and the WebRTC layer, defined in `core`.

Remote tracks are the mirror image: RTP packets → native depacketizer →
FFmpeg decode → RGB → JPEG for the UI, or Opus → PCM → native audio output.
Decoding lives in `crates/native-media`.

## Signaling protocol (draft)

JSON messages over WebSocket:

- `client → server`: `create_room`, `join_room { room_id, token? }`,
  `leave_room`, `offer { to, sdp }`, `answer { to, sdp }`,
  `ice_candidate { to, candidate }`, `get_turn_credentials`.
- `server → client`: `room_created`, `joined`, `peer_joined`, `peer_left`,
  `offer`, `answer`, `ice_candidate`, `turn_credentials`, `error`.

The exact schema is implemented with `serde` in `core`/`signaling` and shared by
the client and server.

## Security

- WSS for production signaling (plain WS only for local dev).
- Ephemeral TURN credentials provisioned by the server; never persist long-lived
  TURN secrets in the client config.
- Room access tokens / optional passwords; the server validates input and
  enforces ownership, so no unauthenticated room administration.
- Local config holds no secrets by default.

## Deployment

`deploy/compose.yaml` defines independently toggleable `signaling`, `coturn`,
and (optional) `sfu` services, with env-var config, health checks, restart
policies, and a `.env.example`. The app also works against externally hosted
infrastructure without Docker.

## Testing matrix

Automated: config validation + persistence, signaling integration, WebRTC
connectivity (loopback), device enumeration, capture start/stop, reconnection,
and resource-cleanup tests.

Manual: Hyprland Wayland, KDE Plasma Wayland, GNOME Wayland, Sway, X11,
Windows 10, Windows 11 — for mic-only, screen-only, screen+system-audio, and
simultaneous mic+system-audio calls.

## Roadmap

1. Architecture doc + workspace scaffold (this increment).
2. `core` domain model + traits, `config` persistence + validation.
3. Linux Wayland capture (portal + PipeWire + native FFmpeg).
4. Windows capture (GDI + FFmpeg).
5. Microphone + system audio capture.
6. WebRTC media transport (mesh).
7. Signaling server + client.
8. GPUI desktop UI + backend actor.
9. Docker Compose infrastructure.
10. Automated tests + manual testing matrix.
