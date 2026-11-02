// TypeScript mirrors of the Rust types exposed over Tauri IPC.
// Field names match the serde representation of the Rust structs exactly.

export type ConnectionMode = "mesh" | "sfu";
export type CaptureBackend = "auto" | "portal" | "x11" | "windows";
export type VideoCodec = "vp8" | "vp9" | "h264" | "h265" | "av1";
export type AudioCodec = "opus";
export type CursorMode = "hidden" | "embedded" | "metadata";

export type DeviceKind =
  | "microphone"
  | "audio_output"
  | "monitor"
  | "window"
  | "screen";

export interface Device {
  id: string;
  name: string;
  kind: DeviceKind;
}

export interface CaptureSource {
  id: string;
  name: string;
  kind: "monitor" | "window" | "screen";
}

export interface CaptureOptions {
  width: number;
  height: number;
  fps: number;
  cursor: CursorMode;
  codec: VideoCodec;
  bitrate_kbps: number;
  hardware_encoding: boolean;
}

export interface StunServer {
  url: string;
}

export interface TurnServer {
  url: string;
  username: string | null;
  credential: string | null;
}

export interface NetworkConfig {
  signaling_host: string;
  signaling_port: number;
  signaling_secure: boolean;
  stun_servers: StunServer[];
  turn_servers: TurnServer[];
  connection_mode: ConnectionMode;
}

export interface AudioConfig {
  mic_device_id: string | null;
  output_device_id: string | null;
  mic_volume: number;
  system_volume: number;
  echo_cancellation: boolean;
  noise_suppression: boolean;
  bitrate_kbps: number;
  codec: AudioCodec;
}

export interface VideoConfig {
  width: number;
  height: number;
  max_fps: number;
  max_bitrate_kbps: number;
  codec: VideoCodec;
  hardware_encoding: boolean;
}

export interface CaptureConfig {
  backend: CaptureBackend;
  last_monitor_id: string | null;
  last_window_id: string | null;
  system_audio: boolean;
}

export interface ServerAddress {
  host: string;
  port: number;
  secure: boolean;
}

export interface AppState {
  display_name: string;
  last_server: ServerAddress | null;
  last_room: string | null;
  recent_rooms: string[];
}

export interface Config {
  network: NetworkConfig;
  audio: AudioConfig;
  video: VideoConfig;
  capture: CaptureConfig;
  state: AppState;
}

export type ConnectionState =
  | "new"
  | "connecting"
  | "connected"
  | "disconnected"
  | "failed"
  | "closed";

export interface Participant {
  id: string;
  name: string;
  mic_on: boolean;
  screen_on: boolean;
  connection: ConnectionState;
}

export type CallEvent =
  | { type: "room_created"; room_id: string }
  | { type: "room_joined"; room_id: string }
  | { type: "room_deleted"; room_id: string }
  | { type: "participant_joined"; participant: Participant }
  | { type: "participant_left"; id: string }
  | { type: "participant_updated"; participant: Participant }
  | { type: "error"; message: string };
