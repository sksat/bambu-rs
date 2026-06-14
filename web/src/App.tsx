import { useEffect, useState } from "react";
import { useStatus } from "./useStatus";
import type { Conn, TempPoint } from "./useStatus";
import { sendControl } from "./control";
import type { Ams, AmsTray, PrinterStatus } from "./types";
import "./app.css";

// Instrument / telemetry dashboard: dense tabular readouts, one accent, panels
// that reflow fluidly from a single phone column to a multi-column desktop grid
// (resolution-driven, not a separate mobile build). Driven by the /api/ws stream.
export function App() {
  const { status, conn, history } = useStatus();
  const control = useControl();
  return (
    <div className="term">
      <TopBar conn={conn} />
      {status ? (
        <main className="grid">
          <JobSection s={status} />
          <TempSection s={status} history={history} />
          {status.ams && <AmsSection ams={status.ams} />}
          <Controls control={control} />
          <HealthSection s={status} />
          <FooterSection s={status} />
        </main>
      ) : (
        <p className="waiting" data-testid="waiting">
          awaiting telemetry…
        </p>
      )}
      {control.toast && (
        <div className={`toast toast--${control.toast.kind}`} data-testid="toast">
          {control.toast.msg}
        </div>
      )}
      {control.confirmStop && (
        <ConfirmDialog
          message="Stop the running print? This can't be undone."
          onConfirm={() => control.confirmAndStop()}
          onCancel={() => control.cancelStop()}
        />
      )}
    </div>
  );
}

function TopBar({ conn }: { conn: Conn }) {
  return (
    <header className="hdr">
      <span className="hdr__brand">
        bambu<span className="dim"> / dashboard</span>
      </span>
      <span className={`conn conn--${conn}`} data-testid="conn">
        <i className="dot" />
        {conn}
      </span>
    </header>
  );
}

// ── control state ───────────────────────────────────────────────────────────

interface Toast {
  kind: "ok" | "warn" | "err";
  msg: string;
}

function useControl() {
  const [password, setPassword] = useState<string>(() => sessionStorage.getItem("bambu_pw") ?? "");
  const [needPassword, setNeedPassword] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<Toast | null>(null);
  const [confirmStop, setConfirmStop] = useState(false);

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(t);
  }, [toast]);

  const act = async (label: string, path: string, body: Record<string, unknown> | null) => {
    setBusy(label);
    const res = await sendControl(path, body, password || null);
    setBusy(null);
    switch (res.kind) {
      case "ok":
        setToast({ kind: "ok", msg: `${label}: verified` });
        break;
      case "accepted":
        setToast({ kind: "warn", msg: `${label}: sent (unverified)` });
        break;
      case "rejected":
        setToast({ kind: "err", msg: `${label}: rejected — ${res.reason}` });
        break;
      case "needPassword":
        setNeedPassword(true);
        setToast({ kind: "err", msg: "control needs a password" });
        break;
      case "error":
        setToast({ kind: "err", msg: `${label}: ${res.message}` });
        break;
    }
  };

  return {
    password,
    needPassword,
    busy,
    toast,
    confirmStop,
    setPassword: (pw: string) => {
      setPassword(pw);
      sessionStorage.setItem("bambu_pw", pw);
    },
    pause: () => act("pause", "/api/job/pause", { confirm: true }),
    resume: () => act("resume", "/api/job/resume", { confirm: true }),
    requestStop: () => setConfirmStop(true),
    cancelStop: () => setConfirmStop(false),
    confirmAndStop: () => {
      setConfirmStop(false);
      void act("stop", "/api/job/stop", { confirm: true });
    },
    light: (on: boolean) => act(`light ${on ? "on" : "off"}`, "/api/light", { node: "chamber", on }),
    speed: (level: string) => act(`speed ${level}`, "/api/speed", { level }),
  };
}

type Control = ReturnType<typeof useControl>;

function Controls({ control }: { control: Control }) {
  const b = control.busy;
  return (
    <section className="panel">
      <div className="lbl">controls</div>
      <div className="btns">
        <button className="btn" disabled={!!b} onClick={() => void control.pause()}>
          pause
        </button>
        <button className="btn" disabled={!!b} onClick={() => void control.resume()}>
          resume
        </button>
        <button className="btn btn--danger" disabled={!!b} onClick={() => control.requestStop()}>
          stop
        </button>
      </div>
      <div className="btns">
        <button className="btn" disabled={!!b} onClick={() => void control.light(true)}>
          light on
        </button>
        <button className="btn" disabled={!!b} onClick={() => void control.light(false)}>
          light off
        </button>
      </div>
      <div className="btns">
        {["silent", "standard", "sport", "ludicrous"].map((l) => (
          <button key={l} className="btn btn--sm" disabled={!!b} onClick={() => void control.speed(l)}>
            {l}
          </button>
        ))}
      </div>
      {control.needPassword && (
        <label className="pwrow">
          <span className="lbl">control password</span>
          <input
            type="password"
            className="pw"
            autoComplete="off"
            value={control.password}
            onChange={(e) => control.setPassword(e.target.value)}
            placeholder="set, then retry"
            data-testid="password"
          />
        </label>
      )}
    </section>
  );
}

