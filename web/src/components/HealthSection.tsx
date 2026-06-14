import type { PrinterStatus } from "../types";

export function HealthSection({ s }: { s: PrinterStatus }) {
  const hms = s.hms ?? [];
  if (hms.length === 0 && !s.error) return null;
  return (
    <section className="panel span-all sec--health" data-testid="health">
      <div className="secline">
        <span className="lbl alert">health</span>
      </div>
      {s.error && (
        <div className="arow sev-err">
          <span className="sev-tag sev-tag--err">error</span>
          <span className="acode">{s.error.hex}</span>
          <span className="chip">device error</span>
          <a href={s.error.lookup_url} target="_blank" rel="noreferrer">
            troubleshoot ↗
          </a>
        </div>
      )}
      {hms.map((a, i) => (
        <div key={i} className={`arow sev-${a.severity}`}>
          {/* CUD: severity is carried by the left-border colour AND this badge —
              the level number distinguishes sev-2 from sev-3, which share a hue. */}
          <span className="sev-tag" title={`severity level ${a.severity}`}>
            sev {a.severity}
          </span>
          <span className="acode">{a.code_hyphen}</span>
          <span className="chip">{a.module}</span>
          {a.is_lidar && <span className="chip">lidar</span>}
          <a href={a.wiki} target="_blank" rel="noreferrer">
            troubleshoot ↗
          </a>
        </div>
      ))}
    </section>
  );
}
