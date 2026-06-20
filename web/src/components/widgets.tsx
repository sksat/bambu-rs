import { useState } from "react";
import type { CSSProperties } from "react";
import type { TempPoint } from "../useStatus";
import type { AmsTray } from "../types";
import { remainText, swatch, trayLabel, wifiTier } from "../format";

/// The plate preview embedded in a sliced .3mf; renders nothing if absent or for
/// non-.3mf files.
export function Thumb({ file, className }: { file: string; className?: string }) {
  const [ok, setOk] = useState(true);
  if (!ok || !file.toLowerCase().endsWith(".3mf")) return null;
  const name = file.startsWith("/") ? file : `/${file}`;
  return (
    <img
      className={className ?? "thumb"}
      loading="lazy"
      alt=""
      src={`/api/file/thumbnail?name=${encodeURIComponent(name)}`}
      onError={() => setOk(false)}
      data-testid="thumb"
    />
  );
}

export function Field({ label, value, big }: { label: string; value: string; big?: boolean }) {
  return (
    <div className="field">
      <span className="lbl">{label}</span>
      <span className={`val${big ? " val--big" : ""}`} data-testid={`f-${label}`}>
        {value}
      </span>
    </div>
  );
}

export function Bar({ pct, prep, running }: { pct: number; prep?: boolean; running?: boolean }) {
  const tone = prep ? " bar__fill--prep" : running ? " bar__fill--running" : "";
  return (
    <div className="bar">
      <div
        className={`bar__fill${tone}`}
        style={{ width: `${Math.max(0, Math.min(100, pct))}%` }}
        data-testid="bar-fill"
      />
    </div>
  );
}

// The AMS reports a coarse 1–5 level on the wire field `humidity`, but a HIGHER
// number means DRIER (verified against this device: level 5 with raw 0 = dry; the
// upstream field name reads backwards). Render it as a 5-segment dryness meter
// (filled from the humid end up to the level) — a discrete readout gauge, NOT a
// track-and-thumb, which read as an operable slider. The filled count + the
// humid/dry end labels carry level and direction without colour (CUD); colour
// just reinforces (more filled = drier; dry end green, humid end red). The exact
// verdict word (dry/good/fair/damp/wet) stays in the hover/aria title.
const DRY_WORD: Record<number, string> = { 5: "dry", 4: "good", 3: "fair", 2: "damp", 1: "wet" };

export function Humidity({ level, raw }: { level: number; raw?: number | null }) {
  const word = DRY_WORD[level] ?? `${level}/5`;
  const tone = level >= 4 ? "ok" : level === 3 ? "warn" : "err";
  const lvl = Math.min(5, Math.max(1, level));
  return (
    <span
      className={`hum hum--${tone}`}
      aria-label={`humidity ${level} of 5 (${word})`}
      title={`humidity ${level}/5 (${word})${raw != null ? ` · raw ${raw}` : ""} — higher is drier`}
    >
      <span className="hum__end">humid</span>
      <span className="hum__meter" aria-hidden="true">
        {[1, 2, 3, 4, 5].map((i) => (
          <span key={i} className={`hum__seg${i <= lvl ? " is-on" : ""}`} />
        ))}
      </span>
      <span className="hum__end">dry</span>
    </span>
  );
}

// Wi-Fi signal as a 4-bar meter: the filled bar count, the tone colour, and the
// dBm text all carry the strength, so it reads without relying on colour (CUD,
// same philosophy as Humidity). Renders nothing when the signal is absent or
// unparseable (the `<redacted>` sentinel is already None on the wire).
export function WifiSignal({ signal }: { signal: string | null }) {
  const t = wifiTier(signal);
  if (!t) return null;
  return (
    <span
      className={`wifi wifi--${t.tone}`}
      aria-label={`wifi ${t.dbm}dBm (${t.word})`}
      title={`wifi ${t.dbm}dBm — ${t.word}`}
      data-testid="wifi"
    >
      <span className="wifi__bars" aria-hidden="true">
        {[1, 2, 3, 4].map((i) => (
          <span key={i} className={`wifi__bar${i <= t.bars ? " is-on" : ""}`} />
        ))}
      </span>
      <span className="wifi__v">{t.dbm}dBm</span>
    </span>
  );
}

