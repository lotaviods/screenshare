# Sharescreen Android

Native Android client for the Sharescreen signaling server. Connects to the
same WebSocket signaling server as the desktop app, participates in WebRTC
mesh calls, shares the screen via MediaProjection, and renders remote screen
shares with the Google WebRTC SDK.

## Prerequisites

- Android Studio (Hedgehog or newer) or standalone Android SDK + JDK 17
- Android device or emulator (API 24+, recommended API 33+ for screen share)
- A running sharescreen signaling server

## Build

Open the `android/` directory in Android Studio, or build from the command line:

```sh
cd android
./gradlew assembleDebug
```

If `gradlew` is missing, generate it first:

```sh
gradle wrapper
```

## Install

```sh
./gradlew installDebug   # installs on a connected device/emulator
```

Or drag the APK from `app/build/outputs/apk/debug/` onto your device.

## Run

1. Start the signaling server from the project root:

   ```sh
   cargo run -p sharescreen-server
   ```

2. Open the app on your device. Enter your name and the server address.

   - **Emulator:** `ws://10.0.2.2:3000/ws` (10.0.2.2 maps to host localhost)
   - **Physical device:** `ws://<your-laptop-ip>:3000/ws` (same Wi-Fi)

3. Tap **Connect**, then **Create Room** or enter a room ID and **Join Room**.

4. On the desktop app (`make run`), join the same room.

5. Use the mic and screen-share buttons. Remote screen shares appear full screen.

## Architecture

```
┌─ UI (Jetpack Compose) ────────────────────────────────┐
│  HomeScreen   ↔  CallViewModel (StateFlow)  ↔  CallScreen │
│                                                       │
│  CallScreen uses SurfaceViewRenderer for remote video    │
└───────────────────────┬───────────────────────────────┘
                        │
┌─ CallSession (mesh orchestration) ────────────────────┐
│  SignalingClient (OkHttp WebSocket)  →  SignalMessage   │
│  WebRtcManager (PeerConnectionFactory)                  │
│  • Mic: JavaAudioDeviceModule → AudioTrack              │
│  • Screen: ScreenCapturerAndroid → VideoTrack            │
│  • Remote: onAddTrack → VideoTrack → SurfaceViewRenderer │
│  Peer connections keyed by remote peer id               │
│  ICE candidate buffering + flush on remote description  │
│  MediaState broadcast (mic_on, screen_on)                │
└───────────────────────┬───────────────────────────────┘
                        │ WSS
                ┌───────▼───────┐
                │ Signaling server │  (Rust + axum, server/)
                └───────┬───────┘
                    ┌───┴───┐
                    ▼       ▼
              Desktop app  Android app
              (webrtc-rs)  (Google WebRTC)
```

### Protocol compatibility

The Android app uses the same JSON signaling protocol as the desktop client.
Messages are discriminated by `"type"` (SignalMessage) and `"kind"`
(PeerPayload), matching the Rust serde tags. SDP and ICE candidates are
JSON-encoded strings inside PeerPayload, identical to the desktop format.

### Codecs

Both sides negotiate VP8/VP9/H264 for video and Opus for audio. VP8 is the
common default for maximum compatibility between webrtc-rs and Google WebRTC.

## Permissions

| Permission | Purpose |
|---|---|
| INTERNET | WebSocket signaling + WebRTC |
| RECORD_AUDIO | Microphone capture |
| FOREGROUND_SERVICE | Screen capture service |
| FOREGROUND_SERVICE_MEDIA_PROJECTION | Screen capture (Android 14+) |
| POST_NOTIFICATIONS | Foreground service notification (Android 13+) |

## Known limitations

- System audio capture is not implemented (Android does not have a general
  loopback API without root). Only microphone audio is sent.
- The video codec is VP8 by default (no UI to change it yet).
- STUN uses Google's public server by default. Configure your own STUN/TURN
  for production.
- No reconnection logic (signaling drop returns to the home screen).
