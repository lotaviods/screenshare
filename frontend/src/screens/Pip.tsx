import { useEffect, useRef } from "react";

import { api } from "../lib/bridge";

/**
 * Always-on-top mirror of the current video: the remote share when someone is
 * sharing, otherwise the local self-view. Runs in its own Tauri window.
 */
export default function Pip() {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  useEffect(() => {
    let cancelled = false;
    let timer: number | undefined;

    const tick = async () => {
      try {
        const buffer = await api.pipFrame();
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
        // No frame yet; keep polling.
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
  }, []);

  return (
    <div className="pip-window" data-tauri-drag-region>
      <canvas ref={canvasRef} className="pip-canvas" />
      <button
        className="pip-close"
        title="Close picture in picture"
        onClick={() => api.closePipWindow().catch(() => undefined)}
      >
        ×
      </button>
    </div>
  );
}
