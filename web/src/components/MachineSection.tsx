import { useState } from "react";
import type { Control, Axis } from "../useControl";
import type { PrinterStatus } from "../types";
import { WifiSignal } from "./widgets";

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

// Z jog rungs: magnitude grows away from the centre datum (.1 nearest, 10 at the
// ends), mirroring the dial's rings (inner = fine, outer = coarse). Tapping a rung
// jogs Z by that signed step — magnitude is the position, just like the dial.
const Z_RUNGS = [
  { step: 10, label: "10", ri: 2 },
  { step: 1, label: "1", ri: 1 },
  { step: 0.1, label: ".1", ri: 0 },
] as const;
const EXTRUDE_LENS = [1, 5, 10] as const;

// ── Concentric XY jog dial ──────────────────────────────────────────────────
// A polar control: four 90° wedges (Y+ top, X+ right, Y- bottom, X- left), each
// split into three concentric rings = step (0.1 / 1 / 10 mm). Tapping a wedge's ring
// jogs that axis by that step; the centre is home. Replaces the D-pad + the separate
// step selector — direction is the wedge, magnitude is the ring.
const JOG_RINGS = [
  { step: 0.1, label: ".1", r0: 11.5, r1: 20 },
  { step: 1, label: "1", r0: 20, r1: 32 },
  { step: 10, label: "10", r0: 32, r1: 44.5 },
] as const;
const JOG_DIRS = [
  { axis: "y" as Axis, dir: 1, label: "Y+", key: "yplus", ang: -90 },
  { axis: "x" as Axis, dir: 1, label: "X+", key: "xplus", ang: 0 },
  { axis: "y" as Axis, dir: -1, label: "Y−", key: "yminus", ang: 90 },
  { axis: "x" as Axis, dir: -1, label: "X−", key: "xminus", ang: 180 },
] as const;

// Polar → cartesian about the (50,50) viewBox centre.
function polar(r: number, deg: number): [number, number] {
  const a = (deg * Math.PI) / 180;
  return [50 + r * Math.cos(a), 50 + r * Math.sin(a)];
}
// SVG path for an annular sector (ring band r0..r1 between angles a0..a1, <180°).
function ringSector(r0: number, r1: number, a0: number, a1: number): string {
  const [x0o, y0o] = polar(r1, a0);
  const [x1o, y1o] = polar(r1, a1);
  const [x1i, y1i] = polar(r0, a1);
  const [x0i, y0i] = polar(r0, a0);
  return `M ${x0o} ${y0o} A ${r1} ${r1} 0 0 1 ${x1o} ${y1o} L ${x1i} ${y1i} A ${r0} ${r0} 0 0 0 ${x0i} ${y0i} Z`;
}

function JogDial({
  onJog,
  onHome,
  disabled,
  title,
}: {
  onJog: (axis: Axis, mm: number) => void;
  onHome: () => void;
  disabled: boolean;
  title?: string;
}) {
  const HALF = 44; // wedge half-angle; a hair under 45° leaves a thin seam between wedges
  return (
    <svg
      className={disabled ? "dial dial--off" : "dial"}
      viewBox="0 0 100 100"
      role="group"
      aria-label="XY jog"
      aria-disabled={disabled}
      data-testid="jog-dial"
    >
      {title && <title>{title}</title>}
      {JOG_DIRS.flatMap((d) =>
        JOG_RINGS.map((ring, ri) => {
          const [tx, ty] = polar((ring.r0 + ring.r1) / 2, d.ang);
          return (
            <g key={`${d.key}-${ring.label}`} className={`dial__seg dial__seg--r${ri}`}>
              <path
                d={ringSector(ring.r0, ring.r1, d.ang - HALF, d.ang + HALF)}
                data-testid={`jog-${d.key}-${ring.label}`}
                aria-label={`${d.label} ${ring.step} mm`}
                onClick={() => onJog(d.axis, d.dir * ring.step)}
              />
              <text className="dial__num" x={tx} y={ty}>
                {ring.label}
              </text>
            </g>
          );
        }),
      )}
      {JOG_DIRS.map((d) => {
        const [lx, ly] = polar(47, d.ang);
        return (
          <text key={d.key} className="dial__axis" x={lx} y={ly}>
            {d.label}
          </text>
        );
      })}
      <circle
        className="dial__home"
        cx="50"
        cy="50"
        r="11"
        data-testid="home-all"
        aria-label="home all axes (G28)"
        onClick={() => onHome()}
      />
      <text className="dial__home-glyph" x="50" y="50">
        ⌂
      </text>
    </svg>
  );
}

