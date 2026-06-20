// Serve-internal timelapse capture: the server grabs a frame from a configured
// camera on each new print layer (no second printer connection). Status is an
// open read; start/stop are gated writes (carry the control password).

/** One capture run — `smooth` (per-layer, park-synced) or `plain` (time-sampled). */
export interface RunState {
  running: boolean;
  mode: string;
  cameras: string[];
  /** First camera, for back-compat; prefer `cameras`. */
  camera: string | null;
  every: number;
  interval_ms: number | null;
  frames: number;
  failures: number;
  current_layer: number | null;
  out_dir: string | null;
  last_error: string | null;
}

/** The combined status: top-level mirrors the smooth run (back-compat) and
 *  `running` is true if any run is active; `smooth`/`plain`/`park` give each run. */
export interface TimelapseState extends RunState {
  smooth: RunState;
  plain: RunState;
  park: RunState;
}

export type TimelapseMode = "smooth" | "plain" | "park";

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

export function startTimelapse(
  mode: TimelapseMode,
  cameras: string[],
  opts: { every?: number; intervalMs?: number },
  password: string | null,
): Promise<Write> {
  const body: Record<string, unknown> = { mode, cameras };
  // park has no cadence knobs (the signal is in the camera stream); smooth/plain do.
  if (mode === "plain") body.interval_ms = opts.intervalMs ?? 3000;
  else if (mode === "smooth") body.every = opts.every ?? 1;
  return post("/api/timelapse/start", body, password);
}

export function stopTimelapse(
  mode: TimelapseMode | "all",
  password: string | null,
): Promise<Write> {
  return post("/api/timelapse/stop", { mode }, password);
}