function ConfirmDialog({
  message,
  onConfirm,
  onCancel,
}: {
  message: string;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="modal" role="dialog" aria-modal="true" data-testid="confirm">
      <div className="modal__box">
        <p className="modal__msg">{message}</p>
        <div className="btns">
          <button className="btn" onClick={onCancel}>
            cancel
          </button>
          <button className="btn btn--danger" onClick={onConfirm} data-testid="confirm-stop">
            stop print
          </button>
        </div>
      </div>
    </div>
  );
}

// ── read-only sections ──────────────────────────────────────────────────────

function JobSection({ s }: { s: PrinterStatus }) {
  const state = s.gcode_state ?? "—";
  const pct = s.mc_percent ?? 0;
  const prepPct = s.gcode_file_prepare_percent ?? 0;
  const preparing = prepPct > 0 && pct === 0;
  const shown = preparing ? prepPct : pct;
  const file = s.gcode_file ?? s.subtask_name;
  return (
    <section className="panel span-all">
      <div className="job">
        <span className={`state state--${state.toLowerCase()}`} data-testid="state">
          {state}
        </span>
        <div className="job__meta">
          {file && <div className="job__file">{file}</div>}
          <div className="chips">
            {s.spd_lvl != null && (
              <span className="chip">
                {speedName(s.spd_lvl)}
                {s.spd_mag != null && s.spd_mag !== 100 ? ` ${s.spd_mag}%` : ""}
              </span>
            )}
            {s.stage && s.stage !== "no_stage" && (
              <span className="chip">{s.stage.replace(/_/g, " ")}</span>
            )}
            {preparing && <span className="chip warn">preparing</span>}
          </div>
        </div>
      </div>
      <Bar pct={shown} prep={preparing} />
      <div className="readline">
        <Field label="progress" value={`${shown}%`} big />
        {s.layer_num != null && (
          <Field
            label="layer"
            value={`${s.layer_num}${s.total_layer_num ? ` / ${s.total_layer_num}` : ""}`}
          />
        )}
        {s.remaining_time_min != null && <Field label="eta" value={fmtEta(s.remaining_time_min)} />}
      </div>
    </section>
  );
}

function TempSection({ s, history }: { s: PrinterStatus; history: TempPoint[] }) {
  const fans: Array<[string, number]> = (
    [
      ["part", s.cooling_fan_speed],
      ["aux", s.big_fan1_speed],
      ["chamber", s.big_fan2_speed],
      ["hotend", s.heatbreak_fan_speed],
    ] as Array<[string, number | null | undefined]>
  ).filter((e): e is [string, number] => e[1] != null);
  const running = s.gcode_state === "RUNNING";
  return (
    <section className="panel">
      <div className="lbl">temperatures</div>
      <div className="temps">
        <TempReadout label="nozzle" cur={s.nozzle_temper} target={s.nozzle_target} accent />
        <TempReadout label="bed" cur={s.bed_temper} target={s.bed_target} />
      </div>
      <Sparkline history={history} />
      {fans.length > 0 && (
        <div className="fans">
          {fans.map(([name, v]) => (
            <span key={name} className={`fan${running && v === 0 ? " fan--warn" : ""}`}>
              <span className="lbl">{name}</span>
              <span className="fan__v">{v}</span>
            </span>
          ))}
        </div>
      )}
    </section>
  );
}

