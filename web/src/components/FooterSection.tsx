import type { PrinterStatus } from "../types";

export function FooterSection({ s }: { s: PrinterStatus }) {
  const chips: Array<[string, string]> = [];
  // WiFi signal lives in the Overview band now; SD card in the Files panel header.
  if (s.nozzle_diameter) {
    chips.push([
      "nozzle",
      `${s.nozzle_diameter}${s.nozzle_type ? ` ${s.nozzle_type.replace(/_/g, " ")}` : ""}`,
    ]);
  }
  if (s.online?.rfid != null) chips.push(["rfid", s.online.rfid ? "online" : "offline"]);
  if (s.ipcam?.timelapse) chips.push(["timelapse", s.ipcam.timelapse]);
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
