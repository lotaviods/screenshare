import { useEffect, useState } from "react";

import { api } from "../lib/bridge";
import type { Config, Device } from "../lib/types";

interface SettingsProps {
  config: Config;
  onBack: () => void;
  onSave: (config: Config) => void;
}

export default function Settings({ config, onBack, onSave }: SettingsProps) {
  const [draft, setDraft] = useState<Config>(config);
  const [microphones, setMicrophones] = useState<Device[]>([]);
  const [outputs, setOutputs] = useState<Device[]>([]);

  useEffect(() => {
    api.listMicrophones().then(setMicrophones).catch(() => undefined);
    api.listOutputs().then(setOutputs).catch(() => undefined);
  }, []);

  const patchNetwork = (patch: Partial<Config["network"]>) =>
    setDraft({ ...draft, network: { ...draft.network, ...patch } });
  const patchAudio = (patch: Partial<Config["audio"]>) =>
    setDraft({ ...draft, audio: { ...draft.audio, ...patch } });
  const patchVideo = (patch: Partial<Config["video"]>) =>
    setDraft({ ...draft, video: { ...draft.video, ...patch } });
  const patchCapture = (patch: Partial<Config["capture"]>) =>
    setDraft({ ...draft, capture: { ...draft.capture, ...patch } });

  const stunText = draft.network.stun_servers.map((s) => s.url).join("\n");
  const turnText = draft.network.turn_servers
    .map((t) => (t.username ? `${t.url}|${t.username}|${t.credential ?? ""}` : t.url))
    .join("\n");

  return (
    <div className="settings">
      <header className="home-header">
        <h1>Settings</h1>
        <div className="row">
          <button className="ghost" onClick={onBack}>
            Back
          </button>
          <button className="primary" onClick={() => onSave(draft)}>
            Save
          </button>
        </div>
      </header>

      <section className="card">
        <h2>Identity</h2>
        <label>
          <span>Display name</span>
          <input
            value={draft.state.display_name}
            onChange={(e) =>
              setDraft({
                ...draft,
                state: { ...draft.state, display_name: e.target.value },
              })
            }
          />
        </label>
      </section>

      <section className="card">
        <h2>Network</h2>
        <div className="row">
          <label className="grow">
            <span>Signaling host</span>
            <input
              value={draft.network.signaling_host}
              onChange={(e) => patchNetwork({ signaling_host: e.target.value })}
              spellCheck={false}
            />
          </label>
          <label>
            <span>Port</span>
            <input
              type="number"
              value={draft.network.signaling_port}
              onChange={(e) =>
                patchNetwork({ signaling_port: Number(e.target.value) })
              }
            />
          </label>
        </div>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={draft.network.signaling_secure}
            onChange={(e) => patchNetwork({ signaling_secure: e.target.checked })}
          />
          <span>Use secure WebSocket (wss://)</span>
        </label>
        <label>
          <span>STUN servers (one per line)</span>
          <textarea
            value={stunText}
            rows={3}
            spellCheck={false}
            onChange={(e) =>
              patchNetwork({
                stun_servers: e.target.value
                  .split("\n")
                  .map((s) => s.trim())
                  .filter(Boolean)
                  .map((url) => ({ url })),
              })
            }
          />
        </label>
        <label>
          <span>TURN servers (url|user|credential, one per line)</span>
          <textarea
            value={turnText}
            rows={3}
            spellCheck={false}
            onChange={(e) =>
              patchNetwork({
                turn_servers: e.target.value
                  .split("\n")
                  .map((s) => s.trim())
                  .filter(Boolean)
                  .map((line) => {
                    const [url, username, credential] = line.split("|");
                    return {
                      url,
                      username: username || null,
                      credential: credential || null,
                    };
                  }),
              })
            }
          />
        </label>
        <label>
          <span>Connection mode</span>
          <select
            value={draft.network.connection_mode}
            onChange={(e) =>
              patchNetwork({
                connection_mode: e.target.value as Config["network"]["connection_mode"],
              })
            }
          >
            <option value="mesh">Mesh (peer to peer)</option>
            <option value="sfu">SFU</option>
          </select>
        </label>
      </section>

      <section className="card">
        <h2>Audio</h2>
        <label>
          <span>Microphone</span>
          <select
            value={draft.audio.mic_device_id ?? ""}
            onChange={(e) =>
              patchAudio({ mic_device_id: e.target.value || null })
            }
          >
            <option value="">System default</option>
            {microphones.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name}
              </option>
            ))}
          </select>
        </label>
        <label>
          <span>Audio output</span>
          <select
            value={draft.audio.output_device_id ?? ""}
            onChange={(e) =>
              patchAudio({ output_device_id: e.target.value || null })
            }
          >
            <option value="">System default</option>
            {outputs.map((d) => (
              <option key={d.id} value={d.id}>
                {d.name}
              </option>
            ))}
          </select>
        </label>
        <div className="row">
          <label className="grow">
            <span>Mic volume</span>
            <input
              type="range"
              min={0}
              max={2}
              step={0.05}
              value={draft.audio.mic_volume}
              onChange={(e) => patchAudio({ mic_volume: Number(e.target.value) })}
            />
          </label>
          <label className="grow">
            <span>System audio volume</span>
            <input
              type="range"
              min={0}
              max={2}
              step={0.05}
              value={draft.audio.system_volume}
              onChange={(e) =>
                patchAudio({ system_volume: Number(e.target.value) })
              }
            />
          </label>
        </div>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={draft.audio.echo_cancellation}
            onChange={(e) =>
              patchAudio({ echo_cancellation: e.target.checked })
            }
          />
          <span>Echo cancellation</span>
        </label>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={draft.audio.noise_suppression}
            onChange={(e) =>
              patchAudio({ noise_suppression: e.target.checked })
            }
          />
          <span>Noise suppression</span>
        </label>
      </section>

      <section className="card">
        <h2>Video</h2>
        <div className="row">
          <label>
            <span>Width</span>
            <input
              type="number"
              value={draft.video.width}
              onChange={(e) => patchVideo({ width: Number(e.target.value) })}
            />
          </label>
          <label>
            <span>Height</span>
            <input
              type="number"
              value={draft.video.height}
              onChange={(e) => patchVideo({ height: Number(e.target.value) })}
            />
          </label>
          <label>
            <span>Max FPS</span>
            <input
              type="number"
              value={draft.video.max_fps}
              onChange={(e) => patchVideo({ max_fps: Number(e.target.value) })}
            />
          </label>
        </div>
        <div className="row">
          <label>
            <span>Codec</span>
            <select
              value={draft.video.codec}
              onChange={(e) =>
                patchVideo({ codec: e.target.value as Config["video"]["codec"] })
              }
            >
              <option value="vp8">VP8</option>
              <option value="vp9">VP9</option>
              <option value="h264">H.264</option>
            </select>
          </label>
          <label>
            <span>Max bitrate (kbps)</span>
            <input
              type="number"
              value={draft.video.max_bitrate_kbps}
              onChange={(e) =>
                patchVideo({ max_bitrate_kbps: Number(e.target.value) })
              }
            />
          </label>
        </div>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={draft.video.hardware_encoding}
            onChange={(e) =>
              patchVideo({ hardware_encoding: e.target.checked })
            }
          />
          <span>Prefer hardware encoding</span>
        </label>
      </section>

      <section className="card">
        <h2>Capture</h2>
        <label>
          <span>Backend</span>
          <select
            value={draft.capture.backend}
            onChange={(e) =>
              patchCapture({
                backend: e.target.value as Config["capture"]["backend"],
              })
            }
          >
            <option value="auto">Automatic</option>
            <option value="portal">XDG portal (PipeWire)</option>
            <option value="x11">X11 (ximagesrc)</option>
            <option value="windows">Windows (WGC/DXGI)</option>
          </select>
        </label>
        <label className="checkbox">
          <input
            type="checkbox"
            checked={draft.capture.system_audio}
            onChange={(e) => patchCapture({ system_audio: e.target.checked })}
          />
          <span>Share system audio with the screen by default</span>
        </label>
      </section>
    </div>
  );
}
