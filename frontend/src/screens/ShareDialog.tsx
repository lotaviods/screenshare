import { useEffect, useState } from "react";

import { api, errorMessage } from "../lib/bridge";
import type {
  CaptureOptions,
  CaptureSource,
  Config,
  Device,
} from "../lib/types";

interface ShareDialogProps {
  config: Config;
  onClose: () => void;
  onStart: (source: CaptureSource, options: CaptureOptions) => void;
}

const RESOLUTIONS: [number, number][] = [
  [1920, 1080],
  [1280, 720],
  [2560, 1440],
  [3840, 2160],
];

export default function ShareDialog({
  config,
  onClose,
  onStart,
}: ShareDialogProps) {
  const [sources, setSources] = useState<CaptureSource[]>([]);
  const [selected, setSelected] = useState<string>("");
  const [width, setWidth] = useState(config.video.width);
  const [height, setHeight] = useState(config.video.height);
  const [fps, setFps] = useState(config.video.max_fps);
  const [bitrate, setBitrate] = useState(config.video.max_bitrate_kbps);
  const [hardware, setHardware] = useState(config.video.hardware_encoding);
  const [includeSystemAudio, setIncludeSystemAudio] = useState(
    config.capture.system_audio,
  );
  const [systemMonitors, setSystemMonitors] = useState<Device[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .listCaptureSources()
      .then((list) => {
        setSources(list);
        setSelected((prev) => prev || list[0]?.id || "");
      })
      .catch((e) => setError(errorMessage(e)));

    api
      .listSystemMonitors()
      .then(setSystemMonitors)
      .catch(() => undefined);
  }, []);

  const source = sources.find((s) => s.id === selected);

  const start = () => {
    if (!source) return;
    onStart(source, {
      width,
      height,
      fps,
      cursor: "embedded",
      codec: config.video.codec,
      bitrate_kbps: bitrate,
      hardware_encoding: hardware,
    });
  };

  const toggleSystemAudio = async (on: boolean) => {
    setIncludeSystemAudio(on);
    try {
      if (on) {
        const device = systemMonitors[0];
        await api.startSystemAudio(device?.id);
      } else {
        await api.stopSystemAudio();
      }
    } catch (e) {
      setError(errorMessage(e));
      setIncludeSystemAudio(!on);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <h2>Share your screen</h2>
        {error && <div className="banner error">{error}</div>}

        <label>
          <span>Source</span>
          <select
            value={selected}
            onChange={(e) => setSelected(e.target.value)}
          >
            {sources.map((s) => (
              <option key={s.id} value={s.id}>
                {s.name}
              </option>
            ))}
            {sources.length === 0 && <option value="">No sources available</option>}
          </select>
        </label>

        <div className="row">
          <label>
            <span>Resolution</span>
            <select
              value={`${width}x${height}`}
              onChange={(e) => {
                const [w, h] = e.target.value.split("x").map(Number);
                setWidth(w);
                setHeight(h);
              }}
            >
              {RESOLUTIONS.map(([w, h]) => (
                <option key={`${w}x${h}`} value={`${w}x${h}`}>
                  {w}×{h}
                </option>
              ))}
            </select>
          </label>
          <label>
            <span>Frame rate</span>
            <select value={fps} onChange={(e) => setFps(Number(e.target.value))}>
              {[15, 24, 30, 60, 90, 120].map((f) => (
                <option key={f} value={f}>
                  {f} fps
                </option>
              ))}
            </select>
          </label>
        </div>

        <div className="row">
          <label>
            <span>Codec</span>
            <input value={config.video.codec.toUpperCase()} readOnly spellCheck={false} />
          </label>
          <label>
            <span>Bitrate (kbps)</span>
            <input
              type="number"
              value={bitrate}
              onChange={(e) => setBitrate(Number(e.target.value))}
            />
          </label>
        </div>

        <label className="checkbox">
          <input
            type="checkbox"
            checked={hardware}
            onChange={(e) => setHardware(e.target.checked)}
          />
          <span>Prefer hardware encoding</span>
        </label>

        <label className="checkbox">
          <input
            type="checkbox"
            checked={includeSystemAudio}
            onChange={(e) => void toggleSystemAudio(e.target.checked)}
          />
          <span>Share system audio</span>
        </label>

        <div className="row">
          <div className="grow" />
          <button className="ghost" onClick={onClose}>
            Cancel
          </button>
          <button className="primary" disabled={!source} onClick={start}>
            Start sharing
          </button>
        </div>
      </div>
    </div>
  );
}
