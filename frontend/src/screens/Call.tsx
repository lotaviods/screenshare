import { useEffect, useRef, useState } from "react";

import { api, errorMessage } from "../lib/bridge";
import type {
  CaptureOptions,
  CaptureSource,
  Config,
  Participant,
} from "../lib/types";
import { initials } from "./Home";
import ShareDialog from "./ShareDialog";

interface CallProps {
  config: Config;
  roomId: string;
  participants: Participant[];
  onLeave: () => void;
  onDeleteRoom: () => void;
  onOpenSettings: () => void;
  onSaveConfig: (config: Config) => void;
}

function MicStatus({ on }: { on: boolean }) {
  return (
    <span
      className={on ? "mic-status on" : "mic-status off"}
      role="img"
      aria-label={on ? "Microphone on" : "Microphone muted"}
      title={on ? "Microphone on" : "Microphone muted"}
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <path d="M12 2a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z" />
        <path d="M5 10v1a7 7 0 0 0 14 0v-1M12 18v4m-4 0h8" />
        {!on && <path d="m4 4 16 16" />}
      </svg>
    </span>
  );
}

export default function Call({
  config,
  roomId,
  participants,
  onLeave,
  onDeleteRoom,
  onOpenSettings,
  onSaveConfig,
}: CallProps) {
  const [micOn, setMicOn] = useState(false);
  const [systemOn, setSystemOn] = useState(false);
  const [sharing, setSharing] = useState(false);
  const [shareOpen, setShareOpen] = useState(false);
  const [micVolume, setMicVolume] = useState(config.audio.mic_volume);
  const [status, setStatus] = useState<string | null>(null);
  const [roomCopied, setRoomCopied] = useState(false);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const sharer = participants.find((p) => p.screen_on);

  const copyRoomId = async () => {
    try {
      await navigator.clipboard.writeText(roomId);
      setRoomCopied(true);
    } catch (e) {
      setStatus(errorMessage(e));
    }
  };

  // Poll the decoded remote frame and paint it onto the canvas.
  useEffect(() => {
    if (!sharer) return;
    let cancelled = false;
    let timer: number | undefined;

    const tick = async () => {
      try {
        const buffer = await api.remoteFrame(sharer.id);
        if (cancelled || !buffer || buffer.byteLength === 0) return;
        const bitmap = await createImageBitmap(
          new Blob([buffer], { type: "image/jpeg" }),
        );
        if (cancelled) {
          bitmap.close();
          return;
        }
        const canvas = canvasRef.current;
        if (canvas) {
          canvas.width = bitmap.width;
          canvas.height = bitmap.height;
          canvas.getContext("2d")?.drawImage(bitmap, 0, 0);
        }
        bitmap.close();
      } catch {
        // A missing frame is normal; keep polling.
      }
    };

    const loop = () => {
      void tick().finally(() => {
        if (!cancelled) timer = window.setTimeout(loop, 100);
      });
    };
    void loop();

    return () => {
      cancelled = true;
      if (timer) window.clearTimeout(timer);
    };
  }, [sharer]);
  // Mirror the configured device/volume into the audio engine.
  useEffect(() => {
    api.setMicVolume(micVolume).catch(() => undefined);
  }, [micVolume]);

  const toggleMic = async () => {
    const next = !micOn;
    setMicOn(next);
    try {
      if (next) {
        await api.startMicrophone(config.audio.mic_device_id ?? undefined);
      } else {
        await api.stopMicrophone();
      }
    } catch (e) {
      setStatus(errorMessage(e));
      setMicOn(!next);
    }
  };

  const toggleSystem = async () => {
    const next = !systemOn;
    setSystemOn(next);
    try {
      if (next) {
        await api.startSystemAudio(undefined);
      } else {
        await api.stopSystemAudio();
      }
    } catch (e) {
      setStatus(errorMessage(e));
      setSystemOn(!next);
    }
  };

  const startShare = async (source: CaptureSource, options: CaptureOptions) => {
    try {
      await api.startScreenShare(source, options);
      setSharing(true);
      setShareOpen(false);
      onSaveConfig({
        ...config,
        video: {
          ...config.video,
          width: options.width,
          height: options.height,
          max_fps: options.fps,
          max_bitrate_kbps: options.bitrate_kbps,
          hardware_encoding: options.hardware_encoding,
        },
        capture: { ...config.capture, last_monitor_id: source.id },
      });
    } catch (e) {
      setStatus(errorMessage(e));
    }
  };

  const stopShare = async () => {
    try {
      await api.stopScreenShare();
      setSharing(false);
    } catch (e) {
      setStatus(errorMessage(e));
    }
  };

  return (
    <div className="call">
      <aside className="sidebar">
        <header className="sidebar-header">
          <p className="eyebrow">CURRENT ROOM</p>
          <h2>Room</h2>
          <div className="room-code-row">
            <span className="room-id" title={roomId}>
              {roomId}
            </span>
            <button
              className="icon-button"
              title={roomCopied ? "Room ID copied" : "Copy room ID"}
              aria-label={roomCopied ? "Room ID copied" : "Copy room ID"}
              onClick={copyRoomId}
            >
              {roomCopied ? "✓" : "⧉"}
            </button>
          </div>
        </header>
        <div className="sidebar-profile">
          <span className="avatar">{initials(config.state.display_name)}</span>
          <div className="profile-copy">
            <span className="small muted">You</span>
            <strong>{config.state.display_name}</strong>
          </div>
        </div>
        <div className="section-heading sidebar-section-heading">
          <h3>Participants</h3>
          <span className="small muted">{participants.length}</span>
        </div>
        <ul className="participants">
          {participants.map((p) => (
            <li key={p.id}>
              <span className="avatar">{initials(p.name)}</span>
              <span className="participant-copy grow" title={p.name}>
                <strong>{p.name}</strong>
                <span className="muted small">{p.connection}</span>
              </span>
              <input
                className="peer-volume"
                type="range"
                min={0}
                max={2}
                step={0.05}
                defaultValue={1}
                title={`Volume for ${p.name}`}
                aria-label={`Volume for ${p.name}`}
                onChange={(e) =>
                  api
                    .setPeerVolume(p.id, Number(e.target.value))
                    .catch(() => undefined)
                }
              />
              <MicStatus on={p.mic_on} />
            </li>
          ))}
          {participants.length === 0 && (
            <li className="muted">Waiting for others to join…</li>
          )}
        </ul>
        <div className="grow" />
        <button className="ghost sidebar-settings" onClick={onOpenSettings}>
          Settings
        </button>
      </aside>

      <main className="stage">
        {status && <div className="banner error">{status}</div>}
        <div className="stage-surface">
          {sharer ? (
            <figure className="video-container">
              <canvas ref={canvasRef} className="remote-canvas" />
              <button
                className="pip-overlay"
                title="Open in picture-in-picture window"
                onClick={() => api.openPipWindow().catch(() => undefined)}
              >
                PiP
              </button>
            </figure>
          ) : (
            <p className="muted">
              {sharing
                ? "You are sharing your screen."
                : "No one is sharing a screen."}
            </p>
          )}
        </div>

        <div className="controls">
          <button
            className={micOn ? "control active" : "control"}
            onClick={toggleMic}
            title="Toggle microphone"
          >
            {micOn ? "Mic on" : "Mic off"}
          </button>
          <label className="slider">
            <span className="small muted">Mic volume</span>
            <input
              type="range"
              min={0}
              max={2}
              step={0.05}
              value={micVolume}
              onChange={(e) => setMicVolume(Number(e.target.value))}
            />
          </label>
          <button
            className={systemOn ? "control active" : "control"}
            onClick={toggleSystem}
            title="Share system audio"
          >
            {systemOn ? "System on" : "System off"}
          </button>
          {sharing ? (
            <button className="control danger" onClick={stopShare}>
              Stop share
            </button>
          ) : (
            <button className="control primary" onClick={() => setShareOpen(true)}>
              Share screen
            </button>
          )}
          <div className="grow" />
          <button className="control destructive" onClick={onDeleteRoom}>
            Delete room
          </button>
          <button className="control danger" onClick={onLeave}>
            Leave
          </button>
        </div>
      </main>

      {shareOpen && (
        <ShareDialog
          config={config}
          onClose={() => setShareOpen(false)}
          onStart={startShare}
        />
      )}
    </div>
  );
}
