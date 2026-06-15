// Serve-internal timelapse capture: the server grabs a frame from a configured
// camera on each new print layer (no second printer connection). Status is an
// open read; start/stop are gated writes (carry the control password).

export interface TimelapseState {
  running: boolean;
  camera: string | null;
  every: number;
  frames: number;
  failures: number;
  current_layer: number | null;
  out_dir: string | null;
  last_error: string | null;
}

export async function getTimelapse(): Promise<TimelapseState | null> {
  try {
    const r = await fetch("/api/timelapse");
    if (!r.ok) return null;
    return (await r.json()) as TimelapseState;
  } catch {
    return null;
  }
}

type Write = { ok: true } | { error: string } | "needPassword";

async function post(url: string, body: unknown, password: string | null): Promise<Write> {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (password) headers["Authorization"] = `Bearer ${password}`;
  try {
    const r = await fetch(url, { method: "POST", headers, body: JSON.stringify(body) });
    if (r.status === 401) return "needPassword";
    if (!r.ok) {
      const d = (await r.json().catch(() => ({}))) as { error?: string };
      return { error: d.error ?? `HTTP ${r.status}` };
    }
    return { ok: true };
  } catch (e) {
    return { error: e instanceof Error ? e.message : "network error" };
  }
}

export function startTimelapse(camera: string, every: number, password: string | null): Promise<Write> {
  return post("/api/timelapse/start", { camera, every }, password);
}

export function stopTimelapse(password: string | null): Promise<Write> {
  return post("/api/timelapse/stop", {}, password);
}
