import { useCallback, useEffect, useState } from "react";
import { Thumb } from "./widgets";
import { ModelView } from "./Viewer3D";
import { listCameras, type Camera } from "../cameras";
import { startTimelapse, type TimelapseMode } from "../timelapse";

// The default recording camera: park-capable (park detection) preferred, else any
// external (printer-synced smooth), else the first. The user can override this in the
// dialog — this is just the smart default.
function bestRecCamera(cams: Camera[]): Camera | undefined {
  return cams.find((c) => c.park) ?? cams.find((c) => c.kind === "external") ?? cams[0];
}

// The clean-timelapse method a camera uses (park detection vs printer-synced smooth).
function recMethod(cam: Camera): { mode: TimelapseMode; label: string } {
  return cam.park
    ? { mode: "park", label: "park detection" }
    : { mode: "smooth", label: "printer-synced" };
}

// When a print is started with timelapse armed, also kick off the clean-timelapse capture
// on the chosen camera so it's ONE action — no separate trip to the camera panel. Returns
// a short status to append to the start result. (Manual mid-print start still works.)
async function autoStartCleanTimelapse(pw: string | null, cam: Camera | undefined): Promise<string> {
  if (!cam) return "no camera selected — not recording";
  try {
    const res = await startTimelapse(recMethod(cam).mode, [cam.id], { every: 1 }, pw);
    if (res === "needPassword") return "set the control password to auto-record (Controls)";
    if ("error" in res) return `couldn't auto-record: ${res.error}`;
    return `recording a clean timelapse on “${cam.label}”`;
  } catch {
    return "couldn't start the recording";
  }
}

interface FileEntry {
  name: string;
  is_dir: boolean;
  size: number;
}

const REFRESH_MS = 5000;

