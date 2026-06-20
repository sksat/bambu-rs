import type { Ams } from "../types";
import { amsActivity, amsRfidReads } from "../format";
import { Humidity, Tray } from "./widgets";

export function AmsSection({ ams }: { ams: Ams }) {
  const units = ams.units ?? [];
  // Plain-words activity: "loading tray 3" / "unloading tray 3" / "tray 1 →
  // tray 3" / "tray 3 loaded" / "idle" — never a raw 254/255 sentinel.
  const { text, active } = amsActivity(ams.active_tray, ams.target_tray);
  // RFID reader state from real reads, NOT the A1's bogus `online.rfid` (see amsRfidReads).
  const rfidReads = amsRfidReads(ams);
  return (
    <div className="cfold" data-testid="ams">
      <div className="secline">
        <span className="lbl">ams</span>
        {active ? (
          <span className="swap" data-testid="ams-swap">
            {text}
          </span>
        ) : (
          <span className="dim">{text}</span>
        )}
        {/* The RFID reader (reads spool tags) belongs with the AMS — the ✓/✕ shape carries
            the state without relying on colour (CUD). Driven by real reads (see
            amsRfidReads): ✓ = at least one spool's tag was read (reader provably works);
            ✕ = none read. We deliberately DON'T warn (amber) on ✕ — on the A1 that was a
            false alarm, and zero reads can't be told apart from generic/empty spools. */}
        <span
          className="amslink"
          data-testid="ams-rfid"
          title={
            rfidReads > 0
              ? `RFID: ${rfidReads} spool${rfidReads === 1 ? "" : "s"} identified`
              : "no RFID-tagged spools detected"
          }
        >
          rfid {rfidReads > 0 ? "✓" : "✕"}
        </span>
      </div>
      {units.map((u) => (
        <div key={u.id} className="amsunit">
          <div className="rack">
            {(u.trays ?? []).map((t) => (
              <Tray key={t.id} t={t} />
            ))}
            {ams.external && (
              <>
                <span className="rack__sep" aria-hidden />
                <Tray t={ams.external} ext />
              </>
            )}
          </div>
          {u.humidity != null && (
            <div className="amsmeta">
              <span className="lbl">humidity</span>
              <Humidity level={u.humidity} raw={u.humidity_raw} />
              {u.temp ? <span className="dim"> {u.temp.toFixed(0)}°C</span> : null}
            </div>
          )}
        </div>
      ))}
    </div>
  );
}
