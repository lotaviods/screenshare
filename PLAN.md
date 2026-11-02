# Plan — Gap Analysis

What exists, and what is missing, in backend and frontend. Tracks the full
spec against the current code. Checkboxes are off until implemented.

## Status snapshot

| Area | Status |
| --- | --- |
| `sharescreen-core` — domain model + `ScreenCapture`/`AudioCapture` traits | done (first-cut) |
| `sharescreen-config` — config, persistence, validation, tests | done |
| `sharescreen-tauri` — Tauri shell | done (commands, events, app state) |
| `frontend` — React + TS + Vite | done (screens, bridge, media rendering) |
| capture / audio / webrtc / signaling / server / deploy | everything except Windows audio/room-auth done |

---

## Backend — missing

### `crates/capture` — screen capture
- [x] Linux Wayland: XDG Desktop Portal `ScreenCast` via `ashpd`, PipeWire via
      GStreamer `pipewiresrc`, encode (`x264enc`/`vp8enc`/`vp9enc`),
      appsink → `EncodedSample`.
- [x] Hardware-encoder auto-detection (VAAPI/NVENC/QSV candidates → software
      fallback). Untested against real HW (none installed here).
- [x] Linux X11: native `ximagesrc` backend (whole-screen + `xid:` window).
- [ ] X11 per-monitor/per-window enumeration (needs XCB) — source list is
      whole-screen only; the portal path already offers a picker on X11.
- [x] Windows: `d3d11screencapturesrc` — DXGI monitors and WGC windows,
      monitor enumeration via `d3d11screencapturedeviceprovider`, window list
      via `EnumWindows`; `gdiscreencapsrc` fallback for monitors when the
      d3d11 element is absent. Encoders `mfh264enc`/`openh264enc`.
      **Untested** — needs a Windows host with GStreamer 1.22+.
- [x] Windows audio: `wasapi2src` mic + `loopback=true` system audio,
      enumeration via `wasapi2deviceprovider`. **Untested** (same reason).
- [x] Source enumeration — Wayland delegates to the portal picker.
- [x] Cursor mode (hidden / embedded / metadata) mapping.
- [x] Resolution + frame-rate + bitrate + codec selection.
- [ ] Dynamic monitor change (hotplug) while sharing.
- [x] Start/stop without restarting the call; portal session close + thread join.
- [x] Runtime backend selection from `CaptureBackend`.
- Note: H.264 is encoded as constrained-baseline Annex-B, which is what the
      WebRTC RTP payloader expects (it splits frames on start codes) and what
      the SDP advertises (`profile-level-id=42e01f`).

### `crates/audio` — microphone + system audio
- [x] Mic capture: PulseAudio/PipeWire (`pulsesrc`/`autoaudiosrc`) — runtime-verified.
- [x] System loopback: monitor source (`pulsesrc device=@DEFAULT_MONITOR@`) — runtime-verified.
- [x] Opus encode (`opusenc`) into `EncodedSample`.
- [x] Device enumeration (mic, output, system monitor) via `libpulse-binding` — runtime-verified.
- [x] Independent mic/system volume + mute (via `volume` element).
- [ ] Echo cancellation; noise suppression (needs `webrtc-audio-processing`).
- [ ] Device disconnect/reconnect handling without tearing down the call.
- [ ] Windows WASAPI mic + loopback capture.

### `crates/webrtc` — media transport (`webrtc` 0.21)
- [x] Peer connection lifecycle: `WebRtcPeer` (offer/answer/ICE primitives).
- [x] Track wiring: `EncodedSample` → `TrackLocalStaticSample` via `TrackWriter`
      (mic/system/screen).
- [x] Codec registration + `Codec` → RTP codec mapping (Opus/VP8/VP9/H264).
- [x] Connection-state reporting (→ `ConnectionState` via `PeerEvent`).
- [ ] Renegotiation: remove screen-share track mid-call (add works; remove + re-offer pending).
- [ ] Reconnection after transient network loss; ICE restart.
- [ ] Stats reporting (feeds UI quality indicators).
- [x] Cleanup on leave (peer connections closed, decoders detached).
- Note: only the configured video codec is registered, so the negotiated codec
      always matches what the capture pipeline encodes.
