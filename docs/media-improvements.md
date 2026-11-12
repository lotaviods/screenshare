# Media improvement roadmap

## Evidence and limits

An inspection of Discord Windows `app-1.0.9267` found native voice/media modules and helper executables. REA PE import evidence for `discord_voice.node` is `ev_1d12dae9e19c2b795ba9ff1b2367ac96d3f303fba6016de1db75d4f0b749b942`; GPU helper imports are `ev_459c5aa3fd6677d3b3332c8c2a84459b3800998e8fa582af1dd40e076df8fb8a`. The shipped voice JavaScript exposes video sink readiness, direct sinks, soundshare callbacks, encoder fallback callbacks and async Windows video device initialization. These are static observations: imports, executable names and exposed APIs alone do not establish which process runs a particular operation, zero-copy behavior, or actual runtime performance. No live Discord media session was observed.

## Priorities

1. **Preview:** bounded latest-frame delivery; avoid blocking capture and avoid stale UI updates; scale and encode off-thread. Measure UI and capture performance before promising zero-copy or smoothness. The Windows adapter exists; Linux currently uses an inline DMA-BUF preview. Moving it to a worker requires retaining PipeWire buffer ownership until processing completes, not just duplicating a DMA-BUF descriptor. Validate on a real Linux desktop.
2. **Encoder resilience:** choose available hardware/software implementations without changing the negotiated video codec; report initialization and runtime failures, fallback, and the effective encoder. Never silently claim hardware encoding.
3. **Device lifecycle and responsiveness:** perform blocking enumeration and initialization away from UI/actor control paths; propagate device-loss and capture failures and clean up streams, including on reconnect.
4. **Isolation:** consider a supervised helper process for native capture/encode or audio DSP, with bounded IPC, explicit lifecycle, crash recovery, and a threat/resource model. A worker thread does not contain native crashes.
5. **Audio processing:** implement echo cancellation/noise suppression only with a validated DSP pipeline, correct reference signal and clear controls; do not imply that the existing settings already work. Consider isolation after measuring cost and validating behavior.
6. **Settings and negotiation:** changes to a session codec must match capture and WebRTC negotiation. Reconnection/rejoin must preserve or explicitly stop media, handle stale events and failures, and have integration tests.

These are independent work items, not a claim that all are implemented. Track platform verification and remaining gaps in the change summary.

## Current increment

- Windows preview requests are demand-driven and use a best-effort nonblocking handoff; preview-worker startup failure no longer aborts sharing. JPEG processing remains CPU work and UI delivery still uses polling and an unbounded event channel: neither zero performance impact nor bounded end-to-end delivery is established.
- A local preview poll no longer clears the remote stage. Windows encoder initialization tries other available candidates for the same codec and logs its selection; this is not runtime fallback or a UI notification. Actual peer interoperability of every candidate is not established.
- The UI identifies echo cancellation and noise suppression as unavailable and labels the hardware preference as not applied. The existing saved settings remain for compatibility.
- Picker device/source enumeration is dispatched to blocking workers with at most one request in flight per type. Capture and audio stream initialization and hotplug checks can still block the backend actor.
- Windows WASAPI capture errors and finished audio/video workers are reported on health checks; Linux audio errors swallowed inside PulseAudio and Linux video failure details remain incomplete. Video packet delivery can now be interrupted on stop when its queue is full; overloaded audio drops new packets instead of blocking capture.
- Windows encoder-initialization fallback emits a nonfatal UI notice. Runtime fallback and peer interoperability for every candidate are not established.
- Explicit disconnect closes signaling, stops local streams and invalidates the old session's event forwarder. A local signaling/rejoin integration test covers room and advertised-state transitions, not real capture restoration or every UI race.
- The call sidebar samples selected ICE-pair STUN RTT every two seconds and graphs the highest available peer value. This is network round-trip time, not one-way latency or end-to-end media delay; unavailable values are not guessed.
- The call stage renders every active screen share as a tile: one fills the stage, two to four form a 2x2 grid, and larger calls use three columns. Clicking a tile focuses it and clicking it again (or "Show all") returns to the grid. Each tile is an independent decoded JPEG polled at 10 fps, so many simultaneous shares multiply UI decode work; per-tile downscaling and a shared-tile budget are not implemented.
- Linux preview isolation, fully asynchronous initialization, supervised processes, and audio DSP remain unimplemented. These require dedicated lifecycle and platform tests.
