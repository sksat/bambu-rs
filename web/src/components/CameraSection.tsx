import { useEffect, useRef, useState } from "react";
import {
  listCameras,
  getCamerasConfig,
  setCamerasConfig,
  type Camera,
} from "../cameras";
import {
  getTimelapse,
  startTimelapse,
  stopTimelapse,
  type TimelapseState,
} from "../timelapse";

// Refresh cadence (the delay AFTER a frame settles before fetching the next). We
// drive the loop off the <img>'s load/error rather than a fixed timer so a slow
// source can never be re-requested before the current grab finishes — important
// for the A1 built-in cam, whose grab can take seconds to fail; a fixed timer
// would cancel-and-retry faster than it errors, pile up server-side blocking
// grabs, and never even notice it's offline.
const FAST_MS = 800;
const SLOW_MS = 5000;

// A live view of ONE camera by id. Two modes:
//   - stream: a single long-lived <img> on the MJPEG `/stream` endpoint — the
//     browser renders frames continuously, so there's NO re-fetch loop; on error
//     we bump the cache-buster to reconnect after a back-off.
//   - snapshot: poll one JPEG per cycle, driven off the <img> load/error so a
//     slow source (e.g. the A1 built-in cam taking seconds to fail) is never
//     re-requested before the current grab finishes.
// A configured-but-not-streaming source shows an "offline" message. Re-mount it
// (key by id) when the active tab changes to reset cleanly.
function CameraView({ id, label, stream }: { id: string; label: string; stream?: boolean }) {
  const [ts, setTs] = useState(() => Date.now());
  const [offline, setOffline] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => () => clearTimeout(timer.current), []);

  const scheduleNext = (delay: number) => {
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setTs(Date.now()), delay);
  };

  const kind = stream ? "stream" : "snapshot";

  return (
    <div className="cam__frame">
      <img
        className={offline ? "cam__view cam__view--off" : "cam__view"}
        alt={label}
        src={`/api/cameras/${id}/${kind}?t=${ts}`}
        data-testid="camera-view"
        data-mode={kind}
        onLoad={() => {
          if (offline) setOffline(false);
          // Stream is continuous — don't re-request it; only snapshots poll.
          if (!stream) scheduleNext(FAST_MS);
        }}
        onError={() => {
          if (!offline) setOffline(true);
          scheduleNext(SLOW_MS); // reconnect (stream) / retry (snapshot)
        }}
      />
      {offline && (
        <div className="cam__msg" data-testid="camera-offline">
          no frame — camera offline
        </div>
      )}
    </div>
  );
}

// Start/stop the serve-internal per-layer timelapse, capturing from whichever
// camera tab is active. Polls `/api/timelapse` so the button + frame count
// reflect a capture started from any tab (or that auto-stopped at print end).
function TimelapseBar({ activeCamera, password }: { activeCamera: string; password: string | null }) {
  const [tl, setTl] = useState<TimelapseState | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    const poll = async () => {
      const s = await getTimelapse();
      if (live) setTl(s);
    };
    void poll();
    const id = setInterval(() => void poll(), 2000);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, []);

  const running = tl?.running ?? false;

  const start = async () => {
    setBusy(true);
    setMsg(null);
    const r = await startTimelapse(activeCamera, 1, password);
    if (r === "needPassword") setMsg("needs the control password (set it in Controls)");
    else if ("error" in r) setMsg(r.error);
    else setTl(await getTimelapse());
    setBusy(false);
  };
  const stop = async () => {
    setBusy(true);
    setMsg(null);
    await stopTimelapse(password);
    setTl(await getTimelapse());
    setBusy(false);
  };

  return (
    <div className="cam__tl" data-testid="timelapse-bar">
      {running ? (
        <>
          <span className="cam__tl-rec" data-testid="timelapse-running">
            ● recording — {tl?.frames ?? 0} frames
            {tl?.current_layer != null ? ` · layer ${tl.current_layer}` : ""}
            {tl?.failures ? ` · ${tl.failures} failed` : ""}
          </span>
          <button
            className="cam__manage"
            data-testid="timelapse-stop"
            disabled={busy}
            onClick={() => void stop()}
          >
            stop
          </button>
        </>
      ) : (
        <>
          <span className="cam__tl-idle dim">timelapse → {activeCamera}</span>
          <button
            className="cam__manage"
            data-testid="timelapse-start"
            disabled={busy}
            title="capture one frame per print layer from this camera"
            onClick={() => void start()}
          >
            ● start timelapse
          </button>
        </>
      )}
      {msg && (
        <span className="cam__tl-msg dim" data-testid="timelapse-msg">
          {msg}
        </span>
      )}
    </div>
  );
}

// The cameras panel: a tab per available camera (built-in + each external) with
// the active one shown live. Editing the external list happens in a floating
// modal so opening it never resizes or hides the live view.
export function CamerasSection({ password }: { password: string | null }) {
  const [cameras, setCameras] = useState<Camera[]>([]);
  const [active, setActive] = useState<string>("");
  const [managing, setManaging] = useState(false);

  const reload = async () => {
    const cams = await listCameras();
    setCameras(cams);
    setActive((a) => (cams.some((c) => c.id === a) ? a : (cams[0]?.id ?? "")));
  };

  useEffect(() => {
    void reload();
  }, []);

  const activeCam = cameras.find((c) => c.id === active);

  return (
    <>
      <section className="panel cam" data-testid="cameras">
        <div className="cam__head">
          <span className="lbl">cameras</span>
          <button
            className="cam__manage"
            data-testid="cameras-manage"
            onClick={() => setManaging(true)}
          >
            manage
          </button>
        </div>

        {cameras.length === 0 ? (
          <div className="cam__empty" data-testid="cameras-empty">
            no cameras configured
          </div>
        ) : (
          <>
            {cameras.length > 1 && (
              <div className="cam__tabs" role="tablist">
                {cameras.map((c) => (
                  <button
                    key={c.id}
                    role="tab"
                    aria-selected={c.id === active}
                    className={c.id === active ? "cam__tab cam__tab--on" : "cam__tab"}
                    data-testid={`camera-tab-${c.id}`}
                    onClick={() => setActive(c.id)}
                  >
                    {c.label}
                  </button>
                ))}
              </div>
            )}
            {activeCam && (
              <CameraView
                key={activeCam.id}
                id={activeCam.id}
                label={activeCam.label}
                stream={activeCam.stream}
              />
            )}
            {activeCam && <TimelapseBar activeCamera={activeCam.id} password={password} />}
          </>
        )}
      </section>

      {managing && (
        <CameraManageModal
          password={password}
          onClose={() => setManaging(false)}
          onSaved={async () => {
            setManaging(false);
            await reload();
          }}
        />
      )}
    </>
  );
}

