import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Quality, DEFAULT_PROFILE } from "./Quality";
import { Drawer } from "./Drawer";
import { Preview } from "./Preview";
import { Send } from "./Send";
import type { Evaluation, GpuCaps, NicView, Profile } from "./types";
import "./App.css";

/** Link state is polled rather than pushed -- see the comment on the Rust command. */
const POLL_MS = 1500;

/** How long a card stays highlighted after its link state changes. */
const FLASH_MS = 2200;

/** Slider drags fire fast; the estimate only needs to keep up with the eye. */
const EVAL_DEBOUNCE_MS = 120;

const CODEC_LABEL: Record<string, string> = { hevc: "HEVC", av1: "AV1", h264: "H.264" };
const CHROMA_LABEL: Record<string, string> = { yuv444: "4:4:4", yuv420: "4:2:0" };
const DEPTH_LABEL: Record<string, string> = { ten: "10-bit", eight: "8-bit" };

export default function App() {
  const [nics, setNics] = useState<NicView[] | null>(null);
  const [caps, setCaps] = useState<GpuCaps | null>(null);
  const [capsError, setCapsError] = useState<string | null>(null);
  const [profile, setProfile] = useState<Profile>(DEFAULT_PROFILE);
  const [evaluation, setEvaluation] = useState<Evaluation | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [flashing, setFlashing] = useState<Record<number, "up" | "down">>({});
  const [settingsOpen, setSettingsOpen] = useState(false);

  // Previous link state per adapter, so a change can be announced instead of the
  // list silently re-rendering.
  const prevUp = useRef<Map<number, boolean>>(new Map());

  useEffect(() => {
    invoke<GpuCaps>("gpu_caps").then(setCaps).catch((e) => setCapsError(String(e)));
  }, []);

  const reload = useCallback(async () => {
    try {
      setNics(await invoke<NicView[]>("list_nics"));
    } catch (e) {
      setError(String(e));
    }
  }, []);

  useEffect(() => {
    let alive = true;
    const timers: number[] = [];

    const poll = async () => {
      try {
        const list = await invoke<NicView[]>("list_nics");
        if (!alive) return;

        for (const n of list) {
          const before = prevUp.current.get(n.index);
          if (before !== undefined && before !== n.up) {
            const kind = n.up ? "up" : "down";
            setFlashing((f) => ({ ...f, [n.index]: kind }));
            timers.push(
              window.setTimeout(() => {
                setFlashing((f) => {
                  const { [n.index]: _drop, ...rest } = f;
                  return rest;
                });
              }, FLASH_MS),
            );
          }
          prevUp.current.set(n.index, n.up);
        }

        setNics(list);
        setError(null);
      } catch (e) {
        if (alive) setError(String(e));
      }
    };

    poll();
    const id = window.setInterval(poll, POLL_MS);
    return () => {
      alive = false;
      window.clearInterval(id);
      timers.forEach(window.clearTimeout);
    };
  }, []);

  // The port we would actually record over: wired, live, no gateway (so it is a
  // direct cable rather than the route to the router).
  const target =
    nics?.find((n) => n.up && n.directLinkCandidate && n.medium === "ethernet") ?? null;

  // A wired port with no cable in it is a candidate for *becoming* the link.
  const spare =
    !target && nics
      ? (nics.find((n) => !n.up && n.medium === "ethernet" && !n.hasGateway) ?? null)
      : null;

  useEffect(() => {
    if (!caps) return;
    const id = window.setTimeout(() => {
      invoke<Evaluation>("evaluate_profile", {
        profile,
        linkBps: target ? target.linkSpeedBps : null,
      })
        .then(setEvaluation)
        .catch((e) => setError(String(e)));
    }, EVAL_DEBOUNCE_MS);
    return () => window.clearTimeout(id);
  }, [profile, target?.linkSpeedBps, caps]);

  const rename = useCallback(
    async (mac: string, label: string) => {
      try {
        await invoke("rename_nic", { mac, label });
        await reload();
      } catch (e) {
        setError(String(e));
      }
    },
    [reload],
  );

  return (
    <div className="app">
      <header className="masthead">
        <div className="brand">
          <span className="brand-mark" aria-hidden />
          <div>
            <h1>lanrec</h1>
            <p className="tagline">Gaming-PC über Ethernet aufnehmen</p>
          </div>
        </div>
        <div className="masthead-right">
          <LinkPill target={target} spare={spare} loading={nics === null} />
          <button
            className="icon-btn"
            onClick={() => setSettingsOpen(true)}
            aria-label="Einstellungen"
            title="Einstellungen"
          >
            <GearIcon />
          </button>
        </div>
      </header>

      {error && <div className="alert">{error}</div>}
      {capsError && <div className="alert">Encoder nicht verfügbar: {capsError}</div>}

      <Preview />

      <Send
        profile={profile}
        link={target}
        monitor={null}
        blocked={evaluation?.issues.some((i) => i.blocking) ?? false}
      />

      <SummaryBar
        profile={profile}
        evaluation={evaluation}
        link={target}
        onOpen={() => setSettingsOpen(true)}
      />

      <section className="adapters">
        <h2 className="section-title">
          Netzwerkadapter
          <span className="count">{nics ? nics.length : ""}</span>
        </h2>

        {nics === null && !error && <div className="skeleton" />}

        <div className="grid">
          {nics?.map((n) => (
            <AdapterCard
              key={n.index}
              nic={n}
              flash={flashing[n.index]}
              isTarget={target?.index === n.index}
              onRename={rename}
            />
          ))}
        </div>
      </section>

      <Drawer open={settingsOpen} title="Einstellungen" onClose={() => setSettingsOpen(false)}>
        <Quality
          caps={caps}
          profile={profile}
          onChange={setProfile}
          evaluation={evaluation}
          link={target}
        />
      </Drawer>
    </div>
  );
}

