import { useEffect, useState } from "react";

const REFRESH_MS = 1500; // snapshot poll cadence (the cam serves one JPEG per GET)

// A live view of an external IP camera the SERVER proxies (the LAN cam isn't
// reachable from the dashboard's browser). On mount we ask /api/camera whether a
// camera is configured; if so we render an <img> pointed at the proxied snapshot
// endpoint, cache-busted on a timer so it refreshes like a slow video. If no
// camera is configured the section renders nothing.
export function CameraSection() {
  const [available, setAvailable] = useState(false);
  const [ts, setTs] = useState(() => Date.now());

  // Probe once on mount: tolerate any failure as "no camera".
  useEffect(() => {
    let live = true;
    void (async () => {
      try {
        const r = await fetch("/api/camera");
        const d = (await r.json()) as { available?: boolean };
        if (live && r.ok && d.available) setAvailable(true);
      } catch {
        /* no camera */
      }
    })();
    return () => {
      live = false;
    };
  }, []);

  // Once available, tick the cache-busting query so the <img> re-fetches.
  useEffect(() => {
    if (!available) return;
    const id = setInterval(() => setTs(Date.now()), REFRESH_MS);
    return () => clearInterval(id);
  }, [available]);

  if (!available) return null;

  return (
    <section className="panel cam" data-testid="camera">
      <div className="lbl">camera</div>
      <img
        className="cam__view"
        alt="external camera"
        src={`/api/camera/snapshot?t=${ts}`}
        data-testid="camera-view"
      />
    </section>
  );
}