// A simple file browser over the printer's storage: directories (navigable) and
// files (with plate thumbnails + a print action), auto-refreshing.
export function FilesSection({ sdcard }: { sdcard?: boolean | null }) {
  const [dir, setDir] = useState("/");
  const [entries, setEntries] = useState<FileEntry[] | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [printing, setPrinting] = useState<string | null>(null);
  const [detail, setDetail] = useState<{ path: string; entry: FileEntry } | null>(null);

  const refresh = useCallback(async (d: string) => {
    try {
      const r = await fetch(`/api/file?dir=${encodeURIComponent(d)}`);
      const data = (await r.json()) as { files?: FileEntry[]; error?: string };
      if (r.ok) {
        // Tolerate unexpected shapes (e.g. an older server) instead of crashing.
        setEntries((data.files ?? []).filter((e): e is FileEntry => typeof e?.name === "string"));
        setMsg(null);
      } else {
        setMsg(data.error ?? `HTTP ${r.status}`);
      }
    } catch {
      setMsg("network error");
    }
  }, []);

  // (Re)load on directory change, and auto-refresh on a timer.
  useEffect(() => {
    void refresh(dir);
    const id = setInterval(() => void refresh(dir), REFRESH_MS);
    return () => clearInterval(id);
  }, [dir, refresh]);

  const join = (name: string) => (dir === "/" ? `/${name}` : `${dir}/${name}`);
  const goUp = () => setDir((d) => d.replace(/\/[^/]+\/?$/, "") || "/");

  const onUpload = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    e.target.value = "";
    if (!file) return;
    setBusy(true);
    setMsg(`uploading ${file.name}…`);
    const pw = sessionStorage.getItem("bambu_pw");
    const headers: Record<string, string> = {};
    if (pw) headers["Authorization"] = `Bearer ${pw}`;
    try {
      const q = `dir=${encodeURIComponent(dir)}&name=${encodeURIComponent(file.name)}`;
      const r = await fetch(`/api/file/upload?${q}`, { method: "POST", headers, body: file });
      const d = (await r.json().catch(() => ({}))) as { error?: string };
      if (r.ok) {
        setMsg(`uploaded ${file.name}`);
        void refresh(dir);
      } else if (r.status === 401) {
        setMsg("upload needs the control password — set it in Controls, then retry");
      } else {
        setMsg(d.error ?? `HTTP ${r.status}`);
      }
    } catch {
      setMsg("upload failed");
    } finally {
      setBusy(false);
    }
  };

  const sorted = [...(entries ?? [])].sort((a, b) =>
    a.is_dir !== b.is_dir ? (a.is_dir ? -1 : 1) : a.name.localeCompare(b.name),
  );

  return (
    <section className="panel span-all" data-testid="files">
      <div className="secline files__head">
        <span className="lbl">files</span>
        <code className="path" data-testid="files-path">
          {dir}
        </code>
        <span className="files__spacer" />
        {sdcard != null && (
          <span className={`chip${sdcard ? "" : " warn"}`} data-testid="sd-chip">
            SD {sdcard ? "present" : "none"}
          </span>
        )}
        <button className="btn btn--sm" onClick={() => void refresh(dir)}>
          refresh
        </button>
        <label className={`btn btn--sm${busy ? " btn--busy" : ""}`}>
          {busy ? "uploading…" : "upload"}
          <input type="file" hidden disabled={busy} onChange={onUpload} data-testid="upload-input" />
        </label>
      </div>
      {msg && (
        <div className="dim files__msg" data-testid="files-msg">
          {msg}
        </div>
      )}
      <ul className="filelist">
        {dir !== "/" && (
          <li className="filerow filerow--dir" onClick={goUp} data-testid="updir" title="up one level">
            <span className="ficon ficon--folder">
              <FolderIcon />
            </span>
            <span className="fname fname--up">..</span>
            <span className="fsize" />
            <span className="frow__chev" aria-hidden>
              ‹
            </span>
          </li>
        )}
        {sorted.map((e) =>
          e.is_dir ? (
            <li
              key={e.name}
              className="filerow filerow--dir"
              onClick={() => setDir(join(e.name))}
              data-testid="dir"
              title={e.name}
            >
              <span className="ficon ficon--folder">
                <FolderIcon />
              </span>
              <span className="fname">{e.name}</span>
              <span className="fsize" />
              <span className="frow__chev" aria-hidden>
                ›
              </span>
            </li>
          ) : (
            <li
              key={e.name}
              className={`filerow${printable(e.name) ? " filerow--file" : ""}`}
              data-testid="file"
              onClick={printable(e.name) ? () => setDetail({ path: join(e.name), entry: e }) : undefined}
              title={printable(e.name) ? `${e.name} — open details` : e.name}
            >
              <span className="ficon ficon--file">
                <FileIcon />
                <Thumb file={join(e.name)} className="thumb thumb--row" />
              </span>
              <span className="fname">{e.name}</span>
              <span className="fsize dim">{fmtSize(e.size)}</span>
              {printable(e.name) ? (
                <button
                  className="btn btn--sm filerow__print"
                  onClick={(ev) => {
                    ev.stopPropagation();
                    setPrinting(join(e.name));
                  }}
                  data-testid="print"
                >
                  print
                </button>
              ) : (
                <span />
              )}
            </li>
          ),
        )}
        {sorted.length === 0 && <li className="filelist__empty dim">empty</li>}
      </ul>
      {printing && <StartDialog path={printing} onClose={() => setPrinting(null)} />}
      {detail && (
        <FileDetail
          path={detail.path}
          entry={detail.entry}
          onClose={() => setDetail(null)}
          onPrint={() => {
            setPrinting(detail.path);
            setDetail(null);
          }}
        />
      )}
    </section>
  );
}

