import type { TempPoint } from "../useStatus";
import type { PrinterStatus } from "../types";
import { Sparkline, TempReadout } from "./widgets";

export function TempSection({ s, history }: { s: PrinterStatus; history: TempPoint[] }) {
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
      {/* One tier per metric: each temperature shows its value and its own graph
          together (split by meaning, not "values" then "a graph"). */}
      <div className="temps">
        <div className="temprow">
          <TempReadout label="nozzle" cur={s.nozzle_temper} target={s.nozzle_target} accent />
          <Sparkline history={history} metric="nozzle" />
        </div>
        <div className="temprow">
          <TempReadout label="bed" cur={s.bed_temper} target={s.bed_target} />
          <Sparkline history={history} metric="bed" />
        </div>
      </div>
      {fans.length > 0 && (
        <div className="fans">
          {fans.map(([name, v]) => (
            <span key={name} className={`fan${running && v === 0 ? " fan--warn" : ""}`}>
              <span className="lbl">{name}</span>
              <span className="fan__v">{v}</span>
              {/* CUD: a stopped fan mid-print is a warning — flag it with a shape
                  (⚠), not just the red number, so it reads without colour. */}
              {running && v === 0 && (
                <span className="fan__warn" title="fan stopped mid-print">
                  ⚠ stopped
                </span>
              )}
            </span>
          ))}
        </div>
      )}
    </section>
  );
}
