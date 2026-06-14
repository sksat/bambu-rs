import { useState } from "react";
import type { Control } from "../useControl";
import type { PrinterStatus } from "../types";

// Map the active speed tier (`spd_lvl`) to the command name the buttons send.
const SPEED_BY_LVL: Record<number, string> = { 1: "silent", 2: "standard", 3: "sport", 4: "ludicrous" };

// The chamber light's authoritative state, read from `lights_report`.
function chamberLight(s: PrinterStatus): "on" | "off" | "unknown" {
  const l = s.lights?.find((x) => x.node === "chamber_light" || x.node === "chamber");
  return l?.mode === "on" ? "on" : l?.mode === "off" ? "off" : "unknown";
}

// Which job actions the current gcode state actually allows.
function jobAvail(state: string | null) {
  const s = (state ?? "").toUpperCase();
  return {
    canPause: s === "RUNNING",
    canResume: s === "PAUSE",
    canStop: s === "RUNNING" || s === "PAUSE" || s === "PREPARE" || s === "SLICING",
  };
}

export function Controls({ control, status }: { control: Control; status: PrinterStatus }) {
  const b = control.busy;
  const [line, setLine] = useState("");
  const [force, setForce] = useState(false);
  const sendGcode = () => {
    const l = line.trim();
    if (l) void control.gcode(l, force);
  };

  const light = chamberLight(status);
  const lightBusy = b?.startsWith("light") ?? false;
  const lightLabel = light === "on" ? "light on" : light === "off" ? "light off" : "light —";

  const activeSpeed = SPEED_BY_LVL[status.spd_lvl ?? 0];

  const { canPause, canResume, canStop } = jobAvail(status.gcode_state);
  const stateName = status.gcode_state ?? "unknown";

  return (
    <section className="panel">
      <div className="lbl">controls</div>
      <div className="btns">
        <button
          className="btn"
          disabled={!!b || !canPause}
          title={canPause ? undefined : `pause unavailable while ${stateName}`}
          onClick={() => void control.pause()}
        >
          pause
        </button>
        <button
          className="btn"
          disabled={!!b || !canResume}
          title={canResume ? undefined : `resume unavailable while ${stateName}`}
          onClick={() => void control.resume()}
        >
          resume
        </button>
        <button
          className="btn btn--danger"
          disabled={!!b || !canStop}
          title={canStop ? undefined : `stop unavailable while ${stateName}`}
          onClick={() => control.requestStop()}
        >
          stop
        </button>
      </div>
      <div className="btns">
        <button
          className={`btn light-toggle${light === "on" ? " is-on" : ""}`}
          role="switch"
          aria-checked={light === "on"}
          aria-busy={lightBusy}
          disabled={!!b}
          data-testid="light-toggle"
          data-state={light}
          title={
            light === "unknown"
              ? "light state unknown — click to turn on"
              : `chamber light ${light}`
          }
          onClick={() => void control.light(light !== "on")}
        >
          <span className="dot" aria-hidden="true" />
          {lightLabel}
        </button>
      </div>
      <div className="btns">
        {["silent", "standard", "sport", "ludicrous"].map((l) => {
          const active = l === activeSpeed;
          return (
            <button
              key={l}
              className={`btn btn--sm${active ? " is-active" : ""}`}
              disabled={!!b}
              aria-pressed={active}
              data-testid={`speed-${l}`}
              onClick={() => void control.speed(l)}
            >
              {l}
            </button>
          );
        })}
      </div>
      <div className="gcoderow">
        <span className="lbl">gcode</span>
        <input
          className="pw"
          placeholder="e.g. G28"
          value={line}
          disabled={!!b}
          onChange={(e) => setLine(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") sendGcode();
          }}
          data-testid="gcode-input"
        />
        <button
          className="btn btn--sm"
          disabled={!!b || !line.trim()}
          onClick={sendGcode}
          data-testid="gcode-send"
        >
          send
        </button>
        <label className="startrow" title="override the safety blocklist">
          <input type="checkbox" checked={force} onChange={(e) => setForce(e.target.checked)} />
          <span className="lbl">force</span>
        </label>
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

export function ConfirmDialog({
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
