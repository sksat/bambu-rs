import { useEffect, useRef, useState, type ReactNode } from "react";
import {
  listCameras,
  getCamerasConfig,
  setCamerasConfig,
  type Camera,
  type ParkTuning,
} from "../cameras";
import {
  getTimelapse,
  startTimelapse,
  stopTimelapse,
  type TimelapseState,
  type TimelapseMode,
  type RunState,
} from "../timelapse";
import { ParkPlayer } from "./ParkPlayer";

// Refresh cadence (the delay AFTER a frame settles before fetching the next). We
// drive the loop off the <img>'s load/error rather than a fixed timer so a slow
// source can never be re-requested before the current grab finishes — important
// for the A1 built-in cam, whose grab can take seconds to fail; a fixed timer
// would cancel-and-retry faster than it errors, pile up server-side blocking
// grabs, and never even notice it's offline.
const FAST_MS = 800;
const SLOW_MS = 5000;

// A live view of ONE camera by id. Views:
//   - live: the camera itself — `stream` (a long-lived <img> on the MJPEG endpoint,
//     continuous, no re-fetch loop) or `snapshot` (poll one JPEG per cycle, driven
//     off load/error so a slow source is never re-requested before its grab finishes).
//   - park: a scrubbable player over the captured per-layer park frames (the
//     <ParkPlayer> — index at `/park`, frames at `/park/{n}`); offered for park-capable
//     cameras and usable during a run AND after it stops (the filmstrip stays reviewable).
// A configured-but-dead source shows an "offline" message. Re-mount it (key by id)
// when the active tab changes to reset cleanly.
function CameraView({
  id,
  label,
  stream,
  park,
  parkAvailable,
}: {
  id: string;
  label: string;
  stream?: boolean;
  park?: boolean;
  // Whether a park run owns THIS camera (running OR stopped) — i.e. there's a filmstrip
  // to review, so the park toggle is enabled. Frames stay reviewable after the run stops.
  parkAvailable?: boolean;
}) {
  const [ts, setTs] = useState(() => Date.now());
  const [offline, setOffline] = useState(false);
  const [view, setView] = useState<"live" | "park">("live");
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => () => clearTimeout(timer.current), []);
  // Fall back to live if the park view stops being valid — capability cleared (toggle
  // hides) OR no park run owns this camera anymore (a new run elsewhere) — so we never
  // sit stuck on an unavailable park view.
  useEffect(() => {
    if (view === "park" && (!park || !parkAvailable)) setView("live");
  }, [park, parkAvailable, view]);

  const scheduleNext = (delay: number) => {
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setTs(Date.now()), delay);
  };

  const isPark = view === "park";
  // Live is a continuous stream or a snapshot poll; the park view is the <ParkPlayer>.
  const polling = !stream;
  const liveSrc = `/api/cameras/${id}/${stream ? "stream" : "snapshot"}?t=${ts}`;
  const show = (v: "live" | "park") => {
    setView(v);
    setOffline(false);
    setTs(Date.now()); // force a fresh grab on switch
  };

  return (
    <div className="cam__frame">
      {park && (
        <div className="cam__toggle" data-testid="camera-view-toggle">
          <button
            className={!isPark ? "cam__toggle-btn cam__toggle-btn--on" : "cam__toggle-btn"}
            data-testid="camera-view-live"
            onClick={() => show("live")}
          >
            live
          </button>
          <button
            className={isPark ? "cam__toggle-btn cam__toggle-btn--on" : "cam__toggle-btn"}
            data-testid="camera-view-park"
            disabled={!parkAvailable}
            title={parkAvailable ? undefined : "start a park run to capture frames"}
            onClick={() => show("park")}
          >
            park
          </button>
        </div>
      )}
      {isPark ? (
        <ParkPlayer id={id} />
      ) : (
        <>
          <img
            className={offline ? "cam__view cam__view--off" : "cam__view"}
            alt={label}
            src={liveSrc}
            data-testid="camera-view"
            data-mode={stream ? "stream" : "snapshot"}
            onLoad={() => {
              if (offline) setOffline(false);
              // A continuous stream is never re-requested; snapshots are.
              if (polling) scheduleNext(FAST_MS);
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
        </>
      )}
    </div>
  );
}

// One frame count, reported per-camera (each camera gets one frame per trigger)
// so the number reads as the timelapse length, not a total to divide in your head.
function runLabel(run: RunState | undefined): string {
  const n = run?.cameras.length ?? 1;
  const per = Math.floor((run?.frames ?? 0) / Math.max(n, 1));
  const failed = run?.failures ? ` · ${run.failures} failed` : "";
  return (n > 1 ? `${per} frames/cam · ${n} cams` : `${run?.frames ?? 0} frames`) + failed;
}

// One timelapse mode as a self-describing row: its name, a VISIBLE plain-language
// description of what it captures (not a tooltip — so it's obvious what each does), and a
// start button, or — while running — a recording readout + stop. Each mode is an
// independent run (all can be on at once).
function ModeRow({
  name,
  desc,
  testid,
  run,
  busy,
  onStart,
  onStop,
  startDisabled,
}: {
  name: string;
  desc: ReactNode;
  testid: string;
  run: RunState | undefined;
  busy: boolean;
  onStart: () => void;
  onStop: () => void;
  startDisabled?: boolean;
}) {
  return (
    <div className="cam__mode" data-testid={`timelapse-${testid}`}>
      <div className="cam__mode-info">
        <span className="cam__mode-name">{name}</span>
        <span className="cam__mode-desc dim">{desc}</span>
      </div>
      {run?.running ? (
        <div className="cam__mode-act">
          <span className="cam__tl-rec" data-testid={`timelapse-${testid}-running`}>
            ● rec · {runLabel(run)}
          </span>
          <button
            className="cam__btn"
            data-testid={`timelapse-${testid}-stop`}
            disabled={busy}
            onClick={onStop}
          >
            stop
          </button>
        </div>
      ) : (
        <button
          className="cam__btn cam__btn--save cam__mode-start"
          data-testid={`timelapse-${testid}-start`}
          disabled={busy || startDisabled}
          onClick={onStart}
        >
          start
        </button>
      )}
    </div>
  );
}

// Start/stop the serve-internal timelapse capture for this print. Three independent
// modes (any can be on at once), each self-describing: "smooth" = one clean frame per
// layer (head parked out of shot); "plain" = a frame every N seconds (head in shot, a
// normal video); "park" = live per-layer frames detected from the camera (scrubbed in the
// park view above). Captures the active camera, or — with the "all cams" target — every
// configured camera at once. Polls `/api/timelapse`.
function TimelapseBar({
  cameras,
  activeCamera,
  password,
}: {
  cameras: Camera[];
  activeCamera: string;
  password: string | null;
}) {
  const [tl, setTl] = useState<TimelapseState | null>(null);
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [allCams, setAllCams] = useState(false);
  const [plainSecs, setPlainSecs] = useState(3);

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

  const multi = cameras.length > 1;
  const targets = allCams && multi ? cameras.map((c) => c.id) : [activeCamera];
  const activeIsPark = cameras.find((c) => c.id === activeCamera)?.park ?? false;

  const startMode = async (mode: TimelapseMode) => {
    setBusy(true);
    setMsg(null);
    const opts = mode === "plain" ? { intervalMs: Math.max(0.1, plainSecs) * 1000 } : { every: 1 };
    const r = await startTimelapse(mode, targets, opts, password);
    if (r === "needPassword") setMsg("needs the control password (set it in Controls)");
    else if ("error" in r) setMsg(r.error);
    else setTl(await getTimelapse());
    setBusy(false);
  };
  const stopMode = async (mode: TimelapseMode) => {
    setBusy(true);
    setMsg(null);
    await stopTimelapse(mode, password);
    setTl(await getTimelapse());
    setBusy(false);
  };

  return (
    <div className="cam__tl" data-testid="timelapse-bar">
      <div className="cam__tl-head">
        <span className="lbl">timelapse</span>
        {/* Target: which camera(s) a started mode captures. Only when there's a choice. */}
        {multi && (
          <div className="seg seg--target" role="radiogroup" aria-label="capture target">
            <button
              className={`btn btn--sm seg__opt${!allCams ? " is-active" : ""}`}
              role="radio"
              aria-checked={!allCams}
              disabled={busy}
              title="capture only the camera shown above"
              onClick={() => setAllCams(false)}
            >
              this cam
            </button>
            <button
              className={`btn btn--sm seg__opt${allCams ? " is-active" : ""}`}
              role="radio"
              aria-checked={allCams}
              disabled={busy}
              data-testid="timelapse-all"
              title="capture every configured camera at once (multi-angle)"
              onClick={() => setAllCams(true)}
            >
              all cams ({cameras.length})
            </button>
          </div>
        )}
      </div>

      <ModeRow
        name="smooth"
        desc="one clean frame per layer — head parked out of shot (the classic timelapse)"
        testid="smooth"
        run={tl?.smooth}
        busy={busy}
        onStart={() => void startMode("smooth")}
        onStop={() => void stopMode("smooth")}
      />

      <ModeRow
        name="plain"
        desc={
          tl?.plain.running ? (
            <>a frame every {plainSecs}s — head in shot (a normal sped-up video)</>
          ) : (
            <>
              a frame every{" "}
              <input
                className="pw cam__tl-secs"
                inputMode="decimal"
                value={plainSecs}
                disabled={busy}
                data-testid="timelapse-plain-secs"
                onChange={(e) => setPlainSecs(Number(e.target.value) || 0)}
              />{" "}
              s — head in shot (a normal sped-up video)
            </>
          )
        }
        testid="plain"
        run={tl?.plain}
        busy={busy}
        onStart={() => void startMode("plain")}
        onStop={() => void stopMode("plain")}
        startDisabled={plainSecs < 0.1}
      />

      {/* park: only for park-capable cameras (a stream + a calibrated tuning). */}
      {activeIsPark && (
        <ModeRow
          name="park"
          desc="live per-layer frames detected from the camera — scrub them in the park view above"
          testid="park"
          run={tl?.park}
          busy={busy}
          onStart={() => void startMode("park")}
          onStop={() => void stopMode("park")}
        />
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
  // The park run's state, so the view offers the park toggle whenever a run (active OR
  // recently stopped) owns the active camera — its filmstrip is reviewable either way.
  const [parkRun, setParkRun] = useState<RunState | null>(null);

  const reload = async () => {
    const cams = await listCameras();
    setCameras(cams);
    setActive((a) => (cams.some((c) => c.id === a) ? a : (cams[0]?.id ?? "")));
  };

  useEffect(() => {
    void reload();
  }, []);

  useEffect(() => {
    let live = true;
    const poll = async () => {
      const s = await getTimelapse();
      if (live) setParkRun(s?.park ?? null);
    };
    void poll();
    const id = setInterval(() => void poll(), 2000);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, []);

  const activeCam = cameras.find((c) => c.id === active);
  // A filmstrip exists for the active camera iff a park run (running or stopped) captured
  // it — the player then works (live tail while running, review-only after stop).
  const parkAvailable = !!parkRun && parkRun.cameras.includes(active);

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
                park={activeCam.park}
                parkAvailable={parkAvailable}
              />
            )}
            {activeCam && (
              <TimelapseBar cameras={cameras} activeCamera={activeCam.id} password={password} />
            )}
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
  // Raw JSON for the per-camera park tuning (empty = none). Validated on save.
  park_tuning: string;
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
      else
        setRows(
          cfg.map((e) => ({
            label: e.label,
            url: e.url,
            stream_url: e.stream_url ?? "",
            park_tuning: e.park_tuning ? JSON.stringify(e.park_tuning, null, 2) : "",
          })),
        );
    })();
  }, [password]);

  const save = async () => {
    const external: {
      label?: string;
      url: string;
      stream_url?: string;
      park_tuning?: ParkTuning;
    }[] = [];
    for (const r of rows) {
      if (!r.url.trim()) continue;
      let park_tuning: ParkTuning | undefined;
      const pt = r.park_tuning.trim();
      if (pt) {
        try {
          park_tuning = JSON.parse(pt) as ParkTuning;
        } catch {
          setStatus(`invalid park tuning JSON for “${r.label || r.url}” — must be a JSON object`);
          return;
        }
      }
      external.push({
        label: r.label.trim() || undefined,
        url: r.url.trim(),
        stream_url: r.stream_url.trim() || undefined,
        park_tuning,
      });
    }
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
                <div className="cam__row-group" key={i}>
                  <div className="cam__row">
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
                        setRows((rs) =>
                          rs.map((x, j) => (j === i ? { ...x, stream_url: e.target.value } : x)),
                        )
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
                  {/* Optional per-camera park tuning (needs a stream URL too). Edited as
                      raw JSON — paste tuning.example.json and calibrate; server validates. */}
                  <details className="cam__park" open={r.park_tuning.trim() !== ""}>
                    <summary className="cam__park-sum" data-testid={`camera-park-toggle-${i}`}>
                      park tuning (JSON) — enables the live park preview
                    </summary>
                    <textarea
                      className="cam__park-json"
                      data-testid={`camera-park-${i}`}
                      placeholder={'paste tuning.example.json and calibrate (left_frac, abs_floor, …)'}
                      value={r.park_tuning}
                      spellCheck={false}
                      rows={6}
                      onChange={(e) =>
                        setRows((rs) =>
                          rs.map((x, j) => (j === i ? { ...x, park_tuning: e.target.value } : x)),
                        )
                      }
                    />
                  </details>
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
              onClick={() =>
              setRows((rs) => [...rs, { label: "", url: "", stream_url: "", park_tuning: "" }])
            }
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
