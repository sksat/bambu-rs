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

// The list of currently-available cameras (open read); tolerate failure as none.
export async function listCameras(): Promise<Camera[]> {
  try {
    const r = await fetch("/api/cameras");
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
    const r = await fetch("/api/cameras/config", { headers });
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
    const r = await fetch("/api/cameras/config", {
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
