import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { MonitorView, PreviewFrame } from "./types";

/** Matches the rate the Rust side produces frames at. */
const POLL_MS = 100;

export function Preview() {
  const [monitors, setMonitors] = useState<MonitorView[] | null>(null);
  const [selected, setSelected] = useState<number | null>(null);
  const [running, setRunning] = useState(false);
  const [src, setSrc] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  // Frame counter, so a stalled preview is visible rather than looking frozen
  // on a static desktop.
  const seen = useRef(0);
  const [stale, setStale] = useState(false);

  useEffect(() => {
    invoke<MonitorView[]>("list_monitors")
      .then((m) => {
        setMonitors(m);
        const primary = m.find((x) => x.primary) ?? m[0];
        if (primary) setSelected(primary.index);
      })
      .catch((e) => setError(String(e)));
  }, []);

  const start = useCallback(async (monitor: number | null) => {
    try {
      await invoke("preview_start", { monitor });
      setError(null);
      setRunning(true);
    } catch (e) {
      setError(String(e));
      setRunning(false);
    }
  }, []);

  const stop = useCallback(async () => {
    try {
      await invoke("preview_stop");
    } catch {
      // Stopping a preview that is already gone is not worth reporting.
    }
    setRunning(false);
    setSrc(null);
  }, []);

  // Start as soon as a display is known: a preview you have to switch on is not
  // much of a preview.
  useEffect(() => {
    if (selected !== null) start(selected);
    return () => {
      void invoke("preview_stop").catch(() => {});
    };
  }, [selected, start]);

  useEffect(() => {
    if (!running) return;
    let alive = true;
    let lastCount = -1;
    let quiet = 0;

    const id = window.setInterval(async () => {
      try {
        const f = await invoke<PreviewFrame | null>("preview_frame");
        if (!alive) return;
        if (f) {
          if (f.dataUrl !== src) seen.current += 1;
          setSrc(f.dataUrl);
        }
        // Nothing new for two seconds means the screen simply is not changing.
        quiet = seen.current === lastCount ? quiet + 1 : 0;
        lastCount = seen.current;
        setStale(quiet > 20);
        setError(null);
      } catch (e) {
        if (alive) {
          setError(String(e));
          setRunning(false);
        }
      }
    }, POLL_MS);

    return () => {
      alive = false;
      window.clearInterval(id);
    };
  }, [running, src]);

  const current = monitors?.find((m) => m.index === selected);

  return (
    <section className="preview">
      <div className="preview-head">
        <h2 className="section-title" style={{ margin: 0 }}>
          Vorschau
        </h2>
        <div className="preview-actions">
          {monitors && monitors.length > 1 && (
            <div className="seg">
              {monitors.map((m) => (
                <button
                  key={m.index}
                  className={`seg-btn ${selected === m.index ? "seg-on" : ""}`}
                  onClick={() => setSelected(m.index)}
                  title={m.device}
                >
                  {m.height}p{m.primary ? " ·" : ""}
                </button>
              ))}
            </div>
          )}
          <button
            className="ghost-btn"
            onClick={() => (running ? stop() : start(selected))}
          >
            {running ? "Anhalten" : "Starten"}
          </button>
        </div>
      </div>

      <div className={`screen ${running ? "" : "screen-off"}`}>
        {src ? (
          <img src={src} alt="Live-Vorschau des aufgenommenen Bildschirms" />
        ) : (
          <div className="screen-empty">
            {error ? "Vorschau nicht verfügbar" : running ? "warte auf Bild…" : "angehalten"}
          </div>
        )}

        {running && (
          <div className={`live ${stale ? "live-idle" : ""}`}>
            <span className={`dot ${stale ? "dot-wait" : "dot-live"}`} />
            {stale ? "keine Änderung" : "live"}
          </div>
        )}
      </div>

      {error && <p className="issue issue-block">{error}</p>}
      {current && !error && (
        <p className="preview-note">
          {current.device} · {current.width}×{current.height}
          {stale && " · Bildschirm steht still, deshalb kommen keine neuen Frames"}
        </p>
      )}
    </section>
  );
}