/** One line on the main view: what would be sent, and what it costs. */
function SummaryBar({
  profile,
  evaluation,
  link,
  onOpen,
}: {
  profile: Profile;
  evaluation: Evaluation | null;
  link: NicView | null;
  onOpen: () => void;
}) {
  const usable = link ? link.linkSpeedBps * 0.85 : null;
  const bps = evaluation?.estimatedBps ?? 0;
  const pct = usable ? Math.min(100, (bps / usable) * 100) : null;
  const blocked = evaluation?.issues.some((i) => i.blocking) ?? false;

  const qp = profile.rateControl.mode === "cqp" ? `QP ${profile.rateControl.qp}` : "Bitrate";

  return (
    <button className={`summary ${blocked ? "summary-bad" : ""}`} onClick={onOpen}>
      <div className="summary-main">
        <div className="summary-spec">
          <strong>{CODEC_LABEL[profile.codec] ?? profile.codec}</strong>
          <span className="sep">·</span>
          {CHROMA_LABEL[profile.chroma]}
          <span className="sep">·</span>
          {DEPTH_LABEL[profile.depth]}
          <span className="sep">·</span>
          {profile.height}p{profile.fpsNum}
          <span className="sep">·</span>
          {qp}
        </div>
        <div className="summary-rate">
          {evaluation ? `${(bps / 1e6).toFixed(0)} Mbit/s` : "…"}
          {usable && <span className="dim"> / {(usable / 1e6).toFixed(0)}</span>}
        </div>
      </div>
      {pct !== null && (
        <div className="meter meter-slim">
          <div className="meter-fill" style={{ width: `${pct}%` }} />
        </div>
      )}
      {blocked && <p className="summary-warn">Profil passt nicht — Einstellungen öffnen</p>}
    </button>
  );
}

function LinkPill({
  target,
  spare,
  loading,
}: {
  target: NicView | null;
  spare: NicView | null;
  loading: boolean;
}) {
  if (loading) return <div className="pill pill-idle">suche…</div>;

  if (target) {
    return (
      <div className="pill pill-live">
        <span className="dot dot-live" />
        {target.displayName} · {target.linkSpeedLabel}
      </div>
    );
  }
  if (spare) {
    return (
      <div className="pill pill-wait">
        <span className="dot dot-wait" />
        {spare.displayName} frei · Kabel einstecken
      </div>
    );
  }
  return (
    <div className="pill pill-idle">
      <span className="dot" />
      keine Direktverbindung
    </div>
  );
}

