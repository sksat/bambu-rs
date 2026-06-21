import { useEffect, useRef, useState } from "react";
import {
  listCameras,
  getCamerasConfig,
  setCamerasConfig,
  listCaptures,
  type Camera,
  type CaptureRun,
  type CaptureCam,
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
  const liveSrc = `/api/camera/${id}/${stream ? "stream" : "snapshot"}?t=${ts}`;
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
            title={
              parkAvailable
                ? "review the captured per-layer (parked-head) timelapse frames"
                : "start a clean timelapse below to capture frames to review here"
            }
            onClick={() => show("park")}
          >
            captured
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

// The live state of a capture run, made legible. A run ARMS on start and only begins
// recording once the print is actually printing (lazy), so distinguish "waiting for the
// print" from "recording" from "failing" — the last WITH a reason, since a bare
// "0 frames · N failed" reads as broken when it's just an offline camera. Frame counts are
// per-camera (each camera gets one frame per trigger), so the number reads as the
// timelapse length, not a total to divide in your head.
function captureStatus(run: RunState): { label: string; warn?: boolean; detail?: string } {
  const n = run.cameras.length || 1;
  const per = Math.floor(run.frames / Math.max(n, 1));
  const frames = n > 1 ? `${per} frames/cam · ${n} cams` : `${run.frames} frames`;
  if (run.frames === 0 && run.failures === 0) {
    return { label: "waiting for the print…" };
  }
  if (run.failures > 0) {
    return {
      label: `rec · ${frames} · ${run.failures} failed`,
      warn: true,
      detail: run.last_error ?? "the camera grab is failing — is the camera online?",
    };
  }
  return { label: `rec · ${frames}` };
}

// A compact "now recording" row: what's being recorded, the live readout (waiting /
// rec · N / failing-in-red), and a stop. Shown only while that run is active.
function RecRow({
  label,
  testid,
  run,
  busy,
  onStop,
}: {
  label: string;
  testid: string;
  run: RunState;
  busy: boolean;
  onStop: () => void;
}) {
  const st = captureStatus(run);
  return (
    <div className="cam__rec-row" data-testid={`timelapse-${testid}`}>
      <span className="cam__rec-name">{label}</span>
      <span
        className={`cam__tl-rec${st.warn ? " cam__tl-rec--warn" : ""}`}
        data-testid={`timelapse-${testid}-running`}
        title={st.detail}
      >
        ● {st.label}
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
  );
}

// Tooltips for the clean-timelapse method override. Best-first; `segment` is the robust
// default, `park`/`smooth` kept for comparison or as fallbacks.
const METHOD_TITLE: Record<string, string> = {
  segment: "dense camera stream — one clean parked frame per layer (recommended)",
  park: "camera-detected park (legacy — misses the brief native park on some framings)",
  smooth: "printer layer-synced snapshot burst",
};

// Start/stop the serve-internal capture for this print. Two purposes, each auto-picking
// its method by the active camera: "clean timelapse" (one parked frame per layer — park
// detection on a stream+tuning camera, else printer-layer-synced smooth) and "print video"
// (the real stream, or interval snapshots). A run ARMS on start and records while the
// print runs, stopping on its own at the end. Captures the active camera, or — with "all
// cams" — every configured camera. Polls `/api/timelapse`.
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
  // What a manual "record" captures: the object-only timelapse, or a head-in-shot video.
  const [recType, setRecType] = useState<"timelapse" | "video">("timelapse");
  // Manual override of the clean-timelapse METHOD (segment/park/smooth). `null` = use the
  // capability-based default (the best the active camera supports); set it to force one.
  const [layerMethod, setLayerMethod] = useState<TimelapseMode | null>(null);

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
  // Two purposes, each auto-picking its METHOD by the active camera's capability:
  //  - clean timelapse → park (camera-detected) when the camera has a stream + tuning,
  //    else smooth (printer-layer-synced snapshots).
  //  - print video → records the real stream (stream cams) or samples snapshots (else).
  //    `plain` auto-splits this server-side; the interval only applies to snapshot cams.
  const active = cameras.find((c) => c.id === activeCamera);
  const activeIsSegment = active?.segment ?? false;
  const activeIsPark = active?.park ?? false;
  // The clean-timelapse methods this camera supports, best-first: the robust dense-stream
  // `segment` (stream + park_tuning + select_tuning) ≫ the old camera-detected `park`
  // (stream + park_tuning) ≫ printer-synced `smooth` (every camera). The first is the
  // DEFAULT; the user can override to any supported one (the method selector below).
  const layerMethods: TimelapseMode[] = activeIsSegment
    ? ["segment", "park", "smooth"]
    : activeIsPark
      ? ["park", "smooth"]
      : ["smooth"];
  // Effective method: the user's pick when it's available for this camera, else the default.
  const layerMode: TimelapseMode =
    layerMethod && layerMethods.includes(layerMethod) ? layerMethod : layerMethods[0];
  const layerRun =
    layerMode === "segment" ? tl?.segment : layerMode === "park" ? tl?.park : tl?.smooth;
  const targetCams = allCams && multi ? cameras : active ? [active] : [];
  const anySnapshot = targetCams.some((c) => !c.stream);
  // What the selected record type maps to: clean timelapse → park (camera-detected) or
  // smooth (printer-synced) by capability; video → plain (stream record or interval).
  const recMode: TimelapseMode = recType === "timelapse" ? layerMode : "plain";
  const recRun = recType === "timelapse" ? layerRun : tl?.plain;
  // The interval input only matters for a sampled (snapshot-camera) video.
  const showEvery = recType === "video" && anySnapshot;
  const layerHint =
    layerMode === "segment"
      ? "object only — one clean parked frame per layer, from the dense camera stream; review in the “captured” view above"
      : layerMode === "park"
        ? "object only — one parked frame per layer, detected from the camera; review in the “captured” view above"
        : "object only — one parked frame per layer, printer-synced";
  const recHint =
    recType === "timelapse"
      ? layerHint
      : anySnapshot
        ? "head in shot — a sped-up video sampled from snapshots"
        : "head in shot — records the live camera stream";

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

  // Is anything recording right now? When so, this section is just a status + stop; the
  // manual start controls are the secondary path (the print-start dialog is primary).
  const anyRunning = !!(layerRun?.running || tl?.plain.running);

  return (
    <div className="cam__tl" data-testid="timelapse-bar">
      <div className="cam__tl-head">
        <span className="lbl">recording</span>
        {/* Target: which camera(s) a MANUAL start captures. Only when there's a choice. */}
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
      {/* Frame the manual control as secondary — recording is normally armed at print
          start (one action). Only show this when nothing is recording yet. */}
      {!anyRunning && (
        <p className="cam__tl-cap dim" data-testid="capture-hint">
          starts automatically when you begin a print with “timelapse” on — or record one
          here for a print that's already running.
        </p>
      )}

      {/* What's recording now (each can run independently): a status + stop. */}
      {layerRun?.running && (
        <RecRow
          label="timelapse"
          testid={layerMode}
          run={layerRun}
          busy={busy}
          onStop={() => void stopMode(layerMode)}
        />
      )}
      {tl?.plain.running && (
        <RecRow
          label="video"
          testid="plain"
          run={tl.plain}
          busy={busy}
          onStop={() => void stopMode("plain")}
        />
      )}

      {/* Start a recording: pick WHAT (timelapse vs video) + details, then record. One
          "record" action — the type and interval are options, not separate modes. */}
      <div className="cam__rec">
        <div className="seg seg--rectype" role="radiogroup" aria-label="what to record">
          <button
            className={`btn btn--sm seg__opt${recType === "timelapse" ? " is-active" : ""}`}
            role="radio"
            aria-checked={recType === "timelapse"}
            disabled={busy}
            data-testid="rec-type-timelapse"
            onClick={() => setRecType("timelapse")}
          >
            timelapse
          </button>
          <button
            className={`btn btn--sm seg__opt${recType === "video" ? " is-active" : ""}`}
            role="radio"
            aria-checked={recType === "video"}
            disabled={busy}
            data-testid="rec-type-video"
            onClick={() => setRecType("video")}
          >
            video
          </button>
        </div>
        {/* Clean-timelapse METHOD: defaults to the best the camera supports; override here.
            Only shown when there's a real choice (a segment/park-capable camera). */}
        {recType === "timelapse" && layerMethods.length > 1 && (
          <div
            className="seg seg--method"
            role="radiogroup"
            aria-label="timelapse method"
            data-testid="rec-method"
          >
            {layerMethods.map((m) => (
              <button
                key={m}
                className={`btn btn--sm seg__opt${layerMode === m ? " is-active" : ""}`}
                role="radio"
                aria-checked={layerMode === m}
                disabled={busy}
                data-testid={`rec-method-${m}`}
                title={METHOD_TITLE[m]}
                onClick={() => setLayerMethod(m)}
              >
                {m}
              </button>
            ))}
          </div>
        )}
        {showEvery && (
          <label className="dim cam__rec-every">
            every{" "}
            <input
              className="pw cam__tl-secs"
              inputMode="decimal"
              value={plainSecs}
              disabled={busy}
              data-testid="timelapse-plain-secs"
              onChange={(e) => setPlainSecs(Number(e.target.value) || 0)}
            />{" "}
            s
          </label>
        )}
        <button
          className="cam__btn cam__rec-go"
          data-testid="record-start"
          disabled={busy || !!recRun?.running || (showEvery && plainSecs < 0.1)}
          title="records while the print runs and stops on its own when the print finishes"
          onClick={() => void startMode(recMode)}
        >
          <span className="cam__rec-dot" aria-hidden="true" />
          {recRun?.running ? "recording…" : "record"}
        </button>
      </div>
      <p className="cam__rec-hint dim" data-testid="rec-hint">
        {recHint}
      </p>

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
  const [recordings, setRecordings] = useState(false);
  // The park + smooth run states, so the view offers the "captured" toggle whenever a run
  // (active OR recently stopped) owns the active camera. A smooth run also publishes a
  // per-layer park filmstrip now (live selection), so it enables the toggle too.
  const [parkRun, setParkRun] = useState<RunState | null>(null);
  const [smoothRun, setSmoothRun] = useState<RunState | null>(null);
  const [segmentRun, setSegmentRun] = useState<RunState | null>(null);

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
      if (live) {
        setParkRun(s?.park ?? null);
        setSmoothRun(s?.smooth ?? null);
        setSegmentRun(s?.segment ?? null);
      }
    };
    void poll();
    const id = setInterval(() => void poll(), 2000);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, []);

  const activeCam = cameras.find((c) => c.id === active);
  // A filmstrip exists for the active camera iff a park OR smooth run (running or stopped)
  // captured it — the player then works (live tail while running, review-only after stop).
  // Smooth counts now: its live per-layer selection publishes the same park filmstrip.
  const ownsActive = (r: RunState | null) => !!r && r.cameras.includes(active);
  const parkAvailable =
    ownsActive(parkRun) || ownsActive(smoothRun) || ownsActive(segmentRun);

  return (
    <>
      <section className="panel cam" data-testid="cameras">
        <div className="cam__head">
          <span className="lbl">cameras</span>
          <div className="cam__head-actions">
            <button
              className="cam__manage"
              data-testid="recordings-open"
              onClick={() => setRecordings(true)}
            >
              recordings
            </button>
            <button
              className="cam__manage"
              data-testid="cameras-manage"
              onClick={() => setManaging(true)}
            >
              manage
            </button>
          </div>
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
            {/* Name the active camera's type, so the capture controls reading differently
                per tab is understood as "different cameras", not the UI rewording itself. */}
            {cameras.length > 1 && activeCam && (
              <div className="cam__camtype dim" data-testid="camera-caption">
                {activeCam.stream ? "live-stream camera" : "snapshot camera"}
                {activeCam.park ? " · park-capable" : ""}
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
      {recordings && <RecordingsModal onClose={() => setRecordings(false)} />}
    </>
  );
}

// Clean a run's dir-derived label for display: drop the trailing capture-mode suffix and
// the sanitized file extension, turn underscores into spaces ("cube-petg_gcode_3mf_park"
// → "cube-petg"). Falls back to the raw label if cleaning empties it.
function recLabel(label: string): string {
  const cleaned = label
    .replace(/_(park|smooth|plain)$/, "")
    .replace(/_gcode_3mf/g, "")
    .replace(/_3mf/g, "")
    .replace(/_/g, " ")
    .trim();
  return cleaned || label;
}

function recWhen(epoch: number): string {
  return epoch > 0 ? new Date(epoch * 1000).toLocaleString() : "—";
}

// A floating list of recorded capture runs (newest first). Each camera's mp4 link opens
// `/api/capture/<run>/<cam>/video.mp4`, which the server assembles on demand — so a
// timelapse you recorded is one click to play or save.
function RecordingsModal({ onClose }: { onClose: () => void }) {
  const [runs, setRuns] = useState<CaptureRun[] | null>(null);
  useEffect(() => {
    void listCaptures().then(setRuns);
  }, []);
  return (
    <div
      className="modal"
      role="dialog"
      aria-modal="true"
      data-testid="recordings-modal"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal__box modal__box--cam">
        <div className="cam__modal-head">
          <span className="lbl">recordings</span>
          <button className="cam__manage" data-testid="recordings-close" onClick={onClose}>
            close
          </button>
        </div>
        {runs === null ? (
          <div className="cam__status">loading…</div>
        ) : runs.length === 0 ? (
          <p className="cam__hint cam__hint--empty" data-testid="recordings-empty">
            No recordings yet. Start a print with “timelapse” on (or record one from the
            camera panel) and it'll show up here.
          </p>
        ) : (
          <div className="rec-list" data-testid="recordings-list">
            {runs.map((r) => (
              <div className="rec-run" key={r.id}>
                <div className="rec-run__head">
                  <span className="rec-run__label">{recLabel(r.label)}</span>
                  <span className="dim">{recWhen(r.started_at)}</span>
                </div>
                {r.cameras.map((c) => (
                  <RecCam key={c.id} runId={r.id} c={c} label={recLabel(r.label)} />
                ))}
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  );
}

// One recording in the list: a clickable poster (thumbnail + ▶) that expands to inline
// mp4 playback, plus a download link. The poster and the mp4 are produced on demand by the
// serve from the run's frames (or a stored plain.mp4), so park/smooth/video all play here
// without leaving the dashboard. Falls back to a plain ▶ tile if the thumbnail 404s.
function RecCam({ runId, c, label }: { runId: string; c: CaptureCam; label: string }) {
  const [open, setOpen] = useState(false);
  const [noThumb, setNoThumb] = useState(false);
  const base = `/api/capture/${encodeURIComponent(runId)}/${encodeURIComponent(c.id || "default")}`;
  const thumbUrl = `${base}/thumb.jpg`;
  const mp4Url = `${base}/video.mp4`;
  return (
    <div className="rec-cam" data-testid="rec-cam">
      <button
        className={`rec-cam__poster${noThumb ? " rec-cam__poster--empty" : ""}`}
        data-testid="rec-play"
        title="play (opens an enlarged player)"
        aria-label="play recording"
        onClick={() => setOpen(true)}
      >
        {!noThumb && (
          <img
            className="rec-cam__thumb"
            src={thumbUrl}
            alt=""
            loading="lazy"
            onError={() => setNoThumb(true)}
          />
        )}
        <span className="rec-cam__playicon" aria-hidden>
          ▶
        </span>
      </button>
      <div className="rec-cam__info">
        <span className={`rec-kind rec-kind--${c.kind}`}>
          {c.kind === "video" ? "video" : "timelapse"}
        </span>
        <span className="dim rec-cam__meta">
          {c.id || "camera"}
          {c.kind === "video" ? "" : ` · ${c.frames} frames`}
        </span>
        <a
          className="cam__btn rec-cam__open"
          href={mp4Url}
          target="_blank"
          rel="noreferrer"
          title="open / save the mp4 (assembled on first open — may take a few seconds)"
        >
          save ▸
        </a>
      </div>
      {open && (
        <RecLightbox
          mp4Url={mp4Url}
          thumbUrl={thumbUrl}
          title={`${label}${c.id ? ` · ${c.id}` : ""}`}
          onClose={() => setOpen(false)}
        />
      )}
    </div>
  );
}

// An enlarged player overlay for a single recording — clicking a poster opens this so the
// video plays big (up to ~80vh) instead of cramped in the list. Closes on the scrim, the
// close button, or Esc. Sits above the recordings modal (its own higher z-index layer).
function RecLightbox({
  mp4Url,
  thumbUrl,
  title,
  onClose,
}: {
  mp4Url: string;
  thumbUrl: string;
  title: string;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div
      className="modal rec-light"
      role="dialog"
      aria-modal="true"
      data-testid="rec-lightbox"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="rec-light__box">
        <div className="rec-light__head">
          <span className="rec-light__title">{title}</span>
          <button className="cam__manage" data-testid="rec-light-close" onClick={onClose}>
            close
          </button>
        </div>
        <video
          className="rec-light__video"
          data-testid="rec-video"
          src={mp4Url}
          poster={thumbUrl}
          controls
          autoPlay
        />
      </div>
    </div>
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
