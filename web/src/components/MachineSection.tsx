import { useState } from "react";
import type { Control, Axis } from "../useControl";
import type { PrinterStatus } from "../types";

// Motion is unsafe mid-job: jog/home/calibrate are gated on these phases.
const BUSY_STATES = new Set(["RUNNING", "PAUSE", "PREPARE", "SLICING"]);
function isBusy(state: string | null): boolean {
  return BUSY_STATES.has((state ?? "").toUpperCase());
}

// Jog feedrates (mm/min): XY moves fast, Z is geared slower for safety.
const FEED_XY = 3000;
const FEED_Z = 600;
const FEED_EXTRUDE = 300;
const MIN_EXTRUDE_TEMP = 170; // cold extrusion guard (°C)

const STEPS = [0.1, 1, 10] as const;
const EXTRUDE_LENS = [1, 5, 10] as const;

function fmt(n: number | null): string {
  return n != null ? Math.round(n).toString() : "—";
}

export function MachineSection({ s, control }: { s: PrinterStatus; control: Control }) {
  const b = control.busy;
  const busy = isBusy(s.gcode_state);
  const stateName = s.gcode_state ?? "unknown";
  const [step, setStep] = useState<number>(1);
  const [extLen, setExtLen] = useState<number>(5);
  const [nozzleInput, setNozzleInput] = useState("");
  const [bedInput, setBedInput] = useState("");
  const [cal, setCal] = useState({ bed_level: false, vibration: false, motor_noise: false });

  // A jog/home is unavailable if a write is in flight OR the printer is busy.
  const motionDisabled = !!b || busy;
  const motionTitle = busy ? `unavailable while ${stateName}` : undefined;

  const jogTitle = (extra?: string) => (busy ? `unavailable while ${stateName}` : extra);

  const jog = (axis: Axis, dir: 1 | -1) =>
    void control.jog(axis, dir * step, axis === "z" ? FEED_Z : FEED_XY);

  const setTemp = (part: "nozzle" | "bed", raw: string) => {
    const v = Number(raw);
    if (!Number.isFinite(v)) return;
    void control.setTemp(part, v, true);
  };

  const nozzleCold = (s.nozzle_temper ?? 0) < MIN_EXTRUDE_TEMP;
  const extrudeDisabled = !!b || busy || nozzleCold;
  const extrudeTitle = busy
    ? `unavailable while ${stateName}`
    : nozzleCold
      ? "nozzle must be ≥170°C"
      : undefined;

  const anyCal = cal.bed_level || cal.vibration || cal.motor_noise;

  return (
    <div className="machine" data-testid="machine">
      <div className="lbl">machine</div>

      {/* ── MOVE: printer-shaped jog control ─────────────────────────────── */}
      <div className="msub">
        <div className="lbl">move</div>
        <div className="jog">
          {/* XY D-pad — a 3×3 grid laid out like the bed seen from above. */}
          <div className="jog__pad" role="group" aria-label="XY jog">
            <span className="jog__corner" aria-hidden />
            <button
              className="btn jog__btn"
              disabled={motionDisabled}
              title={motionTitle}
              data-testid="jog-yplus"
              onClick={() => jog("y", 1)}
            >
              <span className="jog__arrow">↑</span>
              <span className="jog__ax">Y+</span>
            </button>
            <span className="jog__corner" aria-hidden />

            <button
              className="btn jog__btn"
              disabled={motionDisabled}
              title={motionTitle}
              data-testid="jog-xminus"
              onClick={() => jog("x", -1)}
            >
              <span className="jog__arrow">←</span>
              <span className="jog__ax">X−</span>
            </button>
            <button
              className="btn jog__btn jog__home"
              disabled={motionDisabled}
              title={jogTitle("home all axes (G28)")}
              data-testid="home-all"
              onClick={() => void control.home("all")}
            >
              ⌂
            </button>
            <button
              className="btn jog__btn"
              disabled={motionDisabled}
              title={motionTitle}
              data-testid="jog-xplus"
              onClick={() => jog("x", 1)}
            >
              <span className="jog__arrow">→</span>
              <span className="jog__ax">X+</span>
            </button>

            <span className="jog__corner" aria-hidden />
            <button
              className="btn jog__btn"
              disabled={motionDisabled}
              title={motionTitle}
              data-testid="jog-yminus"
              onClick={() => jog("y", -1)}
            >
              <span className="jog__arrow">↓</span>
              <span className="jog__ax">Y−</span>
            </button>
            <span className="jog__corner" aria-hidden />
          </div>

          {/* Z column — the gantry, beside the bed. */}
          <div className="jog__z" role="group" aria-label="Z jog">
            <button
              className="btn jog__btn"
              disabled={motionDisabled}
              title={motionTitle}
              data-testid="jog-zplus"
              onClick={() => jog("z", 1)}
            >
              <span className="jog__arrow">↑</span>
              <span className="jog__ax">Z+</span>
            </button>
            <span className="jog__zlbl lbl">Z</span>
            <button
              className="btn jog__btn"
              disabled={motionDisabled}
              title={motionTitle}
              data-testid="jog-zminus"
              onClick={() => jog("z", -1)}
            >
              <span className="jog__arrow">↓</span>
              <span className="jog__ax">Z−</span>
            </button>
          </div>
        </div>

        {/* Step selector: how far each jog travels. */}
        <div className="seg" role="radiogroup" aria-label="jog step (mm)">
          {STEPS.map((mm) => {
            const active = mm === step;
            return (
              <button
                key={mm}
                className={`btn btn--sm seg__opt${active ? " is-active" : ""}`}
                role="radio"
                aria-checked={active}
                data-testid="jog-step"
                onClick={() => setStep(mm)}
              >
                {mm} mm
              </button>
            );
          })}
        </div>
      </div>

      {/* ── TEMPERATURE ──────────────────────────────────────────────────── */}
      <div className="msub">
        <div className="lbl">temperature</div>
        <div className="trow">
          <span className="lbl trow__name">nozzle</span>
          <span className="trow__read" data-testid="machine-nozzle-read">
            {fmt(s.nozzle_temper)}° → {fmt(s.nozzle_target)}°
          </span>
          <input
            className="pw trow__in"
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
        <div className="trow">
          <span className="lbl trow__name">bed</span>
          <span className="trow__read" data-testid="machine-bed-read">
            {fmt(s.bed_temper)}° → {fmt(s.bed_target)}°
          </span>
          <input
            className="pw trow__in"
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

      {/* ── FILAMENT (extrude) ───────────────────────────────────────────── */}
      <div className="msub">
        <div className="lbl">filament</div>
        <div className="seg" role="radiogroup" aria-label="extrude length (mm)">
          {EXTRUDE_LENS.map((mm) => {
            const active = mm === extLen;
            return (
              <button
                key={mm}
                className={`btn btn--sm seg__opt${active ? " is-active" : ""}`}
                role="radio"
                aria-checked={active}
                data-testid="extrude-length"
                onClick={() => setExtLen(mm)}
              >
                {mm} mm
              </button>
            );
          })}
        </div>
        <div className="btns">
          <button
            className="btn btn--sm"
            disabled={extrudeDisabled}
            title={extrudeTitle}
            data-testid="extrude-load"
            onClick={() => void control.extrude(extLen, FEED_EXTRUDE)}
          >
            load
          </button>
          <button
            className="btn btn--sm"
            disabled={extrudeDisabled}
            title={extrudeTitle}
            data-testid="extrude-unload"
            onClick={() => void control.extrude(-extLen, FEED_EXTRUDE)}
          >
            unload
          </button>
        </div>
      </div>

      {/* ── MAINTENANCE ──────────────────────────────────────────────────── */}
      <div className="msub">
        <div className="lbl">maintenance</div>
        <div className="calrow">
          <label className="startrow" title="auto bed leveling">
            <input
              type="checkbox"
              checked={cal.bed_level}
              disabled={busy}
              onChange={(e) => setCal((c) => ({ ...c, bed_level: e.target.checked }))}
            />
            <span className="lbl">bed level</span>
          </label>
          <label className="startrow" title="resonance/vibration compensation">
            <input
              type="checkbox"
              checked={cal.vibration}
              disabled={busy}
              onChange={(e) => setCal((c) => ({ ...c, vibration: e.target.checked }))}
            />
            <span className="lbl">vibration</span>
          </label>
          <label className="startrow" title="motor noise cancellation">
            <input
              type="checkbox"
              checked={cal.motor_noise}
              disabled={busy}
              onChange={(e) => setCal((c) => ({ ...c, motor_noise: e.target.checked }))}
            />
            <span className="lbl">motor noise</span>
          </label>
          <button
            className="btn btn--sm"
            disabled={!!b || busy || !anyCal}
            title={busy ? `unavailable while ${stateName}` : anyCal ? undefined : "select a routine"}
            data-testid="calibrate-run"
            onClick={() => control.calibrate(cal)}
          >
            calibrate
          </button>
        </div>
        <div className="btns">
          <button
            className="btn btn--sm"
            disabled={!!b}
            data-testid="ams-resume"
            onClick={() => void control.ams("resume", false)}
          >
            ams resume
          </button>
          <button
            className="btn btn--sm"
            disabled={!!b}
            data-testid="ams-reset"
            onClick={() => control.ams("reset", true)}
          >
            ams reset
          </button>
          <button
            className="btn btn--sm"
            disabled={!!b}
            data-testid="steppers"
            onClick={() => control.steppers()}
          >
            disable steppers
          </button>
          <button
            className="btn btn--sm btn--danger"
            disabled={!!b}
            data-testid="reboot"
            onClick={() => control.reboot()}
          >
            reboot
          </button>
        </div>
      </div>
    </div>
  );
}