- Note: `RTCIceCandidate::to_json()` hardcodes empty `sdp_mid`; verify during signaling interop.
- Compile-verified only; loopback connectivity test needs the signaling layer.

### `crates/signaling` — client-side signaling
- [x] WebSocket client (tokio-tungstenite): connect/send/receive/disconnect detection.
- [x] Message schema shared with server (`SignalMessage` in `core::protocol`).
- [ ] Auto-reconnect after a dropped connection (disconnect is detected; retry pending).
- [ ] TURN credential fetch (ephemeral, never persisted).

### `server/` — signaling server (axum)
- [x] Room create/join/leave + participant roster.
- [x] Short room codes (6-char base-36 hash) and room deletion by any member
      (every peer gets `RoomDeleted`). Ownership is not tracked: peer ids are
      per-connection, so a reconnect would otherwise orphan the room.
- [x] SDP offer/answer + ICE candidate relay (no media proxying).
- [x] End-to-end integration test (two clients, create/join/relay).
- [x] Ephemeral TURN credentials (coturn shared-secret scheme) over WebSocket
      and HTTP, with a known-answer HMAC test.
- [ ] Optional room password / access token; input validation hardening.
- [ ] Auth hook (optional), session management.

### `deploy/` — Docker Compose infrastructure
- [x] `compose.yaml`: `signaling` + `coturn` by default, optional `sfu` profile.
- [x] `Dockerfile.signaling` (multi-stage, no GStreamer/WebKit in the image).
- [x] `.env.example` with all tunables.
- [x] Health check without curl/wget (`sharescreen-server --health-check`).
- [x] `README.md` with run instructions and TURN setup.
- Note: container *execution* is blocked in the dev sandbox, so the stack was
      verified by building the image and running the release binary directly.

### `crates/platform` — OS integration
- [ ] OS + session detection (Wayland / X11 / Windows).
- [ ] Portal availability detection (Hyprland/KDE/GNOME/Sway backends).
- [ ] Device enumeration glue (routes to capture/audio backends).

### `crates/media` — remote media ingestion + decode
- [x] Remote RTP → full RTP packet → GStreamer `appsrc` feeding.
- [x] Audio decode: Opus → PCM → `pulsesink` (you hear the other side).
- [x] Video decode: VP8/VP9/H264 → RGB → JPEG (latest frame per peer).
- [x] Per-peer remote audio volume.
- [x] Decode test with real VP8 RTP (GStreamer source → depayload → decode → JPEG).
- [x] Decode test with real H264 RTP (STAP-A aggregated, as WebRTC sends it).
- [ ] Audio playback not runtime-tested (needs a real output device).
- Note: the H.264 pipeline decodes the byte-stream directly. The GStreamer
      depayloader cannot build a decoder config without `sprop-parameter-sets`,
      which WebRTC does not send (the parameter sets arrive in-band).

### `crates/call` — call session (signaling + WebRTC mesh)
- [x] Connect to signaling; create/join/leave room.
- [x] One peer connection per remote peer; offer/answer + ICE relay via signaling.
- [x] Local media fan-out: mic/system/screen `EncodedSample` → every peer's track.
- [x] `CallEvent` emission (room created/joined, participant join/leave, connection state).
- [x] Remote track ingestion: remote tracks are attached to the media decoder.
- [x] Media-state relay: peers learn whether the other side is sharing a
      screen or has a live mic (`PeerPayload::MediaState`).
- Note: the screen track is negotiated up front, so stopping a share stops the
      samples rather than removing the transceiver (no renegotiation yet).

### `crates/network` — ICE / STUN / TURN config
- [x] `NetworkConfig` → signaling URL (`ws`/`wss`, host, port).
- [x] `NetworkConfig` → ICE server list (STUN + TURN with credentials).
- [x] `NetworkConfig` → `PeerConfig` for mesh peers.
- [x] Signaling URL with peer identity (id/name query params, percent-encoded).
- [x] Unique peer id generation.
- [ ] Mesh vs SFU connection-mode plumbing (SFU stub for now).
- Note: `platform` crate still deferred (session detection is thin).

### `frontend/src-tauri` — commands + events (real app logic)
- [x] Tauri commands: config get/update, list devices/sources, start/stop share,
      mic mute+volume, system volume.
