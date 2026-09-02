import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { Status, View } from "./types";
import "./App.css";

/** Fast enough to look live; the Rust side updates four times a second. */
const POLL_MS = 250;

const DEFAULT_PORT = 9000;

export default function App() {
  const [view, setView] = useState<View | null>(null);
  const [listen, setListen] = useState(`0.0.0.0:${DEFAULT_PORT}`);
  const [outDir, setOutDir] = useState("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    invoke<string>("suggested_out_dir").then(setOutDir).catch(() => {});
  }, []);

  useEffect(() => {
    let alive = true;
    const poll = async () => {
      try {
        const v = await invoke<View>("view");
        if (alive) setView(v);
      } catch (e) {
        if (alive) setError(String(e));
      }
    };
    poll();
    const id = window.setInterval(poll, POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(id);
    };
  }, []);

  const start = useCallback(async () => {
    try {
      await invoke("start", { req: { listen, outDir } });
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, [listen, outDir]);

  const stop = useCallback(async () => {
    try {
      await invoke("stop");
      setError(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const running = view?.running ?? false;
  const s = view?.status ?? null;
  const fatal = view?.fatal ?? null;

  return (
    <div className="app app-narrow">
      <header className="masthead">
        <div className="brand">
          <span className="brand-mark" aria-hidden />
          <div>
            <h1>lanrec</h1>
            <p className="tagline">Empfänger</p>
          </div>
        </div>
        <StatePill running={running} status={s} fatal={fatal} />
      </header>

      {error && <div className="alert">{error}</div>}
      {fatal && <div className="alert">{fatal}</div>}

      {!running ? (
        <section className="quality">
          <div className="controls" style={{ borderBottom: "none", paddingBottom: 0 }}>
            <div className="field">
              <span className="field-label">Adresse</span>
              <input
                className="text-input"
                value={listen}
                onChange={(e) => setListen(e.target.value)}
                placeholder={`0.0.0.0:${DEFAULT_PORT}`}
                spellCheck={false}
              />
            </div>
            <div className="field" style={{ gridColumn: "1 / -1" }}>
              <span className="field-label">Aufnahmen landen in</span>
              <input
                className="text-input"
                value={outDir}
                onChange={(e) => setOutDir(e.target.value)}
                spellCheck={false}
              />
            </div>
          </div>

          <p className="hint">
            <code>0.0.0.0</code> nimmt von jeder Karte an. Trage die Adresse der
            Direktverbindung ein, wenn nur dieses Kabel zählen soll.
          </p>

          <button className="primary-btn" onClick={start}>
            Empfang starten
          </button>
        </section>
      ) : (
        <Live status={s} outDir={view?.outDir ?? ""} onStop={stop} />
      )}
    </div>
  );
}

function StatePill({
  running,
  status,
  fatal,
}: {
  running: boolean;
  status: Status | null;
  fatal: string | null;
}) {
  if (fatal) {
    return (
      <div className="pill pill-idle" style={{ color: "var(--bad)" }}>
        <span className="dot" />
        gestoppt
      </div>
    );
  }
  if (!running) {
    return (
      <div className="pill pill-idle">
        <span className="dot" />
        aus
      </div>
    );
  }
  if (status?.peer && !status.finished) {
    return (
      <div className="pill pill-live">
        <span className="dot dot-live" />
        nimmt auf
      </div>
    );
  }
  return (
    <div className="pill pill-wait">
      <span className="dot dot-wait" />
      wartet auf Sender
    </div>
  );
}

function Live({
  status,
  outDir,
  onStop,
}: {
  status: Status | null;
  outDir: string;
  onStop: () => void;
}) {
  const receiving = !!status?.peer && !status.finished;
  const done = !!status?.finished;

  return (
    <>
      <section className={`screen-card ${receiving ? "screen-card-live" : ""}`}>
        {receiving && status ? (
          <>
            <div className="big-num">{(status.bitrateBps / 1e6).toFixed(0)}</div>
            <div className="big-unit">Mbit/s</div>
            <div className="spec">
              {(status.codec ?? "").toUpperCase()} · {status.width}×{status.height} ·{" "}
              {status.chroma} · {status.bitDepth}-bit · {status.fps.toFixed(0)} fps
            </div>
          </>
        ) : done && status ? (
          <>
            <div className="big-num">{(status.bytes / 1e6).toFixed(0)}</div>
            <div className="big-unit">MB aufgenommen</div>
            <div className="spec">
              {status.frames} Frames in {status.seconds.toFixed(1)}s
            </div>
          </>
        ) : (
          <>
            <div className="waiting">
              <span className="dot dot-wait" />
              wartet auf {status?.listeningOn ?? "…"}
            </div>
            <div className="spec">Starte den Sender auf dem Gaming-PC.</div>
          </>
        )}
      </section>

      {status && (receiving || done) && (
        <dl className="facts stat-grid">
          <div>
            <dt>Frames</dt>
            <dd>{status.frames}</dd>
          </div>
          <div>
            <dt>Keyframes</dt>
            <dd>{status.keyframes}</dd>
          </div>
          <div>
            <dt>Größe</dt>
            <dd>{(status.bytes / 1e6).toFixed(1)} MB</dd>
          </div>
          <div>
            <dt>Dauer</dt>
            <dd>{status.seconds.toFixed(1)} s</dd>
          </div>
          {status.source && (
            <div className="facts-wide">
              <dt>Quelle</dt>
              <dd>
                {status.source}
                {status.peer ? ` · ${status.peer}` : ""}
              </dd>
            </div>
          )}
          {status.path && (
            <div className="facts-wide">
              <dt>Datei</dt>
              <dd className="path" title={status.path}>
                {status.path}
              </dd>
            </div>
          )}
        </dl>
      )}

      {status?.error && <p className="issue issue-block">{status.error}</p>}
      {done && status && !status.cleanEnd && !status.error && (
        <p className="issue">
          Der Sender hat sich nicht abgemeldet — die Aufnahme endet dort, wo die
          Verbindung abriss.
        </p>
      )}

      <div className="row-actions">
        {status?.path && done && (
          <button
            className="ghost-btn"
            onClick={() => invoke("reveal", { path: status.path }).catch(() => {})}
          >
            Ordner öffnen
          </button>
        )}
        <button className="ghost-btn" onClick={onStop}>
          Empfang beenden
        </button>
      </div>

      <p className="preview-note">{outDir}</p>
    </>
  );
}
