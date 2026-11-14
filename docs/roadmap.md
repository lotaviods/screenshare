# Roadmap and decision log

Living document and handoff record. It must be enough for a new session or a
different agent to continue without re-deriving the analysis.

Rules: add a decision with the next free `D` number and never renumber it; mark
a decision superseded instead of deleting it and say why; tick plan items with
a date and the commit that landed them; append to the session log at the end of
every working session. Keep evidence next to each claim: a defect without a
file reference or a measurement is a hypothesis, not a finding.

This file is written in English to match the other documents in `docs/`. Chat
summaries for the project owner are given in Portuguese; the decisions here are
the source of truth.

Last updated: 2026-11-14.

## Status at a glance

| Area | State |
| --- | --- |
| Crash on call end | Root cause proven; P0.1 code landed in `5836c7b`, live PulseAudio lifecycle validation pending |
| Audio stutter | Playback worker/cadence, queue/write reporting, adaptive prebuffer, libopus PLC/FEC and RTP gap recovery landed in `5836c7b`; live session unverified |
| Video decode | Nonblocking overload policy, IDR recovery and self-view JPEG cap landed in `5836c7b`; runtime acceptance pending |
| Scale to 6-8 | Relay SFU forwards real RTP end-to-end; focus switching adds the new track and pauses the old subscription in automated 2-3 client tests. Mesh remains the default; keyframe requests, thumbnails and measurements remain open |
| Windows | Untested; release gate recorded (D11) |
| Code changes | Inherited working tree committed as `5836c7b`; SFU media-path tests and the codec pin are in `95a09d2` |

## Context

Reports in scope: crashes when a call ends, stuttering ("picotado") remote
audio, and a target of 6-8 participants. Linux is the only platform we can
validate today; Windows must keep building and be validated before release.

### Evidence on hand

- **Crashes.** Three core dumps on 2026-11-13 (`coredumpctl info 124619`,
  `128897`, `143125`) share one backtrace: `sharescreen-media` thread ->
  `playback::Output::drop` -> `pulse::Context::drop` -> `libpulse` ->
  `pa_iochannel_free`. Root cause is field drop order in
  `crates/audio/src/pulse.rs`: `Recorder`, `PlaybackSink` and `Mixer` declare
  `mainloop` before `context`, and `Context::new(&mainloop, ..)` only borrows
  the `pa_mainloop_api` that the `Mainloop` owns.
- **Video.** `~/.config/sharescreen/sharescreen.log` holds about 2,000
  `avcodec_send_packet failed: -1094995529` (`AVERROR_INVALIDDATA`) warnings and
  about 770 `Failed to send RtpPacket .. Full(..)` warnings from the
  `webrtc-rs` driver-to-track queue (capacity 256).
- **Audio.** No live measurement yet. Defects found by inspection are listed
  under D9/P0 below. The Discord comparison is a user report; we have no
  captured Discord media session, so any claim about what Discord does
  differently on this host remains inference (see `docs/media-improvements.md`
  for the same limitation).

## Decision log

Status values: **accepted**, **proposed**, **rejected**, **superseded**.

### D1 - Fix lifecycle and integration defects before changing libraries
**Accepted.** The proven crash is our RAII mistake, and the video failure is
our queue design. A library swap would carry both defects into the new stack.

### D2 - Keep `webrtc-rs` 0.21 as the client transport
**Accepted.** Peer connection, ICE, DTLS/SRTP and track plumbing work. The
`Full` warnings come from our writer blocking on a decoder queue, not from the
library.

### D3 - Keep `libpulse-binding` (Linux) and WASAPI (Windows) for audio I/O
**Accepted.** `cpal`/`rodio` do not cover per-application capture
(`pa_stream_set_monitor_stream`), which system-audio sharing depends on.
`pipewire-rs` stays a Linux-only option for later; it does not address the
crash.

### D4 - Mesh is rejected for the target group size
**Accepted.** At 6-8 participants mesh multiplies encoder and uplink per peer.
A single relay (SFU) is required. This is a product-shaping decision, not an
optimisation.

### D5 - The SFU is a relay-only forwarder in Rust
**Accepted, amended by D18.** Forward already-encoded RTP without transcoding.
The forwarder lives in the existing `server` crate next to signaling and TURN.
The transport choice is recorded in D18.

Existing groundwork, so a later reader does not start from zero:

- `deploy/compose.yaml` already defines an optional `sfu` service
  (`livekit/livekit-server`, profile `sfu`, ports 7880/7881) next to
  `signaling` and `coturn`. It is off by default and no client code uses it.
- `crates/core/src/types.rs` has `ConnectionMode::Sfu` and the settings UI has
  an "SFU" button. It selects the single-peer SFU client path at runtime.
- `PLAN.md` and `README.md` both state the intent to keep signaling and media
  decoupled so an SFU can replace the mesh without touching capture/audio.

Why not just use the LiveKit container that is already in the repo: the LiveKit
Rust SDK takes raw frames (`NativeVideoSource`) and encodes them internally,
which discards the VAAPI/DMA-BUF pipeline in D6 and changes the negotiated
codec. LiveKit stays the fallback if running our own SFU turns out to dominate
the work, but it is a pipeline change, not a drop-in. The comparison in P2.3 is
still open.

