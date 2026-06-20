import type { PrinterStatus } from "../types";

export function FooterSection({ s }: { s: PrinterStatus }) {
  const chips: Array<[string, string]> = [];
  // The band carries WiFi + the nozzle spec; the AMS header carries RFID; the
  // Files panel header carries the SD card. The footer is just conditional notices
  // (extra lights, a pending firmware update) now.
  //
  // We deliberately DON'T surface `ipcam.timelapse` here: it's the printer's built-in
  // camera recording toggle (dead on this A1), and it collides with the dashboard's own
  // "record a timelapse" feature — "timelapse: disable" reads as "your recording is off".
  // The chamber light has its own toggle in the controls panel — don't repeat it
  // here. Other light nodes (e.g. a work light) still surface as status chips.
  for (const l of s.lights ?? []) {
    if (l.node === "chamber_light" || l.node === "chamber") continue;
    chips.push([l.node.replace(/_/g, " "), l.mode]);
  }
  if (s.upgrade?.new_version_state === 1) chips.push(["firmware", "update available"]);
  if (chips.length === 0) return null;
  return (
    <section className="panel span-all foot" data-testid="foot">
      {chips.map(([k, v]) => (
        <span className="fchip" key={k}>
          <span className="lbl">{k}</span>
          <span className="fchip__v">{v}</span>
        </span>
      ))}
    </section>
  );
}