- [x] Call commands: connect/disconnect server, create/join/leave room.
- [x] `CallEvent` → Tauri event bridge (`call-event`).
- [x] Local media routed into the call session when connected, discarded otherwise.
- [x] App state: `tokio::sync::Mutex` engines + sync config store.
- [x] Wired `core`/`config`/`capture`/`audio`/`call`/`network` crates into state.
- [x] Local mic/screen state reported to the call session so peers learn it.
- [x] Disconnect tears down peer connections and decoders.
- [ ] Tauri events for capture/audio state changes (pending).

### Tests
- [x] Signaling integration (two clients, one server).
- [x] Media-state relay integration (two clients, one server).
- [x] WebRTC loopback connectivity (offer/answer in-process) — real ICE+DTLS,
      connected, encoded Opus crossed the wire.
- [x] Video track payload type follows the track's codec.
- [x] Remote decode tests: VP8 RTP → JPEG, H264 RTP (STAP-A) → JPEG.
- [ ] Device enumeration (list mic/output/monitor on each platform).
- [ ] Capture start/stop + resource-cleanup test.
- [ ] Reconnection test.
- [ ] Manual matrix (Hyprland/KDE/GNOME/Sway/X11/Win10/Win11; mic-only,
      screen-only, screen+system-audio, mic+system-audio).

---

## Frontend

- [x] **Tauri bridge layer** — typed `invoke`/`listen` wrappers (`lib/bridge.ts`)
      + TS mirrors of the Rust types (`lib/types.ts`).
- [x] **State management** — React state for config, participants, room,
      connection status; no extra dependency.
- [x] **Home screen** — server status, host/port/WS toggle, join room, create
      room, recent rooms, participant list.
- [x] **Call screen** — participant list with connection state, mic toggle +
      volume, system-audio toggle, share start/stop, leave, settings.
- [x] **Screen-sharing dialog** — source picker, resolution, fps, bitrate,
      hardware-encode toggle, system-audio toggle. The codec is shown read-only:
      it is fixed per call (negotiated with the peer connection) and set in
      Settings.
- [x] **Settings screen** — identity, network (host/port/secure, STUN/TURN),
      audio devices + volumes, video quality, capture backend.
- [x] **Error / permission UX** — error banner, portal permission denials
      surfaced as command errors.
- [x] **Remote media rendering** — remote video is decoded to JPEG in Rust and
      polled by the UI onto a `<canvas>`; remote audio plays through the system
      output. Per-participant volume slider.
- Note: recent rooms are recorded by the Tauri layer when a room is joined.

---

## Open decisions / risks

1. **Media-to-UI path — resolved.** WebRTC is owned by Rust (`webrtc` crate),
   so the WebView does not use browser `getDisplayMedia`/`RTCPeerConnection`.
   Remote tracks arrive as raw RTP (`TrackRemoteStaticRTP`); Rust repackages
   them, decodes with GStreamer, and hands JPEG frames to the UI over IPC
   (`remote_frame`), which the frontend paints onto a `<canvas>`. Audio is
   decoded and played directly by GStreamer.
   - Cost: one JPEG encode/decode per frame and an IPC hop per frame.
   - Alternative if this becomes a bottleneck: a native video surface via a
     Tauri plugin (`gtk4paintablesink`-style), which removes the JPEG step.

2. **Encoding**: GStreamer is the assumed capture+encode layer (HW-accelerated,
   cross-platform). Confirmed only at the design level; not yet proven on Hyprland
   or Windows. First capture increment should validate the PipeWire + encoder path
   end-to-end.

3. **SFU** is a stub in the mesh-first design; keep signaling/media decoupled so
   `str0m`/`rheomesh`/LiveKit can replace the mesh without touching capture/audio.

4. **Windows** requires a Windows host to build/test; Linux-side code must not
   assume it compiles unverified.

## Suggested build order

1. `capture` — Linux Wayland (portal + PipeWire + GStreamer); prove encoder path.
2. `audio` — mic + system loopback (Linux first).
3. `webrtc` — mesh peer connections + track wiring + renegotiation.
4. `signaling` client + `server` (axum).
5. `tauri` commands/events + `network`/`platform` glue.
6. Frontend screens + bridge + media rendering (resolve decision #1).
7. Windows capture/audio.
8. `deploy/` compose + Dockerfiles.
9. Tests + manual matrix.