### D6 - The client keeps its own capture and encode pipeline
**Accepted.** VAAPI/DMA-BUF H.264 on Linux and the Windows encoder path stay.
A server SDK that takes raw frames and encodes internally would discard this
pipeline and change the negotiated codec.

### D7 - Audio must never be dropped by backpressure
**Accepted.** Dropping a video frame is invisible; dropping 20 ms of audio is
audible. Every audio hop (capture -> call -> track -> wire -> jitter buffer ->
device) needs a bounded queue with an explicit, counted policy, and audio
should be written before video when both are pending.

### D8 - Adaptive jitter buffering, not a fixed 60 ms target
**Accepted.** `PLAYBACK_TARGET_MS` is constant and the 200 ms ceiling drops the
oldest samples. Both need to become adaptive with visible statistics.

### D9 - RTP timestamps come from the encoder sample clock
**Accepted.** `TrackWriter::write` builds `Sample::new(Instant::now())`, so the
RTP timeline follows wall-clock write time instead of the sample count. Any
scheduling jitter in `hub_loop`, and any difference between the audio device
clock and wall clock, becomes timestamp jitter or slow drift at the receiver.
`EncodedSample::timestamp_micros` already carries the correct value.

### D10 - Opus runs with FEC and packet-loss concealment enabled
**Accepted.** `OpusEncoder::new` sets only rate, format, bitrate and channel
layout. With no in-band FEC and no PLC, any lost packet is a hard gap.

### D11 - Linux first, Windows gated before every release
**Accepted.** Development and measurement happen on Linux. A release requires
a Windows build plus a smoke test (join, mic, system audio, screen share, leave)
and the same RAII audit on `crates/audio/src/wasapi.rs`.

### D12 - SFU forwards only subscribed tracks
**Accepted.** Several participants may share a screen at the same time (product
answer, 2026-11-14). With 8 participants and N shares, "forward everything to
everyone" is not viable: each client would receive N x 2-4 Mbps and decode N
1080p streams. A client therefore subscribes only to the tracks it renders, and
what it renders is decided by D15 (one focus, the rest secondary). Audio is the
exception: every participant's audio is always subscribed, because you must hear
everyone regardless of which tile is focused.

The local decode budget is the real limit at this scale: the UI today decodes
every tile independently (see `docs/media-improvements.md`), so 8 live 1080p
tiles are not feasible regardless of bandwidth.

### D13 - Instrument the pipeline before promising quality
**Accepted.** Counters for audio queue drops, playback underruns, RTP queue
drops, decode errors and keyframe recovery. A quality claim without counters is
not verifiable.

### D14 - The SFU is co-located with signaling and coturn at first
**Accepted.** The deployment is already one host (`192.168.1.200` in
`deploy/compose.yaml`) running signaling plus coturn, provisioned through
`deploy/compose.yaml`. The SFU joins that compose file as another service and
shares the host until measurements say otherwise. This is an engineering call,
not a product decision: the project owner does not need to choose a topology,
since hosting is already centralised. Revisit only if relay CPU or bandwidth
becomes the bottleneck.

### D15 - One focused share at full quality, the rest as secondary tiles
**Accepted.** Product answer, 2026-11-14. Exactly one share is the focus at a
time and the user chooses it; every other active share is rendered as a small
secondary tile. This is the Teams/Zoom model and it simplifies the system more
than a "4 full-quality tiles" cap would:

- Subscription set per client is deterministic: one full-quality video track,
  N low-resolution thumbnail tracks, all audio tracks.
- No per-tile quality negotiation, no layer switching between many streams, and
  no arbitrary cap to explain in the UI. The previous "max 4" cap is dropped.
- Switching focus is a subscription swap plus a keyframe request (P3.3); the
  new focus appears in a few hundred milliseconds and the old one falls back to
  its thumbnail.
- Bandwidth per client with 8 shares: 1 x 2.5-4 Mbps focus + 7 x ~0.25 Mbps
  thumbnails + 8 x ~40 kbps audio, about 6 Mbps down. Uplink per publisher is
  about 4.3 Mbps.
- CPU per client: one full decode plus up to seven small ones. The small decodes
  are cheap, but the UI still needs a shared budget (P4.4) so a call does not
  pin every core while minimised.
- Keep one guard rail: skip thumbnail decoding entirely when the window is
  minimised or the call screen is not visible.

### D16 - Thumbnails are a second low-resolution track, not RID simulcast
**Accepted.** The publisher encodes each share twice: the focused stream
(1080p30, 2.5-4 Mbps) and a thumbnail stream (480p or 360p at 5 fps, about
200-300 kbps). The SFU routes the focused stream to the one viewer that has it
focused and the thumbnail stream to everyone else.

Why not simulcast with RIDs: `rtc` 0.21 does support send-side simulcast
metadata (`RTCRtpEncodingParameters` with `rid` and
`scale_resolution_down_by`), but it does not downscale pre-encoded samples for
you. Since we feed encoded frames from VAAPI, one low-resolution encode is
needed either way, so simulcast would add SDP and RID plumbing in the SFU for
the same CPU cost. Two tracks reuse the existing `add_track` API.