// A file's detail screen: its metadata alongside the embedded 3D viewer, with a
// shortcut to the print dialog. Replaces the old per-row "3D" button.
function FileDetail({
  path,
  entry,
  onClose,
  onPrint,
}: {
  path: string;
  entry: FileEntry;
  onClose: () => void;
  onPrint: () => void;
}) {
  const name = path.split("/").pop() ?? path;
  return (
    <div
      className="modal"
      role="dialog"
      aria-modal="true"
      data-testid="file-detail"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal__box modal__box--detail">
        <div className="detail__head">
          <span className="lbl">file</span>
          <span className="detail__name">{name}</span>
          <button className="btn btn--sm detail__close" onClick={onClose}>
            close
          </button>
        </div>
        <div className="detail__body">
          <div className="detail__info">
            <Thumb file={path} className="thumb thumb--detail" />
            <dl className="kv">
              <dt>name</dt>
              <dd>{name}</dd>
              <dt>type</dt>
              <dd>{fileKind(name)}</dd>
              <dt>size</dt>
              <dd>{fmtSize(entry.size) || "—"}</dd>
              <dt>path</dt>
              <dd className="kv__path">{path}</dd>
            </dl>
            <button className="btn btn--go detail__print" onClick={onPrint} data-testid="detail-print">
              print…
            </button>
          </div>
          <ModelView path={path} />
        </div>
      </div>
    </div>
  );
}

function fileKind(name: string): string {
  const n = name.toLowerCase();
  if (n.endsWith(".gcode.3mf")) return "sliced 3MF";
  if (n.endsWith(".3mf")) return "3MF model";
  if (n.endsWith(".gcode")) return "G-code";
  return "file";
}

// The plan's clean-timelapse clause: combine whether the user armed timelapse with
// whether the sliced file actually has the per-layer park moves (detected server-side).
// Honest about the uncertain case (couldn't inspect → just echo the arm state).
function timelapseNote(armed: boolean, hasBlocks: boolean | null | undefined): string {
  if (hasBlocks == null) return armed ? "timelapse armed" : "timelapse off";
  if (armed)
    return hasBlocks
      ? "timelapse armed — head parks each layer ✓"
      : "⚠ timelapse armed but this file has no park moves — head won't park";
  return hasBlocks
    ? "timelapse off — this file supports a clean timelapse (arm it to use)"
    : "timelapse off";
}

