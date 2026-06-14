import { useEffect, useState } from "react";

// Minimal P0 view: fetch the token-gated status once and render it. The real
// UI (shadcn cards, live uPlot charts over WebSocket, camera) lands in P4.
interface PrinterStatus {
  gcode_state: string | null;
  [key: string]: unknown;
}

export function App() {
  const [status, setStatus] = useState<PrinterStatus | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    // P0 dev/E2E: the bearer token comes from ?token=. (P4 will exchange it for a
    // cookie so it isn't in the URL.)
    const token = new URLSearchParams(location.search).get("token") ?? "";
    fetch("/api/status", { headers: { Authorization: `Bearer ${token}` } })
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((s: PrinterStatus) => setStatus(s))
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  return (
    <main style={{ fontFamily: "system-ui, sans-serif", padding: "2rem", maxWidth: 720 }}>
      <h1>bambu dashboard</h1>
      {error && (
        <p data-testid="error" style={{ color: "crimson" }}>
          error: {error}
        </p>
      )}
      {status && (
        <p data-testid="state">
          state: <strong>{status.gcode_state ?? "?"}</strong>
        </p>
      )}
      {status && (
        <pre data-testid="status-json" style={{ background: "#f4f4f5", padding: "1rem", borderRadius: 8 }}>
          {JSON.stringify(status, null, 2)}
        </pre>
      )}
    </main>
  );
}