Cost accepted: two m-lines per share, and the publisher sends the thumbnail
even when nobody subscribes. If that waste matters, pause the full-quality
encode when the SFU reports no subscriber; do not build an adaptive layer
controller before measuring.

### D17 - One video codec per SFU room
**Accepted.** The relay forwards payload bytes without transcoding (D5), so a
viewer that negotiated a different video codec than the publisher would receive
an undecodable stream. The first `SfuOffer` in a room pins the video codec and
later offers with a different codec are rejected with an explicit signaling
error. Speakers use Opus in every room, so only video needs the pin. Evidence:
`SfuRoom::negotiate_video_codec` and
`server/tests/signaling_integration.rs::sfu_rejects_a_second_video_codec_in_the_same_room`.

### D18 - The relay transport is the shared webrtc-rs peer wrapper
**Accepted.** The earlier plan preferred `str0m`, but the desktop client
already relies on `WebRtcPeer` for ICE/DTLS/SRTP and packet pass-through (D2).
Reusing that wrapper on the server gives one transport stack, one ICE
implementation and one code path for both legs. The relay is still RTP
pass-through: `RtpForwarder` copies payloads, sequence numbers and timestamps,
and only payload type and SSRC are rewritten for the egress leg. The unused
`str0m` scaffolding was removed. Evidence:
`server/tests/sfu_relay.rs` (payload/sequence/timestamp survival) and
`server/tests/sfu_media_e2e.rs` (two clients through live signaling, plus a
third publisher added by a focus switch).

## Plan

### Phase 0 - Stability and audio correctness

Target: no crash, no silent audio loss, no periodic underrun on a 1 h session.

- [x] P0.1 Reverse the drop order in `Recorder`, `PlaybackSink`, `Mixer`
      (`crates/audio/src/pulse.rs`); add an explicit idempotent `Drop` and
      remove the duplicate `disconnect()` in `stop()` + `Drop`. Landed
      2026-11-14 in `5836c7b`; live PulseAudio close/reopen validation is still
      outstanding.
- [x] P0.2 Audit `crates/audio/src/wasapi.rs` for the same class of bug
      (native handle freed before its owner). Landed 2026-11-14 in `5836c7b`
      (source audit only; runtime validation is Windows-gated).
- [x] P0.3 Playback cadence: schedule against an absolute `next_deadline`
      (`next_deadline += 20 ms`, loop while behind) instead of
      `next_chunk = now + 20 ms`. Landed 2026-11-14 in `5836c7b`.
- [x] P0.4 Give playback its own thread; do not pace a device from the decode
      loop's `try_recv` + 5 ms sleep path (`crates/media/src/lib.rs`). Landed
      2026-11-14 in `5836c7b`.
- [x] P0.5 Stop silent audio drops: `Framer::push` treats `TrySendError::Full`
      as success (`crates/audio/src/native/mod.rs`); `PlaybackSink::play`
      ignores `write_copy` errors. Count and log both. Landed 2026-11-14 in
      `5836c7b`.
- [x] P0.6 RTP timestamps from `EncodedSample::timestamp_micros` (D9). Landed
      2026-11-14 in `5836c7b`.
- [x] P0.7 Adaptive jitter buffer with prebuffer, target and max derived from
      observed arrival jitter; remove the "silence + full re-prebuffer" stall
      after an underrun. Landed 2026-11-14 in `5836c7b`; the one-chunk recovery
      threshold still needs a live jitter measurement to tune.
- [x] P0.8 Opus: enable in-band FEC and PLC, keep DTX off until measured.
      Landed 2026-11-14 in `5836c7b`; live packet-loss behaviour is unverified.
- [x] P0.9 Separate audio and video write paths so a video burst cannot delay
      an audio packet (D7). Landed 2026-11-14 in `5836c7b` (per-stream hubs and
      channels; audio waits for capacity, video does not).
- [ ] P0.10 Lifecycle tests: open/use/close 100x for every audio handle; a
      decode thread that exits while audio output is open. Video and audio
      decoder lifecycles are covered by `crates/media/tests/decoder_init.rs`
      (`5836c7b`); audio-device handle cycling still needs a PulseAudio host.

Acceptance: 1 h two-peer session, zero crashes, zero audio queue drops, zero
playback underruns on a loopback test, counters exported. `audio_metrics()` and
`media_metrics()` export the counters; the session itself has not run.

### Phase 1 - Video resilience

- [x] P1.1 RTP reader never awaits the decoder queue; drop by policy, never by
      blocking the track reader. Landed 2026-11-14 in `5836c7b`.
- [ ] P1.2 Queue policy that keeps keyframes over stale P-frames. The current
      policy drops the newest packet when the queue is full and waits for the
      next IDR, which bounds damage but does not prefer an incoming keyframe.
- [x] P1.3 Decoder recovery: on repeated `AVERROR_INVALIDDATA`, discard to the
      next IDR and rebuild the context if it persists. Landed 2026-11-14 in
      `5836c7b`.
- [x] P1.4 Make the in-call self-view optional (it decodes the full stream
      again) or cap it to 720p15. The JPEG stage is capped at 15 fps; the
      optional switch remains a UI decision (open question 4). Landed
      2026-11-14 in `5836c7b`.

Acceptance: 10 min share, first frame under 1 s after join, decode error rate
under 1 %, no stuck decoder. Not measured.