function StartDialog({ path, onClose }: { path: string; onClose: () => void }) {
  const name = path.split("/").pop() ?? path;
  const is3mf = path.toLowerCase().endsWith(".3mf");
  const [plate, setPlate] = useState(1);
  const [useAms, setUseAms] = useState(false);
  const [amsMap, setAmsMap] = useState("");
  // Arm the printer-side timelapse: the per-layer head-park that makes a clean,
  // object-only timelapse possible. Default off — it changes print motion.
  const [timelapse, setTimelapse] = useState(false);
  // Auto-inspected on open, so the dialog shows whether THIS file supports a clean
  // timelapse without a Preview click: undefined = checking, null = couldn't inspect
  // (unknown), true/false = whether the sliced gcode has the per-layer park moves.
  const [hasBlocks, setHasBlocks] = useState<boolean | null | undefined>(undefined);
  const [result, setResult] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // The capability inspection is an open read (no password) — run it as soon as a .3mf
  // dialog opens (and when the plate changes), so "does this file timelapse?" is answered
  // up front rather than only after Preview.
  useEffect(() => {
    if (!is3mf) return;
    let live = true;
    setHasBlocks(undefined);
    void (async () => {
      try {
        const r = await fetch(
          `/api/file/inspect?name=${encodeURIComponent(path)}&plate=${plate}`,
        );
        const d = (await r.json().catch(() => ({}))) as {
          inspected?: boolean;
          has_timelapse_blocks?: boolean | null;
        };
        if (live) setHasBlocks(d.inspected ? !!d.has_timelapse_blocks : null);
      } catch {
        if (live) setHasBlocks(null);
      }
    })();
    return () => {
      live = false;
    };
  }, [path, plate, is3mf]);

  // Cameras for the recording-target picker. The smart default is the best camera; the
  // user can override it below when "record a clean timelapse" is on.
  const [cams, setCams] = useState<Camera[]>([]);
  const [recCam, setRecCam] = useState<string>("");
  useEffect(() => {
    let live = true;
    void (async () => {
      const list = await listCameras();
      if (!live) return;
      setCams(list);
      setRecCam((cur) => cur || bestRecCamera(list)?.id || "");
    })();
    return () => {
      live = false;
    };
  }, []);

  const run = async (dryRun: boolean) => {
    setBusy(true);
    setResult(dryRun ? "resolving plan…" : "starting…");
    const ams_map = useAms
      ? amsMap.split(",").map((s) => s.trim()).filter(Boolean).map(Number)
      : [];
    const pw = sessionStorage.getItem("bambu_pw");
    const headers: Record<string, string> = { "Content-Type": "application/json" };
    if (pw) headers["Authorization"] = `Bearer ${pw}`;
    try {
      const r = await fetch("/api/job/start", {
        method: "POST",
        headers,
        body: JSON.stringify({
          file: path,
          plate,
          use_ams: useAms,
          ams_map,
          timelapse,
          dry_run: dryRun,
          confirm: !dryRun,
        }),
      });
      const d = (await r.json().catch(() => ({}))) as {
        error?: string;
        plan?: {
          plate: number;
          use_ams: boolean;
          ams_map: number[];
          bed_type: string;
          // null = couldn't inspect the file (unknown); true/false = whether the sliced
          // gcode injects the per-layer park (the precondition for a clean timelapse).
          has_timelapse_blocks?: boolean | null;
        };
      };
      if (dryRun && r.status === 200 && d.plan) {
        setResult(
          `will print plate ${d.plan.plate} · AMS ${
            d.plan.use_ams ? d.plan.ams_map.join(",") || "(none)" : "off"
          } · bed ${d.plan.bed_type} · ${timelapseNote(timelapse, d.plan.has_timelapse_blocks)}`,
        );
      } else if (r.status === 200 || r.status === 202) {
        const base = r.status === 200 ? "✓ print started" : "sent — not yet verified";
        // One action: an armed print also auto-starts the clean-timelapse capture.
        if (timelapse) {
          setResult(`${base} · starting recording…`);
          const cam = cams.find((c) => c.id === recCam) ?? bestRecCamera(cams);
          setResult(`${base} · ${await autoStartCleanTimelapse(pw, cam)}`);
        } else {
          setResult(base);
        }
      } else if (r.status === 401) setResult("needs the control password (set it in Controls)");
      else setResult(d.error ?? `HTTP ${r.status}`);
    } catch {
      setResult("request failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div
      className="modal"
      role="dialog"
      aria-modal="true"
      data-testid="start-dialog"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal__box modal__box--start">
        <div className="start__head">
          <Thumb file={path} className="thumb thumb--start" />
          <div className="start__title">
            <span className="lbl">start a print</span>
            <span className="start__name">{name}</span>
          </div>
        </div>
        <ModelView path={path} />
        {is3mf && (
          <div className="startform">
            <label className="startrow">
              <span className="lbl">plate</span>
              <input
                type="number"
                min={1}
                className="pw startnum"
                value={plate}
                onChange={(e) => setPlate(Math.max(1, Number(e.target.value) || 1))}
                data-testid="start-plate"
              />
            </label>
            <label className="startrow">
              <input type="checkbox" checked={useAms} onChange={(e) => setUseAms(e.target.checked)} />
              <span>use AMS (multi-colour)</span>
            </label>
            {useAms && (
              <label className="startrow">
                <span className="lbl">tray per filament</span>
                <input
                  className="pw"
                  placeholder="0,3  (-1 = external spool)"
                  value={amsMap}
                  onChange={(e) => setAmsMap(e.target.value)}
                  data-testid="start-amsmap"
                />
              </label>
            )}
            <label
              className="startrow"
              title="parks the head out of shot each layer AND auto-starts the camera recording — one action for a clean, object-only timelapse"
            >
              <input
                type="checkbox"
                checked={timelapse}
                onChange={(e) => setTimelapse(e.target.checked)}
                data-testid="start-timelapse"
              />
              <span>record a clean timelapse (parks the head each layer)</span>
            </label>
            {/* The file's clean-timelapse capability, inspected on open. */}
            {hasBlocks != null && (
              <div
                className={`start__tlcap start__tlcap--${hasBlocks ? "ok" : "warn"}`}
                data-testid="start-tl-capability"
              >
                {hasBlocks
                  ? "✓ this file has per-layer park moves — a clean timelapse works"
                  : "⚠ this file has no per-layer park moves — the head won't park (no clean timelapse)"}
              </div>
            )}
            {/* Recording target: smart default (best camera), overridable when there's a
                choice. The method (park detection vs printer-synced) follows the camera. */}
            {timelapse && cams.length > 0 && (
              <label className="startrow start__reccam">
                <span className="lbl">record from</span>
                {cams.length > 1 ? (
                  <select
                    className="pw"
                    value={recCam}
                    data-testid="start-rec-cam"
                    onChange={(e) => setRecCam(e.target.value)}
                  >
                    {cams.map((c) => (
                      <option key={c.id} value={c.id}>
                        {c.label} · {recMethod(c).label}
                      </option>
                    ))}
                  </select>
                ) : (
                  <span className="dim" data-testid="start-rec-cam">
                    {cams[0].label} · {recMethod(cams[0]).label}
                  </span>
                )}
              </label>
            )}
            {/* Reassure that this is one action: the camera records automatically. */}
            {timelapse && (
              <p className="start__tlhint dim" data-testid="start-tl-hint">
                the chosen camera starts recording automatically when the print begins —
                watch and scrub it in the camera panel.
              </p>
            )}
          </div>
        )}
        <p className="start__help dim">
          <strong>Preview</strong> resolves the exact plan without printing.{" "}
          <strong>Start</strong> begins it — the printer must be idle.
        </p>
        {result && (
          <div className="files__msg" data-testid="start-result">
            {result}
          </div>
        )}
        <div className="btns">
          <button className="btn" onClick={onClose}>
            cancel
          </button>
          <button className="btn" disabled={busy} onClick={() => void run(true)}>
            preview
          </button>
          <button
            className="btn btn--go"
            disabled={busy}
            onClick={() => void run(false)}
            data-testid="start-confirm"
          >
            start print
          </button>
        </div>
      </div>
    </div>
  );
}

// Monochrome glyphs (currentColor) for the leading icon tile, so every row has a
// consistent, aligned leading slot — a folder for directories, a page for files (the
// plate thumbnail, when a sliced .3mf has one, paints over this page).
function FolderIcon() {
  return (
    <svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true">
      <path
        fill="currentColor"
        d="M3 6.6A1.6 1.6 0 0 1 4.6 5h4a1.6 1.6 0 0 1 1.13.47L11 6.7h8.4A1.6 1.6 0 0 1 21 8.3v9.1A1.6 1.6 0 0 1 19.4 19H4.6A1.6 1.6 0 0 1 3 17.4z"
      />
    </svg>
  );
}
function FileIcon() {
  return (
    <svg viewBox="0 0 24 24" width="17" height="17" aria-hidden="true">
      {/* page body */}
      <path
        fill="currentColor"
        d="M13 3H6.6a.6.6 0 0 0-.6.6v16.8a.6.6 0 0 0 .6.6h10.8a.6.6 0 0 0 .6-.6V8z"
      />
      {/* folded corner, punched out with the tile background */}
      <path fill="var(--hair-2)" d="M13 3v4.4a.6.6 0 0 0 .6.6H18z" />
    </svg>
  );
}

function printable(name: string): boolean {
  const n = name.toLowerCase();
  return n.endsWith(".3mf") || n.endsWith(".gcode");
}

function fmtSize(bytes: number): string {
  if (!bytes) return "";
  const u = ["B", "KB", "MB", "GB"];
  let n = bytes;
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n >= 10 || i === 0 ? Math.round(n) : n.toFixed(1)} ${u[i]}`;
}
