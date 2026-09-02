import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { NicView, Profile, SendView } from "./types";

/** Matches the rate the Rust side updates its status at. */
const POLL_MS = 250;

const ADDR_KEY = "lanrec.receiver";

export function Send({
  profile,
  link,
  monitor,
  blocked,
}: {
  profile: Profile;
  /** The adapter the stream will be forced onto, if one was found. */
  link: NicView | null;
  monitor: number | null;
  /** The chosen quality cannot work on this GPU. */
  blocked: boolean;
}) {
  const [to, setTo] = useState("10.0.0.2:9000");
  const [view, setView] = useState<SendView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);

  // The receiver address is the one thing worth remembering between launches.
  useEffect(() => {
    try {
      const saved = localStorage.getItem(ADDR_KEY);
      if (saved) setTo(saved);
    } catch {
      // Private windows and blocked site data both land here; a default is fine.
    }
  }, []);

  useEffect(() => {
    let alive = true;
    const poll = async () => {
      try {
        const v = await invoke<SendView>("send_view");
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
    setStarting(true);
    setError(null);
    try {
      try {
        localStorage.setItem(ADDR_KEY, to);
      } catch {
        // Not being able to remember it is not a reason to refuse to record.
      }
      await invoke("send_start", {
        req: {
          to,
          viaMac: link?.mac ?? null,
          monitor,
          profile,
          file: null,
        },
      });
    } catch (e) {
      setError(String(e));
    } finally {
      setStarting(false);
    }
  }, [to, link, monitor, profile]);

  const stop = useCallback(async () => {
    try {
      await invoke("send_stop");
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const clear = useCallback(async () => {
    try {
      await invoke("send_clear");
      setView(null);
    } catch (e) {
      setError(String(e));
    }
  }, []);

  const s = view?.status ?? null;
  const running = view?.running ?? false;
  const done = !!s?.finished;

  return (
    <section className={`send ${running ? "send-live" : ""}`}>
      <div className="send-head">
        <h2 className="section-title" style={{ margin: 0 }}>
          Aufnahme
        </h2>
        {running && (
          <span className="rec-badge">
            <span className="rec-dot" />
            läuft
          </span>
        )}
      </div>

      {!running && !done && (
        <>
          <div className="send-row">
            <div className="field" style={{ flex: 1, minWidth: 0 }}>
              <span className="field-label">Empfänger</span>
              <input
                className="text-input"
                value={to}
                onChange={(e) => setTo(e.target.value)}
                placeholder="10.0.0.2:9000"
                spellCheck={false}
              />
            </div>
            <button
              className="rec-btn"
              onClick={start}
              disabled={starting || blocked}
              title={blocked ? "Das gewählte Profil passt nicht" : undefined}
            >
              {starting ? "startet…" : "Aufnahme starten"}
            </button>
          </div>

          <p className="send-note">
            {link ? (
              <>
                Wird über <strong>{link.displayName}</strong> gesendet
                {link.ipv4[0] ? ` (${link.ipv4[0]})` : ""} — erzwungen, nicht der
                Routing-Tabelle überlassen.
              </>
            ) : (
              <>
                Keine Direktverbindung erkannt. Die Route wählt dann das System —
                bei zwei Karten kann das die falsche sein.
              </>
            )}
          </p>
          {blocked && (
            <p className="issue issue-block">
              Das gewählte Profil passt nicht. Einstellungen öffnen.
            </p>
          )}
        </>
      )}

      {(running || done) && s && (
        <>
          <div className="send-live-grid">
            <div className="send-big">
              <div className="big-num">{(s.bitrateBps / 1e6).toFixed(0)}</div>
              <div className="big-unit">Mbit/s</div>
            </div>
            <dl className="facts send-facts">
              <div>
                <dt>Frames</dt>
                <dd>{s.frames}</dd>
              </div>
              <div>
                <dt>Dauer</dt>
                <dd>{s.seconds.toFixed(0)} s</dd>
              </div>
              <div>
                <dt>Größe</dt>
                <dd>{(s.bytes / 1e6).toFixed(0)} MB</dd>
              </div>
              <div>
                <dt>Größter</dt>
                <dd>{(s.peakFrameBytes / 1e3).toFixed(0)} kB</dd>
              </div>
              <div className="facts-wide">
                <dt>Verloren</dt>
                <dd className={s.queueDropped > 0 ? "val-bad" : "val-live"}>
                  {s.queueDropped === 0
                    ? "keine"
                    : `${s.queueDropped} Frames — Encoder oder Leitung kommt nicht mit`}
                </dd>
              </div>
            </dl>
          </div>

          <p className="send-note">
            {s.destination}
            {s.via ? ` · über ${s.via}` : ""}
            {s.localAddr ? ` · ${s.localAddr}` : ""}
            {s.repeats > 0 ? ` · ${s.repeats} wiederholt` : ""}
          </p>

          {s.error && <p className="issue issue-block">{s.error}</p>}

          <div className="row-actions">
            {running ? (
              <button className="rec-btn rec-btn-stop" onClick={stop}>
                Aufnahme beenden
              </button>
            ) : (
              <button className="ghost-btn" onClick={clear}>
                Neue Aufnahme
              </button>
            )}
          </div>
        </>
      )}

      {error && <p className="issue issue-block">{error}</p>}
    </section>
  );
}
