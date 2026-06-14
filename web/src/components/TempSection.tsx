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
