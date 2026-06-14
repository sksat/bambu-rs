import { useEffect, useRef, useState } from "react";
import {
  listCameras,
  getCamerasConfig,
  setCamerasConfig,
  type Camera,
} from "../cameras";

// Refresh cadence (the delay AFTER a frame settles before fetching the next). We
// drive the loop off the <img>'s load/error rather than a fixed timer so a slow
// source can never be re-requested before the current grab finishes — important
// for the A1 built-in cam, whose grab can take seconds to fail; a fixed timer
// would cancel-and-retry faster than it errors, pile up server-side blocking
// grabs, and never even notice it's offline.
const FAST_MS = 800;
const SLOW_MS = 5000;

// A live view of ONE camera by id: an <img> pointed at the proxied snapshot,
// cache-busted each cycle. A configured-but-not-streaming source (e.g. the A1
// built-in cam is off) shows an "offline" message and polls slowly. Re-mount it
// (key by id) when the active tab changes to reset the loop cleanly.
function CameraView({ id, label }: { id: string; label: string }) {
  const [ts, setTs] = useState(() => Date.now());
  const [offline, setOffline] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => () => clearTimeout(timer.current), []);

  const scheduleNext = (delay: number) => {
    clearTimeout(timer.current);
    timer.current = setTimeout(() => setTs(Date.now()), delay);
  };

  return (
    <div className="cam__frame">
      <img
        className={offline ? "cam__view cam__view--off" : "cam__view"}
        alt={label}
        src={`/api/cameras/${id}/snapshot?t=${ts}`}
        data-testid="camera-view"
        onLoad={() => {
          if (offline) setOffline(false);
          scheduleNext(FAST_MS);
        }}
        onError={() => {
          if (!offline) setOffline(true);
          scheduleNext(SLOW_MS);
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
            {activeCam && <CameraView key={activeCam.id} id={activeCam.id} label={activeCam.label} />}
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
      else setRows(cfg.map((e) => ({ label: e.label, url: e.url })));
    })();
  }, [password]);

  const save = async () => {
    const external = rows
      .map((r) => ({ label: r.label.trim() || undefined, url: r.url.trim() }))
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
            External snapshot cameras the dashboard proxies — one row per camera, each a
            name and a URL that returns a single JPEG (e.g. <code>http://cam/snapshot.jpg</code>).
            The built-in printer camera is added automatically and isn&apos;t listed here.
          </p>
          {rows.length > 0 ? (
            <>
              <div className="cam__row cam__row--head">
                <span className="cam__col cam__col--label">name</span>
                <span className="cam__col">snapshot URL</span>
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
                    aria-label={`camera ${i + 1} URL`}
                    value={r.url}
                    data-testid={`camera-url-${i}`}
                    onChange={(e) =>
                      setRows((rs) => rs.map((x, j) => (j === i ? { ...x, url: e.target.value } : x)))
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
              onClick={() => setRows((rs) => [...rs, { label: "", url: "" }])}
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