### Phase 2 - SFU spike

- [x] P2.1 Relay one screen track plus two audio tracks between three clients
      through the `server` crate; no transcoding. Landed 2026-11-14 in the SFU
      media-path commit `95a09d2`:
      `server/tests/sfu_relay.rs` proves payload/sequence/timestamp survival
      through the forwarder, and `server/tests/sfu_media_e2e.rs` drives two
      real clients through live signaling (screen + mic) and a third publisher
      added by a focus switch. This is automated coverage, not a manual
      multi-host session.
- [ ] P2.2 Measure per-client CPU, memory and one-way latency at 3 peers.
- [ ] P2.3 Compare against the LiveKit path on paper only: what we would lose
      from D6, what we would gain in operability. Decide D5. D5 is accepted and
      amended by D18 for the transport; the LiveKit comparison is still open.

Acceptance: 3 clients, 10 min, no relay-induced decode errors, decision
recorded. Media relay is proven in tests; duration, decode-error rate and the
decision remain open.

### Phase 3 - Call layer rework

- [ ] P3.1 Replace mesh in `crates/call` with publish/subscribe against the
      SFU. The SFU path exists and is selected by `ConnectionMode::Sfu`, but
      mesh is still the default and both topologies coexist.
- [ ] P3.2 Subscription API and roster integration; join/leave while sharing.
      Subscription snapshots drive client renegotiation; join/leave while
      sharing has only automated control-plane coverage.
- [ ] P3.3 Reconnect and keyframe request on subscribe. Reconnect handling and
      PLI/FIR upstream requests are not implemented; a late subscriber waits
      for the encoder's next IDR.
- [x] P3.4 Keep `crates/webrtc` as the peer wrapper; the SFU changes the
      topology, not the codec handling. Confirmed 2026-11-14 (D18).
- [ ] P3.5 Focus switching: `set_focus(peer_id)` swaps the full-quality
      subscription and requests a keyframe; everyone else stays on the
      thumbnail track (D15). The swap, the new egress track and pausing the
      previous subscription are proven in `server/tests/sfu_media_e2e.rs`
      (2026-11-14, `95a09d2` and `27619bb`); keyframe requests and thumbnail
      tracks are still
      missing, and the 500 ms time is not asserted.

Acceptance: 4 clients on one relay; a late joiner gets video without a manual
keyframe; leaving mid-share does not disturb the others; focusing a different
share shows it in under 500 ms.

### Phase 4 - Scale to 6-8

- [ ] P4.1 Publish two tracks per share: focus stream (1080p30, 2.5-4 Mbps) and
      thumbnail stream (480p/360p at 5 fps, ~200-300 kbps) (D16).
- [ ] P4.2 Secondary tile strip in the UI, with exactly one focus at a time
      (D15). No cap on the number of secondary tiles; pause their decode while
      the window is minimised.
- [ ] P4.3 UI decode budget: shared jpeg/decode budget across tiles instead of
      one independent decoder pipeline per tile.
- [ ] P4.4 Skip the thumbnail encode when the SFU reports no subscriber, only if
      measurement shows the uplink waste matters.
- [ ] P4.5 Server capacity target and TURN interaction (`server/src/turn.rs`).
- [ ] P4.6 Soak: 8 peers, 1 h, audio underrun and drop counters at zero.

### Phase 5 - Windows

- [ ] P5.1 Build gate on Windows for every release (D11).
- [ ] P5.2 WASAPI capture and playback smoke test on real hardware.
- [ ] P5.3 Encoder selection and fallback visible in the UI, not only in logs.

## Metrics to expose

- audio: queued frames, drops, underruns, re-prebuffers, decoded gaps
- video: RTP packets dropped, access units discarded, decode errors, recoveries
- call: per-peer RTT, bitrate, packets lost (already partially sampled in the
  sidebar), SFU relay counters once it exists, focused-track subscription and
  time-to-first-frame after a focus switch

## Open questions

1. ~~May several participants share a screen at the same time?~~ **Answered
   2026-11-14: yes, several simultaneous shares are allowed.** Resolved by D12.
2. Target end-to-end latency for audio (conversational, about 150 ms, or
   relaxed)? Not yet answered; assume conversational and measure.
3. ~~Where will the SFU run, and is it co-located with TURN?~~ **Answered
   2026-11-14: engineering call, not a user decision.** Resolved by D14.
4. Should the self-view remain decoded from the outgoing stream by default?
   Not yet answered.
5. ~~How many simultaneous shares must be visible at full quality?~~ **Answered
   2026-11-14: one focus at full quality, the rest as secondary tiles (D15).**
   The earlier "max 4" cap is dropped.

## Working state and handoff

Purpose of this section: the project owner is not a systems engineer and work
may resume in a different session or with a different agent. Anything needed to
continue without re-deriving the analysis belongs here.

### State as of 2026-11-14

