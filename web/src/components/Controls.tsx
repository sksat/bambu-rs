import { useState } from "react";
import type { Control } from "../useControl";

export function Controls({ control }: { control: Control }) {
  const b = control.busy;
  const [line, setLine] = useState("");
  const [force, setForce] = useState(false);
  const sendGcode = () => {
    const l = line.trim();
    if (l) void control.gcode(l, force);
  };
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
          <button
            key={l}
            className="btn btn--sm"
            disabled={!!b}
            onClick={() => void control.speed(l)}
          >
            {l}
          </button>
        ))}
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
        <button className="btn btn--sm" disabled={!!b} onClick={sendGcode} data-testid="gcode-send">
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
