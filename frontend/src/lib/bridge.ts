import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  CallEvent,
  CaptureOptions,
  CaptureSource,
  Config,
  Device,
} from "./types";

/** Typed wrappers around the Rust Tauri commands. */
export const api = {
  getConfig: () => invoke<Config>("get_config"),
  updateConfig: (config: Config) => invoke<void>("update_config", { config }),

  listMicrophones: () => invoke<Device[]>("list_microphones"),
  listOutputs: () => invoke<Device[]>("list_outputs"),
  listSystemMonitors: () => invoke<Device[]>("list_system_monitors"),
  listCaptureSources: () => invoke<CaptureSource[]>("list_capture_sources"),

  startScreenShare: (source: CaptureSource, options: CaptureOptions) =>
    invoke<void>("start_screen_share", { source, options }),
  stopScreenShare: () => invoke<void>("stop_screen_share"),
  startMicrophone: (device?: string) =>
    invoke<void>("start_microphone", { device: device ?? null }),
  stopMicrophone: () => invoke<void>("stop_microphone"),
  startSystemAudio: (device?: string) =>
    invoke<void>("start_system_audio", { device: device ?? null }),
  stopSystemAudio: () => invoke<void>("stop_system_audio"),
  setMicVolume: (volume: number) => invoke<void>("set_mic_volume", { volume }),
  setMicMuted: (muted: boolean) => invoke<void>("set_mic_muted", { muted }),
  setSystemVolume: (volume: number) =>
    invoke<void>("set_system_volume", { volume }),

  connectServer: () => invoke<void>("connect_server"),
  disconnectServer: () => invoke<void>("disconnect_server"),
  createRoom: () => invoke<void>("create_room"),
  joinRoom: (roomId: string) => invoke<void>("join_room", { roomId }),
  leaveRoom: () => invoke<void>("leave_room"),
  deleteRoom: () => invoke<void>("delete_room"),

  /** Latest decoded video frame (JPEG) from a peer; empty when unavailable. */
  remoteFrame: (peerId: string) => invoke<ArrayBuffer>("remote_frame", { peerId }),
  setPeerVolume: (peerId: string, volume: number) =>
    invoke<void>("set_peer_volume", { peerId, volume }),
};

/** Subscribe to call events emitted by the Rust side. */
export function onCallEvent(
  handler: (event: CallEvent) => void,
): Promise<UnlistenFn> {
  return listen<CallEvent>("call-event", (event) => handler(event.payload));
}

/** Human-readable message for a rejected command. */
export function errorMessage(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return String(error);
}
