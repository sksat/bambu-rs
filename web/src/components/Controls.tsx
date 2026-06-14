import type { Control } from "../useControl";

export function Controls({ control }: { control: Control }) {
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
