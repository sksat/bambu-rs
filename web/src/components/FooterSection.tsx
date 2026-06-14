import type { PrinterStatus } from "../types";

export function FooterSection({ s }: { s: PrinterStatus }) {
  const chips: Array<[string, string]> = [];
  if (s.wifi_signal) chips.push(["wifi", s.wifi_signal]);
  // SD card status lives in the Files panel header now.
  if (s.nozzle_diameter) {
    chips.push([
      "nozzle",
      `${s.nozzle_diameter}${s.nozzle_type ? ` ${s.nozzle_type.replace(/_/g, " ")}` : ""}`,
    ]);
  }
  if (s.online?.rfid != null) chips.push(["rfid", s.online.rfid ? "online" : "offline"]);
  if (s.ipcam?.timelapse) chips.push(["timelapse", s.ipcam.timelapse]);
  for (const l of s.lights ?? []) chips.push([l.node.replace(/_/g, " "), l.mode]);
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
