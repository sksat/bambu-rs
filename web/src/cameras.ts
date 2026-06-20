// Camera API client. The server lists cameras (built-in + proxied externals) and
// serves one JPEG per GET per camera id; external cameras are editable at runtime
// through the gated config endpoint (reads are open, writes carry the password).

export interface Camera {
  id: string;
  kind: "internal" | "external";
  label: string;
  // True when the server can proxy a live MJPEG stream for this camera, so the
  // view uses `/stream` (continuous video) instead of polling `/snapshot`.
  stream?: boolean;
  // True when this camera can run the live park preview: it has both a stream and
  // a calibrated park_tuning, so the view offers a park toggle.
  park?: boolean;
}

// Per-camera live-park detection tuning (the 11 knobs; no defaults). Edited as raw
// JSON in the manage form and validated server-side.
export type ParkTuning = Record<string, number>;

export interface ExternalCfg {
  id: string;
  label: string;
  url: string;
  // Optional live MJPEG stream URL (null/absent = snapshot-only).
  stream_url?: string | null;
  // Optional per-camera park tuning (null/absent = no live park preview).
  park_tuning?: ParkTuning | null;
}

// One captured park frame, for the player's scrubber: its index `n`, the relative
// capture time `t` (seconds into the run), and the detector's confidence.
export interface ParkFrame {
  n: number;
  t: number | null;
  confidence: number | null;
}

// A camera's park filmstrip (open read): whether the run is still live, the frame
// count, and per-frame metadata. Available while a run is active AND after it stops
// (until the next run), so the player can review the whole strip.
export interface ParkIndex {
  running: boolean;
  count: number;
  parks: ParkFrame[];
}

// Fetch a camera's park filmstrip index (`/park` — the individual frames are `/park/{n}`).
// 404 (no run for this id) and any error read as an empty, not-running strip — the player
// then shows the "no frames yet" state.
export async function listParks(id: string): Promise<ParkIndex> {
  try {
    const r = await fetch(`/api/camera/${encodeURIComponent(id)}/park`);
    if (!r.ok) return { running: false, count: 0, parks: [] };
    return (await r.json()) as ParkIndex;
  } catch {
    return { running: false, count: 0, parks: [] };
  }
}

// A finished/in-progress capture run on disk, for the recordings list.
export type CaptureKind = "park" | "smooth" | "video";
export interface CaptureCam {
  id: string;
  kind: CaptureKind;
  frames: number;
  has_mp4: boolean;
}
export interface CaptureRun {
  id: string;
  started_at: number; // unix epoch (0 if unknown)
  label: string;
  cameras: CaptureCam[];
}

// List recorded capture runs (open read), newest first. Tolerate failure as none. Each
// camera's mp4 is at `/api/capture/<run>/<cam>/video.mp4` (assembled on demand).
export async function listCaptures(): Promise<CaptureRun[]> {
  try {
    const r = await fetch("/api/capture");
    if (!r.ok) return [];
    return ((await r.json()) as { captures?: CaptureRun[] }).captures ?? [];
  } catch {
    return [];
  }
}

// The list of currently-available cameras (open read); tolerate failure as none.
export async function listCameras(): Promise<Camera[]> {
  try {
    const r = await fetch("/api/camera");
    if (!r.ok) return [];
    const d = (await r.json()) as { cameras?: Camera[] };
    return d.cameras ?? [];
  } catch {
    return [];
  }
}

// The external-camera config WITH urls, for the manage form (gated read).
export async function getCamerasConfig(
  password: string | null,
): Promise<ExternalCfg[] | "needPassword" | "error"> {
  const headers: Record<string, string> = {};
  if (password) headers["Authorization"] = `Bearer ${password}`;
  try {
    const r = await fetch("/api/camera/config", { headers });
    if (r.status === 401) return "needPassword";
    if (!r.ok) return "error";
    const d = (await r.json()) as { external?: ExternalCfg[] };
    return d.external ?? [];
  } catch {
    return "error";
  }
}

// Replace the external-camera list (gated write).
export async function setCamerasConfig(
  external: {
    label?: string;
    url: string;
    stream_url?: string;
    park_tuning?: ParkTuning;
  }[],
  password: string | null,
): Promise<{ ok: true } | { error: string } | "needPassword"> {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  if (password) headers["Authorization"] = `Bearer ${password}`;
  try {
    const r = await fetch("/api/camera/config", {
      method: "POST",
      headers,
      body: JSON.stringify({ external }),
    });
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