// Z jog: a vertical ladder echoing the dial. Z+ rungs stack above the centre datum,
// Z- below; magnitude grows outward (coarse 10 mm at the ends), so the same "further
// out = bigger move" reading carries over from the XY dial — no separate step picker.
function JogZStack({
  onJog,
  disabled,
  title,
}: {
  onJog: (mm: number) => void;
  disabled: boolean;
  title?: string;
}) {
  const rung = (sign: 1 | -1, z: (typeof Z_RUNGS)[number]) => (
    <button
      key={`${sign > 0 ? "zplus" : "zminus"}-${z.label}`}
      className={`zrung zrung--r${z.ri}`}
      disabled={disabled}
      title={title}
      data-testid={`jog-${sign > 0 ? "zplus" : "zminus"}-${z.label}`}
      aria-label={`Z${sign > 0 ? "+" : "−"} ${z.step} mm`}
      onClick={() => onJog(sign * z.step)}
    >
      <span className="zrung__dir">{sign > 0 ? "↑" : "↓"}</span>
      <span className="zrung__mag">{z.label}</span>
    </button>
  );
  return (
    <div className="zstack" role="group" aria-label="Z jog" data-testid="jog-zstack">
      {/* Z+ : coarse (10) at the top, fine (.1) nearest the datum. */}
      {Z_RUNGS.map((z) => rung(1, z))}
      <span className="zstack__datum lbl">Z</span>
      {/* Z- : fine (.1) nearest the datum, coarse (10) at the bottom (Z_RUNGS reversed). */}
      {[...Z_RUNGS].reverse().map((z) => rung(-1, z))}
    </div>
  );
}

export function MachineSection({ s, control }: { s: PrinterStatus; control: Control }) {
  const b = control.busy;
  const busy = isBusy(s.gcode_state);
  const stateName = s.gcode_state ?? "unknown";
  // Both jog controls encode magnitude in position: the dial's rings (XY) and the
  // Z ladder's rungs — so there's no separate step state to track.
  const [extLen, setExtLen] = useState<number>(5);
  const [cal, setCal] = useState({ bed_level: false, vibration: false, motor_noise: false });

  // A jog/home is unavailable if a write is in flight OR the printer is busy.
  const motionDisabled = !!b || busy;
  const motionTitle = busy ? `unavailable while ${stateName}` : undefined;

  // Temp the AMS unload heats to: honour an explicit nozzle setpoint, else a
  // default that softens both PLA and PETG enough to retract cleanly.
  const amsUnloadTemp = s.nozzle_target && s.nozzle_target > 0 ? s.nozzle_target : 240;
  const nozzleCold = (s.nozzle_temper ?? 0) < MIN_EXTRUDE_TEMP;
  const extrudeDisabled = !!b || busy || nozzleCold;
  const extrudeTitle = busy
    ? `unavailable while ${stateName}`
    : nozzleCold
      ? "nozzle must be ≥170°C"
      : undefined;

  const anyCal = cal.bed_level || cal.vibration || cal.motor_noise;

  return (
    <div className="cfold" data-testid="machine">
      {/* ── MOVE: concentric XY jog dial + Z column ─────────────────────────── */}
      <div className="msub">
        <div className="lbl">move</div>
        <div className="jog">
          {/* XY: a radial dial — direction is the wedge, step (0.1/1/10mm) is the ring. */}
          <JogDial
            onJog={(axis, mm) => void control.jog(axis, mm, FEED_XY)}
            onHome={() => void control.home("all")}
            disabled={motionDisabled}
            title={motionTitle}
          />
          {/* Z ladder — the gantry, beside the bed; magnitude grows outward like the dial. */}
          <JogZStack
            onJog={(mm) => void control.jog("z", mm, FEED_Z)}
            disabled={motionDisabled}
            title={motionTitle}
          />
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
        {/* AMS-coordinated unload: pulls filament the whole way back to the spool
            (the extruder-only retract above just nudges it at the nozzle). The
            firmware heats to the target itself, so it works from a cold nozzle —
            unlike the extrude buttons, only the busy gate applies. */}
        <div className="btns">
          <button
            className="btn btn--sm"
            disabled={!!b || busy}
            title={busy ? `unavailable while ${stateName}` : "AMS unload (whole path)"}
            data-testid="ams-unload"
            onClick={() => control.amsChange(255, amsUnloadTemp)}
          >
            AMS unload
          </button>
        </div>
      </div>

      {/* ── MAINTENANCE ──────────────────────────────────────────────────── */}
      <div className="msub">
        <div className="lbl">maintenance</div>
        {/* Hardware/connectivity status you check during upkeep: the installed nozzle
            spec and the WiFi signal. Each renders only when its data is present. */}
        {(s.nozzle_diameter || s.wifi_signal) && (
          <div className="mstat" data-testid="machine-hw">
            {s.nozzle_diameter && (
              <span className="jobspec" data-testid="nozzle-spec" title="installed nozzle">
                Ø{s.nozzle_diameter}
                {s.nozzle_type ? ` ${s.nozzle_type.replace(/_/g, " ")}` : ""}
              </span>
            )}
            <WifiSignal signal={s.wifi_signal} />
          </div>
        )}
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