function AdapterCard({
  nic,
  flash,
  isTarget,
  onRename,
}: {
  nic: NicView;
  flash?: "up" | "down";
  isTarget: boolean;
  onRename: (mac: string, label: string) => void;
}) {
  const cls = [
    "card",
    `card-${nic.suitability}`,
    isTarget ? "card-target" : "",
    flash ? `flash-${flash}` : "",
  ]
    .filter(Boolean)
    .join(" ");

  return (
    <article className={cls}>
      {isTarget && <div className="ribbon">Aufnahmestrecke</div>}

      <div className="card-head">
        <span className={`dot ${nic.up ? "dot-live" : "dot-down"}`} />
        <NameEditor nic={nic} onRename={onRename} />
        <span className="medium">{mediumLabel(nic.medium)}</span>
      </div>

      <p className="hw">
        {nic.label ? `${nic.name} · ` : ""}
        {nic.description}
      </p>

      <dl className="facts">
        <div>
          <dt>Link</dt>
          <dd className={nic.up ? "val-live" : "val-dim"}>
            {nic.up ? nic.linkSpeedLabel : "kein Kabel"}
          </dd>
        </div>
        <div>
          <dt>MTU</dt>
          <dd className={nic.jumbo ? "val-live" : ""}>{nic.mtu}</dd>
        </div>
        <div className="facts-wide">
          <dt>IPv4</dt>
          <dd>{nic.ipv4.length ? nic.ipv4.join(", ") : "—"}</dd>
        </div>
      </dl>

      {nic.note && <p className="note">{nic.note}</p>}

      {flash === "up" && <div className="toast">Verbindung erkannt</div>}
      {flash === "down" && <div className="toast toast-down">Verbindung getrennt</div>}
    </article>
  );
}

/** Click the adapter name to give it one of your own. */
function NameEditor({
  nic,
  onRename,
}: {
  nic: NicView;
  onRename: (mac: string, label: string) => void;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState("");
  const input = useRef<HTMLInputElement>(null);

  useEffect(() => {
    if (editing) input.current?.select();
  }, [editing]);

  // Without a MAC there is nothing stable to hang a name on, so renaming is not
  // offered rather than silently forgotten on the next reboot.
  if (!nic.mac) return <h3>{nic.displayName}</h3>;

  const start = () => {
    setDraft(nic.label ?? "");
    setEditing(true);
  };

  const commit = () => {
    setEditing(false);
    if (draft.trim() !== (nic.label ?? "")) onRename(nic.mac!, draft);
  };

  if (editing) {
    return (
      <input
        ref={input}
        className="name-input"
        value={draft}
        placeholder={nic.name}
        maxLength={40}
        onChange={(e) => setDraft(e.target.value)}
        onBlur={commit}
        onKeyDown={(e) => {
          if (e.key === "Enter") commit();
          if (e.key === "Escape") setEditing(false);
        }}
      />
    );
  }

  return (
    <h3 className="name" onClick={start} title="Zum Umbenennen klicken">
      {nic.displayName}
      <PencilIcon />
    </h3>
  );
}

function mediumLabel(m: NicView["medium"]) {
  switch (m) {
    case "ethernet":
      return "Ethernet";
    case "wifi":
      return "WLAN";
    case "loopback":
      return "Loopback";
    default:
      return "sonstige";
  }
}

function GearIcon() {
  return (
    <svg width="17" height="17" viewBox="0 0 24 24" fill="none" aria-hidden>
      <circle cx="12" cy="12" r="3" stroke="currentColor" strokeWidth="1.7" />
      <path
        d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 1 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 1 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.6a1.65 1.65 0 0 0 1-1.51V3a2 2 0 1 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 1 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"
        stroke="currentColor"
        strokeWidth="1.5"
      />
    </svg>
  );
}

function PencilIcon() {
  return (
    <svg className="pencil" width="12" height="12" viewBox="0 0 24 24" fill="none" aria-hidden>
      <path
        d="M12 20h9M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4z"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  );
}