- The inherited working tree is committed as `5836c7b` ("feat(media):
  stabilize audio/video pipelines and scaffold relay SFU"). That commit holds
  P0.1, P0.2, P0.3-P0.9, P1.1, P1.3, P1.4, the RTP forwarder, the SFU control
  plane and the initial client SFU path.
- P0.1: dependent stream fields drop before `Context`, `Mainloop` drops last,
  and `PlaybackSink::stop`/`Drop` share an idempotent shutdown path. No live
  PulseAudio close/reopen loop has run.
- P0.3-P0.9: playback has a bounded worker and absolute cadence, the prebuffer
  target follows arrival variation and recovers with a one-chunk threshold,
  queue/write failures are counted, RTP sample timestamps follow encoder PTS,
  and libopus FEC/PLC is configured. `audio_metrics()` exports device underruns
  and `media_metrics()` exports video queue drops plus Opus gap/FEC/PLC/silence
  counters; neither is shown in the UI yet.
- P1.1/P1.3/P1.4: video RTP enqueue uses `try_send` with a cumulative drop
  counter; audio enqueue remains awaited. On video overflow the decoder waits
  for an H.264 IDR and recreates its context; after three decode errors it also
  waits for an IDR. The self-view JPEG stage is capped at 15 fps. Queue stress
  and the 10-minute share acceptance remain unverified.
- P0.2 audit: WASAPI declares the COM guard after the endpoint/client handles,
  so it is dropped last; capture and playback stop their clients in `Drop`.
  No PulseAudio-style owner ordering defect was found, but Windows build and
  device validation are unavailable here.
- SFU transport (D18): the server owns one `WebRtcPeer` answerer per
  participant and forwards already packetized RTP. `server/src/sfu.rs` holds
  the subscription policy, the codec pin (D17) and the bounded `RtpForwarder`.
  `server/src/lib.rs` wires publications, focus, per-viewer snapshots, ingress
  source tasks, egress writers, ICE buffering and cleanup.
- Proven relay behaviour, 2026-11-14, SFU media-path commit `95a09d2`:
  - `server/tests/sfu_relay.rs` drives publisher -> ingress peer -> forwarder ->
    egress peer -> subscriber and asserts the payload, sequence number and
    timestamp survive, with the egress SSRC rewritten.
  - `server/tests/sfu_media_e2e.rs` drives two real clients through the live
    WebSocket server (screen + microphone) and a third publisher whose track
    is added after a focus switch; it also asserts that the previously focused
    publisher stops forwarding. Both tests run in under two seconds.
  - `crates/webrtc/tests/loopback.rs` proves an answerer can pre-register
    egress tracks and fill the offerer's reserved receive-only slots.
- The unused `str0m` scaffolding (`RelayPeer`, `str0m` dependency) was removed
  in the media-path commit `95a09d2`; D18 records why.
- Client negotiation: `WebRtcPeer` adds receive-only transceivers and named
  relay tracks; remote tracks expose stream/track ids. The call layer now has
  the `ConnectionMode::Sfu` path with publication, offer/answer, ICE,
  subscription-driven renegotiation, focus signalling and track attachment.
  Mesh remains the default.
- Audio loss recovery: `OpusDecoder` dynamically loads libopus for PLC and FEC,
  with FFmpeg as fallback. The receive loop detects bounded forward RTP gaps,
  applies PLC for earlier losses and FEC from the next packet for the final
  one, and counts gaps/FEC/PLC/silence. Unit tests cover PLC/FEC and sequence
  wrap/reorder; live loss is unverified.
- Latest validation: `cargo test --workspace` passes, including the six
  signaling integration tests, the two SFU media-path tests and the WebRTC
  loopback tests. `cargo clippy --workspace --all-targets -- -D warnings`
  passes and `cargo fmt --all` is clean. `sharescreen-ui` starts and opens its
  Vulkan window; `sharescreen-server` binds `0.0.0.0:8080`. No live audio
  device, manual multi-host SFU session or long-running call was exercised.
- The crash diagnosis is proven by core dumps, but the fix has not been checked
  in a live PulseAudio close/reopen loop.

### Start here (next actions, in order)

1. Request a keyframe on subscribe/focus (P3.3): send PLI/FIR upstream from the
   ingress receiver when a new viewer is wired, so the first focused frame does
   not wait for the encoder's periodic IDR.
2. Show exported audio/media counters in the call sidebar and run the Phase 0
   acceptance session on a machine with PulseAudio (1 h, two peers, zero drops).
3. Measure P2.2 (CPU, memory, one-way latency at 3 peers) and write the P2.3
   LiveKit comparison.
4. Rework the call UI/default from mesh to SFU, then implement the two-encode
   thumbnail track (D16) and the shared decode budget (P4.3).
5. Run the 8-peer soak and complete the Windows build/device gates.

Do not treat the SFU as production-ready before keyframe requests and a
multi-host session exist. The automated tests prove the media path, not call
quality.

### Facts a new reader would otherwise re-discover

- `crates/audio/src/pulse.rs` wraps `libpulse-binding`, whose standard
  `Mainloop` is `!Send` and must outlive the `Context` and every stream it
  created. The fix declares dependent fields before the mainloop; live
  lifecycle validation remains open.
- `crates/media/src/lib.rs` spawns one `sharescreen-media` thread per remote
  track. Device pacing now runs on its own bounded playback worker.
- `crates/media/src/lib.rs` bounds remote RTP at 256 packets. Video uses
  nonblocking enqueue and counts drops; audio waits for capacity to avoid loss.
- `crates/webrtc/src/peer.rs` maps `EncodedSample::timestamp_micros` to a
  per-track monotonic anchor for `rtc::media::Sample.timestamp`.
- `crates/call/src/hub.rs` broadcasts each sample to every peer sequentially
  (`join_all`), and `crates/call/src/lib.rs` uses 64-slot channels per stream.
- In SFU mode the client registers its writers under the `__sfu__` key and
  publishes stable ids (`<self>-mic`, `<self>-system`, `<self>-screen`). The
  server keys source and forwarder routes by room + publication id.
- `server/src/lib.rs` sends subscription snapshots only to viewers with an
  established SFU peer, except while a room has publications. This keeps mesh
  message ordering intact.
- The server's answerer pre-registers egress tracks before setting the remote
  offer; `rtc` maps them onto the receive-only m-lines the client reserved.
  This is why the client must reserve enough audio/video slots up front.
- The SFU pauses a subscription by deactivating the viewer's forwarder entry
  instead of dropping the writer. A focus swap therefore stops the old
  publisher's packets while keeping the negotiated m-line reusable, so
  re-focusing an earlier publisher does not consume another receive slot.
- `rtc` 0.21 exposes send-side simulcast metadata (`RTCRtpEncodingParameters`
  with `rid` and `scale_resolution_down_by`) but does not downscale pre-encoded
  samples, so simulcast would not save an encode for us (D16).
- The settings UI has an "SFU" button and `crates/core` has
  `ConnectionMode::Sfu`; both now select the single-peer SFU path. Mesh remains
  the config default.
- Deployment is one host via `deploy/compose.yaml` (signaling + coturn, plus a
  disabled LiveKit `sfu` profile). `/home/usr_jg` is the only validated
  platform.
- Validation commands: `make test`, `make clippy`, `make fmt`, `make build`.
  Core dumps: `coredumpctl list`, `coredumpctl info <pid>`.
  Logs: `~/.config/sharescreen/sharescreen.log`.
- Tests that need local sockets can fail inside a restricted sandbox; the full
  suite (including `sfu_media_e2e`) passes outside it.

### Rules for continuing this document

- Add a decision with the next free `D` number; never renumber.
- Mark superseded decisions instead of deleting them, and say why.
- Tick plan items with a date and the commit that landed them.
- Append to the session log below on every working session, even a short one.

## Session log

### 2026-11-14

- Read `ARCHITECTURE.md`, `PLAN.md`, `README.md`, `docs/media-improvements.md`
  and the workspace layout.
- Inspected capture, audio, native-media, webrtc, call, media and UI code.
- Diagnosed the crash from three core dumps; root cause is drop order in
  `crates/audio/src/pulse.rs` (D1, P0.1).
- Counted the log evidence for video: ~2,000 `AVERROR_INVALIDDATA` and ~770
  `webrtc-rs` queue-full warnings.
- Found six independent audio defects (P0.3 to P0.9) that explain stutter
  without blaming the network.
- Product answers received: 6-8 participants, multiple simultaneous shares,
  Linux first with Windows required, evaluation only for now.
- Decisions recorded: D1 to D14. No code changed.
- Answered the open questions: several participants may share at once (D12 now
  accepted; low-resolution thumbnails became required, later resolved as a
  second track by D16 rather than simulcast); hosting stays a single host with
  the SFU co-located (D14), which is an engineering call rather than a question
  for the owner.
- Found that `deploy/compose.yaml` already contains a disabled LiveKit `sfu`
  service and that `ConnectionMode::Sfu` plus an "SFU" UI button already exist
  as an unused stub. Recorded under D5 so nobody redoes that discovery.
- Noted the two decisions the owner still needs to make: simultaneous shares
  visible at full quality (default 4) and whether the self-view stays on.
- Second round: owner chose "one focused share, the rest secondary", which
  replaces the 4-tile cap and became D15 (focus model) and D16 (thumbnail as a
  second low-resolution track instead of RID simulcast). Self-view is still
  undecided.
- Started P0.1: reordered PulseAudio resource fields so streams precede
  `Context` and `Mainloop` is last; made `PlaybackSink::stop` idempotent and
  used it from `Drop`. `cargo fmt --all` and `cargo check -p sharescreen-audio`
  passed. Live PulseAudio lifecycle and Windows validation remain outstanding;
  no commit exists yet.
- Continued implementation: isolated playback device pacing in a bounded
  worker, used absolute playback deadlines, adapted jitter prebuffer to packet
  arrival variation, shortened recovery prebuffer after underrun, counted and
  logged capture queue overflow and playback writes, configured libopus FEC,
  mapped outgoing RTP sample time from encoder PTS, and made video decoder
  queue submission nonblocking with a drop counter. `cargo test --workspace`
  passed (native-device smoke tests are ignored by the suite).
- Added H.264 IDR detection tests, video decoder recreation after repeated
  errors and recovery gating after queue saturation; capped self-view JPEG
  output at 15 fps. `cargo test -p sharescreen-media` passed.
- Audited WASAPI field/COM drop order and added the initial `str0m` peer and
  tested room subscription manager for the SFU, including SDP offer acceptance.
  str0m 0.23.1 compiles on this Linux host; no RTP transport or multi-client
  SFU session exists yet.
- `cargo test --workspace` passed after the SFU groundwork was added. The two
  audio device smoke tests remain ignored; this host has no PulseAudio server.
- Re-ran `cargo test --workspace` outside the sandbox after the sandbox denied
  local socket creation in `media_state`; the full suite passed. `cargo fmt
  --all`, `cargo check --workspace` and `git diff --check` also passed. The UI
  was launched once and initialized its Vulkan window; no live media session
  was exercised.
- Ran `cargo run -p sharescreen-ui`; the UI initialized Vulkan and opened. It
  was stopped with Ctrl+C after startup verification. No live call/audio or
  Windows hardware test was performed. No commit exists, so plan checkboxes
  remain open under the commit tracking rule.
- Added SFU control-plane signaling messages for publishing/unpublishing tracks
  and selecting focus; the server maintains room subscription policy and sends
  per-viewer snapshots after publication/focus changes. Added a WebSocket
  integration test for thumbnail selection and focus swap. Empty snapshots
  initially disrupted legacy message ordering; the server now sends snapshots
  only while a room has publications.
- The new SFU signaling integration test passed, then the complete
  `cargo test --workspace` passed outside the sandbox. The sandbox denied
  local socket creation, so the escalated test run was necessary. `cargo fmt
  --all`, `cargo check --workspace` and `git diff --check` passed. The UI was
  run again and initialized Vulkan. The client still ignores SFU subscriptions,
  and media forwarding is not implemented.
- Added packet-level pass-through to the WebRTC wrapper. The initial loopback
  test exposed that the egress sender rejects a source SSRC; the wrapper now
  remaps payload type and SSRC while preserving RTP payload, sequence and
  timestamp. Its WebRTC loopback test passes outside the sandbox. Added bounded
  server fan-out queues and tests for audio backpressure and counted video
  drops. The forwarding queues still have no wiring to room subscriptions or
  peer lifecycle.
- Implemented a dynamically loaded libopus decoder path for real PLC/FEC while
  retaining FFmpeg as a fallback. Remote audio now tracks RTP sequence gaps,
  uses PLC for all but the final missing packet and requests FEC from the next
  packet when supported; metrics distinguish PLC, FEC and silence fallback.
  `cargo test -p sharescreen-media -p sharescreen-native-media` passed,
  including the new sequence-gap and libopus loss-recovery tests. No live
  lossy network session was tested.
- Re-ran `cargo test --workspace` outside the sandbox after RTP fan-out and
  audio recovery changes; all workspace, WebSocket, WebRTC loopback and doc
  tests passed. Audio device smoke tests remain ignored, and there is no live
  media-loss or one-hour session evidence.
- Added the WebRTC wrapper APIs for receive-only transceivers and named relay
  tracks, plus remote stream/track id accessors. The focused transceiver test
  and `cargo check --workspace` passed. The client call topology remains mesh.
- Restored the missing SFU signaling peer lifecycle helpers in
  `server/src/lib.rs`: codec validation, offer/answer, ICE forwarding, pending
  ICE storage with a 128-candidate bound and flush after remote SDP, TURN
  configuration for the server-side peer, and cleanup on room/member departure.
- Wired the server SFU peer's incoming tracks into the bounded RTP forwarder
  and creates outgoing relay tracks for the peer's subscriptions when its SDP
  offer is accepted. This forwards packet payloads without decode/re-encode.
  It is only a server-side initial negotiation path: the desktop client still
  uses mesh and never sends `SfuOffer`/publication messages, renegotiation on
  focus or publication changes is absent, and the 3-client relay has not been
  exercised. P2.1 remains open until client signaling and a three-client test
  are completed.
- Added an initial SFU client path selected by the saved `ConnectionMode`: one
  server peer carries mic, system audio and screen publications plus two audio
  and one video receive slot. The client publishes the three stable track IDs,
  exchanges offer/answer/ICE, and attaches returned tracks to remote decoders.
  Mesh remains the default. This is not yet a complete multi-client call:
  clients joining later do not trigger renegotiation, focus snapshots are not
  applied, incoming SFU publications do not update roster screen state, and
  the one-video-slot setup only covers the single-screen spike. No live SFU
  session was exercised.
- The client now compares each `SfuSubscriptions` snapshot and sends a fresh
  offer when the subscribed track IDs change; it serializes renegotiations and
  retries once an in-flight offer receives its answer. The current server
  handles each offer by replacing that viewer's server-side peer. Focus
  switching, larger participant counts and runtime behavior
  still need an integration test.
- Subscription snapshots, including an empty set after the final unpublish,
  are now sent only to viewers with an established SFU peer. This preserves
  mesh signaling order while allowing SFU viewers to remove stale relay tracks.
  `CallSession::set_focus` exposes the server focus control.
- Wired stage-tile clicks through the UI backend to `CallSession::set_focus`.
  Clicking a remote share asks the SFU to focus that publisher; returning to
  the grid or focusing the local share clears the remote focus. Mesh ignores
  this control. UI focus signaling compiles but has not been exercised in a
  live SFU session.
- Kept `PeerPayload::MediaState` as a signaling-only roster update in SFU mode:
  room join and peer join send the local mic/screen state, and subsequent
  toggles broadcast updates. No mesh peer connection is created for these
  messages. This lets the UI discover active SFU screen publishers; runtime
  verification remains outstanding.
- The WebRTC peer wrapper now rejects H.265 and AV1 instead of silently
  registering VP8 for those configured codecs. Mesh offer/answer failures are
  surfaced as call errors. The current transport codec registry supports VP8,
  VP9 and H.264; encoder availability remains a separate platform check.
- When focus is cleared and no publisher has a thumbnail publication yet, the
  SFU subscription policy now keeps one deterministic full track (lowest
  publisher ID) subscribed. This prevents a single-share call from going blank
  on return to grid mode while thumbnail encoding is not yet available.
- The RTP forwarder now namespaces its route keys by room, tracks source tasks,
  aborts them on peer close or unpublish, and advances a per-route epoch when a
  publication is removed. Packets from a prior source generation are ignored
  after republishing the same track ID. This covers server-side cleanup by
  inspection; reconnect/unpublish behavior has not been exercised live.
- The client SFU offer now reserves 14 audio and 7 video receive slots, matching
  two audio tracks and one video track for each of seven remote participants.
  Empty negotiated slots do not create decoders. This is SDP capacity only;
  subscription policy, thumbnail production and 8-person performance remain
  unvalidated.
- Refined SFU subscription dispatch in `server/src/lib.rs`: viewers in rooms with
  active publications receive subscription updates, and established SFU peers
  receive final empty snapshots on unpublish without leaking empty updates to
  mesh clients. All 4 signaling integration tests pass.
- Fixed `crates/webrtc/tests/loopback.rs` test cases to pass supported video codecs
  (`Codec::Vp8`/`H264`) rather than `Codec::Opus` to `WebRtcPeer::new`, resolving
  the regression from strict video codec validation. All 5 loopback tests pass.
- Added 100x video and audio decoder lifecycle tests in `crates/media/tests/decoder_init.rs`
  proving leak-free repeated initialization, teardown and worker exit.
- Resolved workspace clippy warnings, eliminated unused dependencies, formatted
  with `cargo fmt`, and passed `git diff --check`.
- Ran the complete workspace test suite (`cargo test --workspace`) with all tests passing.
- Ran both applications: `sharescreen-server` started and bound its listener, and
  `sharescreen-ui` initialized Vulkan and opened its GUI window.

#### Continuation session (after the token-limit handoff)

- Fixed the failing `sfu_renegotiation_reuses_peer_connection_on_new_tracks`
  test: its offer was empty (no m-lines, therefore no ICE credentials) and it
  did not tolerate interleaved `SfuIceCandidate`/`SfuSubscriptions` messages.
  Added `recv_signaling` to the integration test.
- Committed the inherited working tree as `5836c7b` after a full
  `cargo test --workspace` pass.
- Added `crates/webrtc/tests/loopback.rs::answerer_fills_pre_offered_receive_slots`
  to prove the SFU mapping assumption: an answerer can pre-register send tracks
  and they fill the offerer's reserved receive-only m-lines.
- Added `server/tests/sfu_relay.rs`: publisher RTP -> ingress peer ->
  `RtpForwarder` -> egress peer -> subscriber, asserting payload, sequence and
  timestamp survive and the egress SSRC is rewritten.
- Added `server/tests/sfu_media_e2e.rs`: two real clients through the live
  WebSocket server exchange relayed screen + microphone RTP; a third publisher
  is added to a viewer after `SetFocus`, and its payload arrives on a new egress
  track. Both tests pass repeatedly (3 consecutive runs) and run in under a
  second.
- Added D17 (one video codec per room) and enforcement in
  `SfuRoom::negotiate_video_codec`, plus a signaling integration test for a
  mismatched second codec.
- Removed the unused `str0m` `RelayPeer` scaffolding and dependency; recorded
  D18 (relay transport is the shared webrtc-rs wrapper).
- Added `#[allow(clippy::too_many_arguments)]` to three pre-existing FFI-shaped
  helpers so `make clippy` (`-D warnings`) passes.
- Re-ran `cargo test --workspace`, `cargo clippy --workspace --all-targets --
  -D warnings`, `cargo fmt --all` and `git diff --check`; all pass. Started
  `sharescreen-ui` (Vulkan window) and `sharescreen-server` (binds :8080).
- Made focus swaps pause the previous subscription: `RtpForwarder` now keeps
  the negotiated writer and gates it with an `active` flag. Renegotiation
  activates currently subscribed tracks and deactivates the rest, so the old
  publisher stops forwarding without losing its m-line. Added a fan-out unit
  test and extended the three-client test to assert the unfocused publisher
  goes silent.
- Remaining gaps recorded above: keyframe requests, metrics in the UI,
  CPU/latency measurements, thumbnail encode, shared decode budget, 8-peer
  soak and Windows.

## Not validated

- Windows: no build or runtime check on this host.
- Multi-peer behaviour: the 3-peer and 8-peer targets are plans, not results.
  The automated SFU tests cover two clients plus a third focused publisher, not
  a manual multi-host call.
- The Discord comparison: no captured session; differences are inferred.
- Hardware capture/encode failure paths (DMA-BUF, VAAPI, WASAPI) under driver
  error conditions.
- Live PulseAudio close/reopen and audio-jitter measurements.
- Keyframe requests on subscribe, relay CPU/memory/latency measurements and
  thumbnail decoding/encoding behaviour.
