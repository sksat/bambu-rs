import { useState } from "react";
import type { CSSProperties } from "react";
import type { TempPoint } from "../useStatus";
import type { AmsTray } from "../types";
import { remainText, swatch } from "../format";

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
      src={`/api/files/thumbnail?name=${encodeURIComponent(name)}`}
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

export function Humidity({ level }: { level: number }) {
  return (
    <span className="hum" aria-label={`humidity ${level} of 5`}>
      {[1, 2, 3, 4, 5].map((i) => (
        <i key={i} className={`hum__seg${i <= level ? " on" : ""}`} />
      ))}
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
  const id = ext ? "EXT" : t.id;
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

export function Sparkline({ history }: { history: TempPoint[] }) {
  const W = 600;
  const H = 56;
  const pad = 3;
  const all = history.flatMap((p) => [p.nozzle, p.bed]).filter((v): v is number => v != null);
  if (history.length < 2 || all.length === 0) {
    return <div className="spark spark--empty" data-testid="spark" />;
  }
  const min = Math.min(...all);
  const max = Math.max(...all);
  const range = Math.max(1, max - min);
  const x = (i: number) => pad + (i / (history.length - 1)) * (W - 2 * pad);
  const y = (v: number) => pad + (1 - (v - min) / range) * (H - 2 * pad);
  const line = (sel: (p: TempPoint) => number | null) =>
    history
      .map((p, i) => {
        const v = sel(p);
        return v == null ? null : `${x(i).toFixed(1)},${y(v).toFixed(1)}`;
      })
      .filter(Boolean)
      .join(" ");
  return (
    <svg className="spark" viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" data-testid="spark">
      <polyline className="spark__bed" points={line((p) => p.bed)} />
      <polyline className="spark__nozzle" points={line((p) => p.nozzle)} />
    </svg>
  );
}
