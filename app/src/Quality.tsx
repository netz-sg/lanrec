import type {
  BitDepth,
  Chroma,
  Codec,
  Evaluation,
  GpuCaps,
  NicView,
  Profile,
} from "./types";

/** Named starting points. Anything can still be adjusted afterwards. */
const PRESETS: { key: string; label: string; hint: string; make: () => Profile }[] = [
  {
    key: "max",
    label: "Maximum",
    hint: "4:4:4 · 10-bit · QP 14",
    make: () => ({
      codec: "hevc",
      chroma: "yuv444",
      depth: "ten",
      width: 2560,
      height: 1440,
      fpsNum: 60,
      fpsDen: 1,
      rateControl: { mode: "cqp", qp: 14 },
      gopSeconds: 2,
    }),
  },
  {
    key: "balanced",
    label: "Ausgewogen",
    hint: "4:4:4 · 10-bit · QP 19",
    make: () => ({
      codec: "hevc",
      chroma: "yuv444",
      depth: "ten",
      width: 2560,
      height: 1440,
      fpsNum: 60,
      fpsDen: 1,
      rateControl: { mode: "cqp", qp: 19 },
      gopSeconds: 2,
    }),
  },
  {
    key: "lean",
    label: "Sparsam",
    hint: "AV1 · 4:2:0 · QP 24",
    make: () => ({
      codec: "av1",
      chroma: "yuv420",
      depth: "ten",
      width: 2560,
      height: 1440,
      fpsNum: 60,
      fpsDen: 1,
      rateControl: { mode: "cqp", qp: 24 },
      gopSeconds: 2,
    }),
  },
];

const RESOLUTIONS: { label: string; w: number; h: number }[] = [
  { label: "1080p", w: 1920, h: 1080 },
  { label: "1440p", w: 2560, h: 1440 },
  { label: "2160p", w: 3840, h: 2160 },
];

const RATES = [30, 60, 120];

