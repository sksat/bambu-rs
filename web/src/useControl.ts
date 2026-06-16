import { useEffect, useState } from "react";
import { sendControl } from "./control";

export interface Toast {
  kind: "ok" | "warn" | "err";
  msg: string;
}

/** A queued confirm-gated action: a human-readable message + the work to run
 *  once the operator confirms it in the shared ConfirmDialog. `label` names the
 *  affirmative button (e.g. "calibrate") so it says what it does, not "confirm". */
export interface ConfirmReq {
  message: string;
  run: () => void;
  label?: string;
}

export type Axis = "x" | "y" | "z";
export type TempPart = "nozzle" | "bed";
export type AmsAction = "resume" | "reset" | "pause";

export interface CalibrateOpts {
  bed_level: boolean;
  vibration: boolean;
  motor_noise: boolean;
}

/** Control state + actions: posts to the API, surfaces a toast, handles the
 *  stop confirm dialog, the generic machine confirm dialog, and the optional
 *  write password. */
export function useControl() {
  const [password, setPassword] = useState<string>(() => sessionStorage.getItem("bambu_pw") ?? "");
  const [needPassword, setNeedPassword] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<Toast | null>(null);
  const [confirmStop, setConfirmStop] = useState(false);
  // The pending machine confirm (calibrate / reboot / steppers / ams-reset …);
  // null when nothing awaits confirmation.
  const [confirm, setConfirm] = useState<ConfirmReq | null>(null);

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(t);
  }, [toast]);

  const act = async (label: string, path: string, body: Record<string, unknown> | null) => {
    setBusy(label);
    const res = await sendControl(path, body, password || null);
    setBusy(null);
    switch (res.kind) {
      case "ok":
        setToast({ kind: "ok", msg: `${label}: verified` });
        break;
      case "accepted":
        setToast({ kind: "warn", msg: `${label}: sent (unverified)` });
        break;
      case "rejected":
        setToast({ kind: "err", msg: `${label}: rejected — ${res.reason}` });
        break;
      case "needPassword":
        setNeedPassword(true);
        setToast({ kind: "err", msg: "control needs a password" });
        break;
      case "error":
        setToast({ kind: "err", msg: `${label}: ${res.message}` });
        break;
    }
  };

  // Queue a confirm-gated action: shows the shared dialog, then runs `work` on
  // confirm. Mirrors the requestStop/confirmAndStop flow for arbitrary writes.
  const requestConfirm = (message: string, work: () => void, label?: string) =>
    setConfirm({ message, run: work, label });

  return {
    password,
    needPassword,
    busy,
    toast,
    confirmStop,
    confirm,
    setPassword: (pw: string) => {
      setPassword(pw);
      sessionStorage.setItem("bambu_pw", pw);
    },
    pause: () => act("pause", "/api/job/pause", { confirm: true }),
    resume: () => act("resume", "/api/job/resume", { confirm: true }),
    requestStop: () => setConfirmStop(true),
    cancelStop: () => setConfirmStop(false),
    confirmAndStop: () => {
      setConfirmStop(false);
      void act("stop", "/api/job/stop", { confirm: true });
    },
    light: (on: boolean) => act(`light ${on ? "on" : "off"}`, "/api/light", { node: "chamber", on }),
    speed: (level: string) => act(`speed ${level}`, "/api/speed", { level }),
    gcode: (line: string, force: boolean) =>
      act(`gcode ${line}`, "/api/gcode", { line, confirm: true, force }),

    // ── machine control ──────────────────────────────────────────────────────
    // Generic confirm dialog plumbing (shared by the confirm-gated writes).
    cancelConfirm: () => setConfirm(null),
    runConfirm: () => {
      const c = confirm;
      setConfirm(null);
      c?.run();
    },

    home: (axes: "all" | Axis) => act(`home ${axes}`, "/api/home", { axes }),
    jog: (axis: Axis, delta: number, feedrate: number) =>
      act(
        `move ${axis.toUpperCase()} ${delta > 0 ? "+" : ""}${delta}`,
        "/api/move",
        { axis, delta, feedrate },
      ),
    extrude: (delta: number, feedrate: number) =>
      act(`${delta < 0 ? "retract" : "extrude"} ${Math.abs(delta)}`, "/api/extrude", {
        delta,
        feedrate,
      }),
    // celsius > 0 needs confirm; the caller passes confirm=true once acknowledged.
    setTemp: (part: TempPart, celsius: number, confirm: boolean, force = false) =>
      act(`${part} ${celsius}°`, "/api/temp", { part, celsius, confirm, force }),
    cooldown: (part: TempPart) =>
      act(`${part} cool`, "/api/temp", { part, celsius: 0, confirm: false, force: false }),
    calibrate: (opts: CalibrateOpts) => {
      const picked = (
        [
          ["bed level", opts.bed_level],
          ["vibration", opts.vibration],
          ["motor noise", opts.motor_noise],
        ] as Array<[string, boolean]>
      )
        .filter(([, on]) => on)
        .map(([n]) => n)
        .join(", ");
      requestConfirm(
        `Run calibration (${picked})? The printer will move on its own.`,
        () => {
          void act("calibrate", "/api/calibrate", { ...opts, confirm: true });
        },
        "calibrate",
      );
    },
    ams: (action: AmsAction, confirm: boolean) => {
      if (!confirm) return act(`ams ${action}`, "/api/ams", { action, confirm: false });
      requestConfirm(
        `AMS ${action}? This interrupts the current spool state.`,
        () => {
          void act(`ams ${action}`, "/api/ams", { action, confirm: true });
        },
        action,
      );
    },
    // AMS-coordinated filament change (full path back to the spool), unlike the
    // extruder-only `extrude` retract. target 255 = unload, 254 = external spool,
    // 0..3 = AMS tray. Always confirm-gated — it physically moves filament.
    amsChange: (target: number, tarTemp: number) => {
      const what =
        target === 255
          ? "Unload the filament"
          : target === 254
            ? "Load the external spool"
            : `Change to tray ${target}`;
      const label = target === 255 ? "unload" : "change";
      requestConfirm(
        `${what} at ${tarTemp}°C? The AMS will move filament along the whole path.`,
        () => {
          void act(`ams ${label}`, "/api/ams/change", {
            target,
            tar_temp: tarTemp,
            confirm: true,
          });
        },
        label,
      );
    },
    reboot: () =>
      requestConfirm(
        "Reboot the printer? It will disconnect while it restarts.",
        () => {
          void act("reboot", "/api/reboot", { confirm: true });
        },
        "reboot",
      ),
    steppers: () =>
      requestConfirm(
        "Disable the steppers? The axes will be free to move by hand.",
        () => {
          void act("disable steppers", "/api/steppers", { confirm: true });
        },
        "disable steppers",
      ),
  };
}

export type Control = ReturnType<typeof useControl>;
