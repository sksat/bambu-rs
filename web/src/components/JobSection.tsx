import type { PrinterStatus } from "../types";
import { fmtEta, speedName } from "../format";
import { Bar, Field, Thumb } from "./widgets";

export function JobSection({ s }: { s: PrinterStatus }) {
  const state = s.gcode_state ?? "—";
  const pct = s.mc_percent ?? 0;
  const prepPct = s.gcode_file_prepare_percent ?? 0;
  const preparing = prepPct > 0 && pct === 0;
  const shown = preparing ? prepPct : pct;
  const file = s.gcode_file ?? s.subtask_name;
  return (
    <section className="panel span-all">
      <div className="job">
        {file && <Thumb file={file} className="thumb thumb--job" />}
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
      <Bar pct={shown} prep={preparing} running={state === "RUNNING"} />
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
