import type { Ams } from "../types";
import { amsActivity } from "../format";
import { Humidity, Tray } from "./widgets";

export function AmsSection({ ams, rfid }: { ams: Ams; rfid?: boolean | null }) {
  const units = ams.units ?? [];
  // Plain-words activity: "loading tray 3" / "unloading tray 3" / "tray 1 →
  // tray 3" / "tray 3 loaded" / "idle" — never a raw 254/255 sentinel.
  const { text, active } = amsActivity(ams.active_tray, ams.target_tray);
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
        {/* The RFID reader (reads spool tags) belongs with the AMS — the ✓/✕
            shape carries the state without relying on colour (CUD); offline also
            goes amber. Hidden only when the printer doesn't report it. */}
        {rfid != null && (
          <span
            className={`amslink${rfid ? "" : " amslink--warn"}`}
            data-testid="ams-rfid"
            title={`RFID reader ${rfid ? "online" : "offline"}`}
          >
            rfid {rfid ? "✓" : "✕"}
          </span>
        )}
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
