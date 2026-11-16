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
│  (GStreamer or portal/GDI) (mic + system loopback)  (peer connections)   │
│              │                           │                           │         │
│              └──────────►  GStreamer encode (Linux)  ◄───────────────┘         │
│                             FFmpeg encode (Windows)                            │
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
│   ├── capture/     # ScreenCapture trait impls (GStreamer, portal/PipeWire, GDI)
│   ├── audio/       # AudioCapture trait impls (mic + system loopback)
│   ├── gst/         # GStreamer pipeline runner shared by capture and audio
│   ├── processing/  # FFmpeg file processing: probe, thumbnails, convert, compress
│   ├── native-media/ # native GPU encode/decode primitives (FFmpeg, PipeWire)
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

## Media stack

Two libraries, split by latency requirement:

| Responsibility | Technology |
| --- | --- |
| Screen, microphone capture | GStreamer (`pipewiresrc`, `pulsesrc`) |
| System audio capture | native PulseAudio mixer |
| Real-time encoding | GStreamer (`x264enc`, `vp8enc`, `vp9enc`, VAAPI when installed) |
| WebRTC transport | `webrtc-rs` (`sharescreen-webrtc`), unchanged |
| Recording, conversion, post-processing, thumbnails, compression | FFmpeg (`sharescreen-processing`) |

GStreamer feeds the existing `webrtc-rs` transport through an `appsink`: the
encoder produces whole access units, `sharescreen-gst` hands them over as
plain Rust data, and `sharescreen-capture` copies them into `EncodedSample`.
The negotiated codec is unchanged, so the SFU and every peer keep working. The
WebRTC stack stays replaceable because no GStreamer type crosses a crate
boundary.

`CaptureEngine::Auto` prefers the GStreamer backend on Linux when the runtime
and `pipewiresrc` are installed; the explicit `Portal`/`X11` backend keeps the
native portal + FFmpeg VAAPI path as a fallback. When a share asks for hardware
encoding and this GStreamer install has no hardware encoder for the codec,
`Auto` starts the native VAAPI path instead: software `x264enc` at 2560x1440@60
measured 169% CPU here, which lags the whole desktop. If the native start fails
the share stays on the GStreamer software encoder. Microphone capture prefers
GStreamer `pulsesrc` and falls back to `libpulse`. System audio deliberately
stays on the native mixer: it records each application playback stream and
excludes the call's own output, which a plain monitor source would echo back to
peers.

`sharescreen-processing` owns everything that is not real time: `probe`,
`thumbnail_jpeg`, `convert` (video re-encode, audio stream copy) and `compress`.

Known limits of this cut, stated rather than hidden:

- Windows still runs the native GDI → FFmpeg path. GStreamer is a Linux-only
  dependency (`cfg(target_os = "linux")`) until the MSVC runtime is packaged and
  the Windows CI image can build it.
- Live recording is not wired. `sharescreen-processing` handles files; muxing
  the live session needs encoded-packet plumbing into the call session.
- The GStreamer screen path publishes no JPEG self-view preview and no separate
  thumbnail stream. Thumbnails are an optimization; full frames still flow.
- GStreamer encoders expose no portable force-keyframe call, so a keyframe
  request is logged and served by the next periodic GOP keyframe.
- Hardware encode (`vah264enc`/`vaapih264enc`, `vavp8enc`, `vavp9enc`) is used
  only when the element exists; otherwise the software encoder runs, unless
  hardware encoding was requested and FFmpeg has a VAAPI encoder, in which case
  `Auto` falls back to the native path. The GStreamer hardware branch is
  untested on this workstation because no VAAPI plugin is installed.

