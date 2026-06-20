import type { PrinterStatus } from "../types";
import type { Control } from "../useControl";
import { fmtEta, fmtFinishTime, speedName } from "../format";
import { JobControls } from "./Controls";
import { Bar, Field, Thumb, WifiSignal } from "./widgets";

export function OverviewSection({ s, control }: { s: PrinterStatus; control: Control }) {
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
        {/* Hardware/connectivity cluster in the band's top-right corner: the
            installed nozzle spec and the WiFi signal you keep an eye on while a
            print runs. Each piece renders only when its data is present. */}
        <div className="job__hw">
          {s.nozzle_diameter && (
            <span className="jobspec" data-testid="nozzle-spec" title="installed nozzle">
              Ø{s.nozzle_diameter}
              {s.nozzle_type ? ` ${s.nozzle_type.replace(/_/g, " ")}` : ""}
            </span>
          )}
          <WifiSignal signal={s.wifi_signal} />
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
        {s.remaining_time_min != null && (
          <>
            <Field label="eta" value={fmtEta(s.remaining_time_min)} />
            {/* Absolute clock time the job should finish, so you can plan around
                it without doing now+eta math in your head. */}
            <Field label="done by" value={fmtFinishTime(s.remaining_time_min, new Date())} />
          </>
        )}
        {/* Pause/resume/stop sit at the end of the telemetry line, beside the
            progress/eta readouts you're already watching. */}
        <JobControls control={control} status={s} />
      </div>
    </section>
  );
}
