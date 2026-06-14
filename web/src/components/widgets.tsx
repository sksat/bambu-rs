import type { TempPoint } from "../useStatus";
import type { AmsTray } from "../types";
import { remainText, swatch } from "../format";

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

export function Bar({ pct, prep }: { pct: number; prep?: boolean }) {
  return (
    <div className="bar">
      <div
        className={`bar__fill${prep ? " bar__fill--prep" : ""}`}
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
  const cls = `tray${t.is_active ? " tray--active" : t.is_target ? " tray--target" : ""}`;
  return (
    <div className={cls} data-testid={`tray-${ext ? "ext" : t.id}`} title={t.name ?? undefined}>
      <span className="tray__sw" style={color ? { background: color } : { borderStyle: "dashed" }} />
      <span className="tray__id">{ext ? "EXT" : t.id}</span>
      <span className="tray__mat">{t.material ?? "—"}</span>
      <span className="tray__rem">{remainText(t.remain)}</span>
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