export function Quality({
  caps,
  profile,
  onChange,
  evaluation,
  link,
}: {
  caps: GpuCaps | null;
  profile: Profile;
  onChange: (p: Profile) => void;
  evaluation: Evaluation | null;
  link: NicView | null;
}) {
  const codecCaps = caps?.codecs.find((c) => c.codec === profile.codec) ?? null;
  const qp = profile.rateControl.mode === "cqp" ? profile.rateControl.qp : null;

  const usable = link ? link.linkSpeedBps * 0.85 : null;
  const bps = evaluation?.estimatedBps ?? 0;
  const pct = usable ? Math.min(100, (bps / usable) * 100) : null;
  const blocked = evaluation?.issues.some((i) => i.blocking) ?? false;

  return (
    <section className="quality">
      <div className="quality-head">
        <h2 className="section-title" style={{ margin: 0 }}>
          Qualität
        </h2>
        {caps && (
          <span className="gpu-tag">
            {caps.adapter} · {caps.encoderEngines} NVENC
          </span>
        )}
      </div>

      <div className="presets">
        {PRESETS.map((p) => {
          const candidate = p.make();
          const active =
            candidate.codec === profile.codec &&
            candidate.chroma === profile.chroma &&
            candidate.depth === profile.depth &&
            JSON.stringify(candidate.rateControl) === JSON.stringify(profile.rateControl);
          return (
            <button
              key={p.key}
              className={`preset ${active ? "preset-active" : ""}`}
              onClick={() => onChange({ ...p.make(), width: profile.width, height: profile.height, fpsNum: profile.fpsNum, fpsDen: profile.fpsDen })}
            >
              <span className="preset-label">{p.label}</span>
              <span className="preset-hint">{p.hint}</span>
            </button>
          );
        })}
      </div>

      <div className="controls">
        <Field label="Codec">
          <div className="seg">
            {(caps?.codecs ?? []).map((c) => (
              <button
                key={c.codec}
                className={`seg-btn ${profile.codec === c.codec ? "seg-on" : ""}`}
                onClick={() => onChange(coerce({ ...profile, codec: c.codec }, caps))}
              >
                {c.label}
              </button>
            ))}
            {!caps && <span className="dim">…</span>}
          </div>
        </Field>

        <Field label="Chroma">
          <div className="seg">
            <SegBtn
              on={profile.chroma === "yuv444"}
              disabled={codecCaps ? !codecCaps.yuv444 : false}
              onClick={() => onChange({ ...profile, chroma: "yuv444" as Chroma })}
            >
              4:4:4
            </SegBtn>
            <SegBtn
              on={profile.chroma === "yuv420"}
              onClick={() => onChange({ ...profile, chroma: "yuv420" as Chroma })}
            >
              4:2:0
            </SegBtn>
          </div>
        </Field>

        <Field label="Bittiefe">
          <div className="seg">
            <SegBtn
              on={profile.depth === "ten"}
              disabled={codecCaps ? !codecCaps.tenBit : false}
              onClick={() => onChange({ ...profile, depth: "ten" as BitDepth })}
            >
              10-bit
            </SegBtn>
            <SegBtn
              on={profile.depth === "eight"}
              onClick={() => onChange({ ...profile, depth: "eight" as BitDepth })}
            >
              8-bit
            </SegBtn>
          </div>
        </Field>

        <Field label="Auflösung">
          <div className="seg">
            {RESOLUTIONS.map((r) => (
              <SegBtn
                key={r.label}
                on={profile.width === r.w && profile.height === r.h}
                onClick={() => onChange({ ...profile, width: r.w, height: r.h })}
              >
                {r.label}
              </SegBtn>
            ))}
          </div>
        </Field>

        <Field label="Bildrate">
          <div className="seg">
            {RATES.map((f) => (
              <SegBtn
                key={f}
                on={profile.fpsNum === f && profile.fpsDen === 1}
                onClick={() => onChange({ ...profile, fpsNum: f, fpsDen: 1 })}
              >
                {f}
              </SegBtn>
            ))}
          </div>
        </Field>
      </div>

      {qp !== null && (
        <div className="qp">
          <div className="qp-head">
            <span className="field-label">Quantisierung</span>
            <span className="qp-val">
              QP {qp} <span className="dim">· {qpVerdict(qp)}</span>
            </span>
          </div>
          <input
            type="range"
            min={10}
            max={34}
            step={1}
            value={qp}
            onChange={(e) =>
              onChange({
                ...profile,
                rateControl: { mode: "cqp", qp: Number(e.target.value) },
              })
            }
            className="slider"
          />
          <div className="qp-scale">
            <span>10 · nahezu verlustfrei</span>
            <span>34 · sichtbar komprimiert</span>
          </div>
        </div>
      )}

      <div className={`estimate ${blocked ? "estimate-bad" : ""}`}>
        <div className="estimate-head">
          <span className="field-label">Geschätzt über die Leitung</span>
          <span className="estimate-num">
            {(bps / 1e6).toFixed(0)} Mbit/s
            {usable && <span className="dim"> von {(usable / 1e6).toFixed(0)}</span>}
          </span>
        </div>
        {pct !== null && (
          <div className="meter">
            <div
              className="meter-fill"
              style={{ width: `${pct}%`, opacity: blocked ? 0.5 : 1 }}
            />
          </div>
        )}
        {!link && <p className="dim small">Keine Direktverbindung erkannt — Budget unbekannt.</p>}

        {evaluation?.issues.map((i, n) => (
          <p key={n} className={`issue ${i.blocking ? "issue-block" : ""}`}>
            {i.message}
          </p>
        ))}
      </div>
    </section>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="field">
      <span className="field-label">{label}</span>
      {children}
    </div>
  );
}

function SegBtn({
  on,
  disabled,
  onClick,
  children,
}: {
  on: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: React.ReactNode;
}) {
  return (
    <button
      className={`seg-btn ${on ? "seg-on" : ""}`}
      disabled={disabled}
      onClick={onClick}
      title={disabled ? "Von dieser GPU für diesen Codec nicht unterstützt" : undefined}
    >
      {children}
    </button>
  );
}

/**
 * Pull a profile back to something the newly chosen codec can actually do.
 *
 * Switching to AV1 on Ada with 4:4:4 selected would otherwise leave the UI in a
 * state that only fails once encoding starts.
 */
function coerce(p: Profile, caps: GpuCaps | null): Profile {
  const c = caps?.codecs.find((x) => x.codec === p.codec);
  if (!c) return p;
  return {
    ...p,
    chroma: p.chroma === "yuv444" && !c.yuv444 ? "yuv420" : p.chroma,
    depth: p.depth === "ten" && !c.tenBit ? "eight" : p.depth,
  };
}

function qpVerdict(qp: number): string {
  if (qp <= 14) return "nahezu verlustfrei";
  if (qp <= 20) return "visuell transparent";
  if (qp <= 26) return "sehr gut";
  return "sichtbare Artefakte";
}

export const DEFAULT_PROFILE: Profile = PRESETS[0].make();
export type { Codec };
