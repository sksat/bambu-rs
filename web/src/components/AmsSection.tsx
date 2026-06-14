import type { Ams } from "../types";
import { Humidity, Tray } from "./widgets";

export function AmsSection({ ams }: { ams: Ams }) {
  const units = ams.units ?? [];
  const active = ams.active_tray;
  const swapping = active != null && ams.target_tray != null && active !== ams.target_tray;
  return (
    <section className="panel">
      <div className="secline">
        <span className="lbl">ams</span>
        {swapping ? (
          <span className="swap" data-testid="ams-swap">
            swapping {active} → {ams.target_tray}
          </span>
        ) : (
          <span className="dim">{active && active !== "255" ? `tray ${active} loaded` : "idle"}</span>
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
              <Humidity level={u.humidity} />
              {u.temp ? <span className="dim"> {u.temp.toFixed(0)}°C</span> : null}
            </div>
          )}
        </div>
      ))}
    </section>
  );
}