export function TempReadout({
  label,
  cur,
  target,
  accent,
}: {
  label: string;
  cur?: number | null;
  target?: number | null;
  accent?: boolean;
}) {
  return (
    <div className="tr">
      <span className="lbl">{label}</span>
      <span className={`tr__v${accent ? " tr__v--accent" : ""}`} data-testid={`${label}-temp`}>
        {cur != null ? cur.toFixed(1) : "—"}
        <span className="tr__u">°C</span>
      </span>
      <span className="tr__t">{target ? `▏ target ${Math.round(target)}°` : "off"}</span>
    </div>
  );
}

export function Tray({ t, ext }: { t: AmsTray; ext?: boolean }) {
  const color = swatch(t.color, t.cols);
  // State is never colour-only: the active spool also widens its ring + shows a
  // "loaded" marker; the swap target gets a dashed accent ring.
  const target = t.is_target && !t.is_active;
  const cls = [
    "spool",
    ext ? "spool--ext" : "",
    t.is_active ? "spool--active" : "",
    target ? "spool--target" : "",
    color ? "" : "spool--empty",
  ]
    .filter(Boolean)
    .join(" ");
  const temps =
    t.nozzle_temp_min != null && t.nozzle_temp_max != null
      ? `${t.nozzle_temp_min}–${t.nozzle_temp_max}°C`
      : null;
  const title = [t.name ?? t.material ?? undefined, temps].filter(Boolean).join(" · ") || undefined;
  // 1-based to match the number printed on the AMS; data-testid keeps the raw
  // 0-based wire id so tests/automation address trays by the protocol value.
  const id = ext ? "EXT" : trayLabel(t.id);
  const rem = remainText(t.remain);
  return (
    <div className={cls} data-testid={`tray-${ext ? "ext" : t.id}`} title={title}>
      <span
        className="spool__disc"
        // The filament colour is the hero; the hub punches a hole using the panel
        // background so the ring reads as a wound spool seen face-on.
        style={color ? ({ "--fil": color } as CSSProperties) : undefined}
        aria-hidden
      >
        {t.is_active && <i className="spool__load" aria-hidden />}
      </span>
      <span className="spool__id">
        {id}
        <span className="spool__mat"> · {t.material ?? "—"}</span>
      </span>
      {rem !== "—" ? <span className="spool__rem">{rem}</span> : null}
    </div>
  );
}

// One series per chart: the nozzle and bed each get their own value+graph tier,
// so the section reads by meaning (per metric), not "values" then "a graph".
export function Sparkline({ history, metric }: { history: TempPoint[]; metric: "nozzle" | "bed" }) {
  const W = 600;
  const H = 40;
  const pad = 3;
  const sel = (p: TempPoint) => (metric === "nozzle" ? p.nozzle : p.bed);
  const vals = history.map(sel).filter((v): v is number => v != null);
  if (history.length < 2 || vals.length === 0) {
    return <div className="spark spark--empty" data-testid={`spark-${metric}`} />;
  }
  const min = Math.min(...vals);
  const max = Math.max(...vals);
  const range = Math.max(1, max - min);
  const x = (i: number) => pad + (i / (history.length - 1)) * (W - 2 * pad);
  const y = (v: number) => pad + (1 - (v - min) / range) * (H - 2 * pad);
  const points = history
    .map((p, i) => {
      const v = sel(p);
      return v == null ? null : `${x(i).toFixed(1)},${y(v).toFixed(1)}`;
    })
    .filter(Boolean)
    .join(" ");
  return (
    <svg
      className="spark"
      viewBox={`0 0 ${W} ${H}`}
      preserveAspectRatio="none"
      data-testid={`spark-${metric}`}
    >
      <polyline className={`spark__${metric}`} points={points} />
    </svg>
  );
}
