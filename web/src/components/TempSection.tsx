import { useState } from "react";
import type { TempPoint } from "../useStatus";
import type { Control } from "../useControl";
import type { PrinterStatus } from "../types";
import { Sparkline, TempReadout } from "./widgets";

export function TempSection({
  s,
  history,
  control,
}: {
  s: PrinterStatus;
  history: TempPoint[];
  control: Control;
}) {
  const b = control.busy;
  const [nozzleInput, setNozzleInput] = useState("");
  const [bedInput, setBedInput] = useState("");

  const setTemp = (part: "nozzle" | "bed", raw: string) => {
    const v = Number(raw);
    if (!Number.isFinite(v)) return;
    void control.setTemp(part, v, true);
  };

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
    <section className="panel" data-testid="temperature">
      <div className="lbl">temperature</div>
      {/* One tier per metric: each temperature shows its value, its own graph, and
          its set/cool control together (split by meaning, not "values" then "a
          graph" then "controls" off in another panel). */}
      <div className="temps">
        <div className="temprow">
          <TempReadout label="nozzle" cur={s.nozzle_temper} target={s.nozzle_target} accent />
          <Sparkline history={history} metric="nozzle" />
          <div className="tctl">
            <input
              className="pw tctl__in"
              inputMode="numeric"
              placeholder="°C"
              value={nozzleInput}
              disabled={!!b}
              onChange={(e) => setNozzleInput(e.target.value)}
              data-testid="temp-nozzle-input"
            />
            <button
              className="btn btn--sm"
              disabled={!!b || !nozzleInput.trim()}
              data-testid="temp-nozzle-set"
              onClick={() => setTemp("nozzle", nozzleInput)}
            >
              set
            </button>
            <button
              className="btn btn--sm"
              disabled={!!b}
              data-testid="temp-nozzle-cool"
              onClick={() => void control.cooldown("nozzle")}
            >
              cool
            </button>
          </div>
        </div>
        <div className="temprow">
          <TempReadout label="bed" cur={s.bed_temper} target={s.bed_target} />
          <Sparkline history={history} metric="bed" />
          <div className="tctl">
            <input
              className="pw tctl__in"
              inputMode="numeric"
              placeholder="°C"
              value={bedInput}
              disabled={!!b}
              onChange={(e) => setBedInput(e.target.value)}
              data-testid="temp-bed-input"
            />
            <button
              className="btn btn--sm"
              disabled={!!b || !bedInput.trim()}
              data-testid="temp-bed-set"
              onClick={() => setTemp("bed", bedInput)}
            >
              set
            </button>
            <button
              className="btn btn--sm"
              disabled={!!b}
              data-testid="temp-bed-cool"
              onClick={() => void control.cooldown("bed")}
            >
              cool
            </button>
          </div>
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