function TempReadout({
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

function AmsSection({ ams }: { ams: Ams }) {
  const units = ams.units ?? [];
  const active = ams.active_tray;
  const swapping = active != null && ams.target_tray != null && active !== ams.target_tray;
  return (
    <section className="panel">
      <div className="secline">
        <span className="lbl">ams</span>
        {swapping ? (
          <span className="swap" data-testid="ams-swap">
            swapping {active} → {ams.target_tray}
          </span>
        ) : (
          <span className="dim">
            {active && active !== "255" ? `tray ${active} loaded` : "idle"}
          </span>
        )}
      </div>
      {units.map((u) => (
        <div key={u.id} className="amsunit">
          <div className="trays">
            {(u.trays ?? []).map((t) => (
              <Tray key={t.id} t={t} />
            ))}
            {ams.external && <Tray t={ams.external} ext />}
          </div>
          {u.humidity != null && (
            <div className="amsmeta">
              <span className="lbl">humidity</span>
              <Humidity level={u.humidity} />
              {u.temp ? <span className="dim"> {u.temp.toFixed(0)}°C</span> : null}
            </div>
          )}
        </div>
      ))}
    </section>
  );
}

function Tray({ t, ext }: { t: AmsTray; ext?: boolean }) {
  const color = swatch(t.color, t.cols);
  const cls = `tray${t.is_active ? " tray--active" : t.is_target ? " tray--target" : ""}`;
  return (
    <div className={cls} data-testid={`tray-${ext ? "ext" : t.id}`} title={t.name ?? undefined}>
      <span
        className="tray__sw"
        style={color ? { background: color } : { borderStyle: "dashed" }}
      />
      <span className="tray__id">{ext ? "EXT" : t.id}</span>
      <span className="tray__mat">{t.material ?? "—"}</span>
      <span className="tray__rem">{remainText(t.remain)}</span>
    </div>
  );
}

function HealthSection({ s }: { s: PrinterStatus }) {
  const hms = s.hms ?? [];
  if (hms.length === 0 && !s.error) return null;
  return (
    <section className="panel span-all sec--health" data-testid="health">
      <div className="secline">
        <span className="lbl alert">health</span>
      </div>
      {s.error && (
        <div className="arow sev-err">
          <span className="acode">{s.error.hex}</span>
          <span className="chip">device error</span>
          <a href={s.error.lookup_url} target="_blank" rel="noreferrer">
            troubleshoot ↗
          </a>
        </div>
      )}
      {hms.map((a, i) => (
        <div key={i} className={`arow sev-${a.severity}`}>
          <span className="acode">{a.code_hyphen}</span>
          <span className="chip">{a.module}</span>
          {a.is_lidar && <span className="chip">lidar</span>}
          <a href={a.wiki} target="_blank" rel="noreferrer">
            troubleshoot ↗
          </a>
        </div>
      ))}
    </section>
  );
}

function FooterSection({ s }: { s: PrinterStatus }) {
  const chips: Array<[string, string]> = [];
  if (s.wifi_signal) chips.push(["wifi", s.wifi_signal]);
  if (s.sdcard != null) chips.push(["sd card", s.sdcard ? "present" : "none"]);
  if (s.nozzle_diameter) {
    chips.push(["nozzle", `${s.nozzle_diameter}${s.nozzle_type ? ` ${s.nozzle_type.replace(/_/g, " ")}` : ""}`]);
  }
  if (s.online?.rfid != null) chips.push(["rfid", s.online.rfid ? "online" : "offline"]);
  if (s.ipcam?.timelapse) chips.push(["timelapse", s.ipcam.timelapse]);
  for (const l of s.lights ?? []) chips.push([l.node.replace(/_/g, " "), l.mode]);
  if (s.upgrade?.new_version_state === 1) chips.push(["firmware", "update available"]);
  if (chips.length === 0) return null;
  return (
    <section className="panel span-all foot" data-testid="foot">
      {chips.map(([k, v]) => (
        <span className="fchip" key={k}>
          <span className="lbl">{k}</span>
          <span className="fchip__v">{v}</span>
        </span>
      ))}
    </section>
  );
}

// ── small pieces ──────────────────────────────────────────────────────────

function Field({ label, value, big }: { label: string; value: string; big?: boolean }) {
  return (
    <div className="field">
      <span className="lbl">{label}</span>
      <span className={`val${big ? " val--big" : ""}`} data-testid={`f-${label}`}>
        {value}
      </span>
    </div>
  );
}

function Bar({ pct, prep }: { pct: number; prep?: boolean }) {
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

function Humidity({ level }: { level: number }) {
  return (
    <span className="hum" aria-label={`humidity ${level} of 5`}>
      {[1, 2, 3, 4, 5].map((i) => (
        <i key={i} className={`hum__seg${i <= level ? " on" : ""}`} />
      ))}
    </span>
  );
}

function Sparkline({ history }: { history: TempPoint[] }) {
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

// ── formatting ────────────────────────────────────────────────────────────

function fmtEta(min: number): string {
  if (min <= 0) return "done";
  if (min < 60) return `${min}m`;
  return `${Math.floor(min / 60)}h ${min % 60}m`;
}

function speedName(lvl: number): string {
  return ({ 1: "silent", 2: "standard", 3: "sport", 4: "ludicrous" } as Record<number, string>)[lvl] ?? `speed ${lvl}`;
}

// Bambu colours are RGBA hex (e.g. DE4343FF); drop the alpha for CSS.
function swatch(color?: string | null, cols?: string[]): string | null {
  const c = color ?? cols?.[0];
  if (!c) return null;
  return `#${c.length >= 6 ? c.slice(0, 6) : c}`;
}

function remainText(remain?: number | null): string {
  // A1 spools report 0 (no RFID weight) and -1 means unknown — neither is "0%".
  return remain != null && remain > 0 ? `${remain}%` : "—";
}
