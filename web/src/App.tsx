import { useEffect, useState } from "react";
import type { ReactNode } from "react";
import type { PrinterStatus } from "./types";
import "./app.css";

type Conn = "connecting" | "live" | "offline";

// Live dashboard: subscribe to /api/ws (P1) and render a mobile-first set of
// cards. The real shadcn/uPlot chart UI lands in P4; this is the responsive
// baseline that already works end-to-end on a phone over Tailscale.
export function App() {
  const [status, setStatus] = useState<PrinterStatus | null>(null);
  const [conn, setConn] = useState<Conn>("connecting");
  const token = new URLSearchParams(location.search).get("token") ?? "";

  useEffect(() => {
    let ws: WebSocket | null = null;
    let retry: ReturnType<typeof setTimeout> | undefined;
    let closed = false;

    const connect = () => {
      const proto = location.protocol === "https:" ? "wss" : "ws";
      ws = new WebSocket(`${proto}://${location.host}/api/ws?token=${encodeURIComponent(token)}`);
      ws.onopen = () => setConn("live");
      ws.onmessage = (ev) => {
        try {
          setStatus(JSON.parse(ev.data as string) as PrinterStatus);
        } catch {
          /* ignore malformed frame */
        }
      };
      ws.onclose = () => {
        if (closed) return;
        setConn("offline");
        retry = setTimeout(connect, 2000); // reconnect with a fixed backoff
      };
      ws.onerror = () => ws?.close();
    };
    connect();

    return () => {
      closed = true;
      if (retry) clearTimeout(retry);
      ws?.close();
    };
  }, [token]);

  return (
    <div className="app">
      <header className="topbar">
        <h1>bambu dashboard</h1>
        <span className={`conn conn--${conn}`} data-testid="conn">
          <span className="dot" />
          {conn}
        </span>
      </header>
      <Cards status={status} />
      {!token && (
        <p className="hint" data-testid="no-token">
          No <code>?token=</code> in the URL — the API will reject the connection.
        </p>
      )}
    </div>
  );
}

function Cards({ status }: { status: PrinterStatus | null }) {
  if (!status) {
    return (
      <p className="hint" data-testid="waiting">
        waiting for the first status frame…
      </p>
    );
  }
  const state = status.gcode_state ?? "?";
  const pct = status.mc_percent ?? 0;
  const printing = state === "RUNNING" || state === "PAUSE";
  return (
    <main className="grid">
      <Card label="state" wide>
        <span className={`state state--${state.toLowerCase()}`} data-testid="state">
          {state}
        </span>
        {status.subtask_name ? <div className="sub">{status.subtask_name}</div> : null}
        {status.print_error ? <div className="err">error 0x{status.print_error.toString(16)}</div> : null}
      </Card>

      <Card label="progress" wide>
        <div className="bar" role="progressbar" aria-valuenow={pct}>
          <div className="bar__fill" style={{ width: `${pct}%` }} data-testid="progress-fill" />
        </div>
        <div className="row">
          <strong data-testid="percent">{pct}%</strong>
          {status.layer_num != null && (
            <span>
              layer {status.layer_num}
              {status.total_layer_num ? ` / ${status.total_layer_num}` : ""}
            </span>
          )}
          {printing && status.remaining_time_min != null && <span>{fmtEta(status.remaining_time_min)} left</span>}
        </div>
      </Card>

      <Temp label="nozzle" cur={status.nozzle_temper} target={status.nozzle_target} testid="nozzle" />
      <Temp label="bed" cur={status.bed_temper} target={status.bed_target} testid="bed" />
    </main>
  );
}

function Card({ label, wide, children }: { label: string; wide?: boolean; children: ReactNode }) {
  return (
    <section className={`card${wide ? " card--wide" : ""}`}>
      <div className="card__label">{label}</div>
      {children}
    </section>
  );
}

function Temp({
  label,
  cur,
  target,
  testid,
}: {
  label: string;
  cur?: number | null;
  target?: number | null;
  testid: string;
}) {
  return (
    <Card label={label}>
      <div className="temp" data-testid={`${testid}-temp`}>
        <span className="temp__cur">{cur != null ? Math.round(cur) : "—"}</span>
        <span className="temp__unit">°C</span>
      </div>
      <div className="temp__target">{target ? `→ ${Math.round(target)}°C` : "off"}</div>
    </Card>
  );
}

function fmtEta(min: number): string {
  if (min < 60) return `${min}m`;
  return `${Math.floor(min / 60)}h ${min % 60}m`;
}