interface Row {
  label: string;
  url: string;
  stream_url: string;
}

// The manage dialog: a floating modal for editing the external-camera list (the
// built-in camera isn't configurable). Mirrors the dashboard's existing pattern —
// on a 401 it points the operator at the Controls password rather than prompting.
function CameraManageModal({
  password,
  onClose,
  onSaved,
}: {
  password: string | null;
  onClose: () => void;
  onSaved: () => Promise<void>;
}) {
  const [rows, setRows] = useState<Row[]>([]);
  const [status, setStatus] = useState<string>("");

  useEffect(() => {
    void (async () => {
      const cfg = await getCamerasConfig(password);
      if (cfg === "needPassword") setStatus("needs the control password (set it in Controls)");
      else if (cfg === "error") setStatus("couldn't load camera config");
      else setRows(cfg.map((e) => ({ label: e.label, url: e.url, stream_url: e.stream_url ?? "" })));
    })();
  }, [password]);

  const save = async () => {
    const external = rows
      .map((r) => ({
        label: r.label.trim() || undefined,
        url: r.url.trim(),
        stream_url: r.stream_url.trim() || undefined,
      }))
      .filter((r) => r.url);
    setStatus("saving…");
    const res = await setCamerasConfig(external, password);
    if (res === "needPassword") setStatus("needs the control password (set it in Controls)");
    else if ("error" in res) setStatus(`save failed: ${res.error}`);
    else await onSaved();
  };

  return (
    <div
      className="modal"
      role="dialog"
      aria-modal="true"
      data-testid="cameras-modal"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal__box modal__box--cam">
        <div className="cam__modal-head">
          <span className="lbl">manage cameras</span>
          <button className="cam__manage" data-testid="cameras-close" onClick={onClose}>
            close
          </button>
        </div>
        <div className="cam__form" data-testid="cameras-form">
          <p className="cam__hint">
            External cameras the dashboard proxies — one row per camera, each a name and a
            URL that returns a single JPEG (e.g. <code>http://cam/snapshot.jpg</code>). Add an
            optional <strong>stream URL</strong> (an MJPEG <code>/stream</code> endpoint) for
            smooth live video instead of snapshot polling. The built-in printer camera is added
            automatically and isn&apos;t listed here.
          </p>
          {rows.length > 0 ? (
            <>
              <div className="cam__row cam__row--head">
                <span className="cam__col cam__col--label">name</span>
                <span className="cam__col">snapshot URL</span>
                <span className="cam__col">stream URL (optional)</span>
                <span className="cam__col--rm" aria-hidden="true" />
              </div>
              {rows.map((r, i) => (
                <div className="cam__row" key={i}>
                  <input
                    className="cam__in cam__in--label"
                    placeholder="e.g. front"
                    aria-label={`camera ${i + 1} name`}
                    value={r.label}
                    onChange={(e) =>
                      setRows((rs) => rs.map((x, j) => (j === i ? { ...x, label: e.target.value } : x)))
                    }
                  />
                  <input
                    className="cam__in"
                    placeholder="http://host/snapshot.jpg"
                    aria-label={`camera ${i + 1} snapshot URL`}
                    value={r.url}
                    data-testid={`camera-url-${i}`}
                    onChange={(e) =>
                      setRows((rs) => rs.map((x, j) => (j === i ? { ...x, url: e.target.value } : x)))
                    }
                  />
                  <input
                    className="cam__in"
                    placeholder="http://host/stream (optional)"
                    aria-label={`camera ${i + 1} stream URL`}
                    value={r.stream_url}
                    data-testid={`camera-stream-${i}`}
                    onChange={(e) =>
                      setRows((rs) => rs.map((x, j) => (j === i ? { ...x, stream_url: e.target.value } : x)))
                    }
                  />
                  <button
                    className="cam__rm"
                    data-testid={`camera-remove-${i}`}
                    title="remove this camera"
                    aria-label={`remove camera ${i + 1}`}
                    onClick={() => setRows((rs) => rs.filter((_, j) => j !== i))}
                  >
                    ✕
                  </button>
                </div>
              ))}
            </>
          ) : (
            <p className="cam__hint cam__hint--empty">
              No external cameras yet — “+ add camera”, then Save.
            </p>
          )}
          <div className="cam__row cam__row--actions">
            <button
              className="cam__btn"
              data-testid="camera-add"
              onClick={() => setRows((rs) => [...rs, { label: "", url: "", stream_url: "" }])}
            >
              + add camera
            </button>
            <button
              className="cam__btn cam__btn--save"
              data-testid="cameras-save"
              onClick={() => void save()}
            >
              save changes
            </button>
          </div>
          {status && (
            <div className="cam__status" data-testid="cameras-status">
              {status}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
