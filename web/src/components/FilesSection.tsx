import { useCallback, useEffect, useState } from "react";
import { Thumb } from "./widgets";
import { ModelView } from "./Viewer3D";

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
      const r = await fetch(`/api/files?dir=${encodeURIComponent(d)}`);
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
      const r = await fetch(`/api/files/upload?${q}`, { method: "POST", headers, body: file });
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
          <li className="filerow filerow--dir" onClick={goUp} data-testid="updir">
            <span className="ficon">↑</span>
            <span className="fname">..</span>
          </li>
        )}
        {sorted.map((e) =>
          e.is_dir ? (
            <li
              key={e.name}
              className="filerow filerow--dir"
              onClick={() => setDir(join(e.name))}
              data-testid="dir"
            >
              <span className="ficon">▸</span>
              <span className="fname">{e.name}/</span>
            </li>
          ) : (
            <li
              key={e.name}
              className={`filerow${printable(e.name) ? " filerow--file" : ""}`}
              data-testid="file"
              onClick={printable(e.name) ? () => setDetail({ path: join(e.name), entry: e }) : undefined}
              title={printable(e.name) ? "open details" : undefined}
            >
              <Thumb file={join(e.name)} className="thumb thumb--row" />
              <span className="fname">{e.name}</span>
              <span className="fsize dim">{fmtSize(e.size)}</span>
              {printable(e.name) && (
                <button
                  className="btn btn--sm"
                  onClick={(ev) => {
                    ev.stopPropagation();
                    setPrinting(join(e.name));
                  }}
                  data-testid="print"
                >
                  print
                </button>
              )}
            </li>
          ),
        )}
        {sorted.length === 0 && <li className="dim">empty</li>}
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
    <div className="modal" role="dialog" aria-modal="true" data-testid="file-detail">
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

function StartDialog({ path, onClose }: { path: string; onClose: () => void }) {
  const name = path.split("/").pop() ?? path;
  const is3mf = path.toLowerCase().endsWith(".3mf");
  const [plate, setPlate] = useState(1);
  const [useAms, setUseAms] = useState(false);
  const [amsMap, setAmsMap] = useState("");
  const [result, setResult] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

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
          dry_run: dryRun,
          confirm: !dryRun,
        }),
      });
      const d = (await r.json().catch(() => ({}))) as {
        error?: string;
        plan?: { plate: number; use_ams: boolean; ams_map: number[]; bed_type: string };
      };
      if (dryRun && r.status === 200 && d.plan) {
        setResult(
          `will print plate ${d.plan.plate} · AMS ${
            d.plan.use_ams ? d.plan.ams_map.join(",") || "(none)" : "off"
          } · bed ${d.plan.bed_type}`,
        );
      } else if (r.status === 200) setResult("✓ print started");
      else if (r.status === 202) setResult("sent — not yet verified");
      else if (r.status === 401) setResult("needs the control password (set it in Controls)");
      else setResult(d.error ?? `HTTP ${r.status}`);
    } catch {
      setResult("request failed");
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal" role="dialog" aria-modal="true" data-testid="start-dialog">
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
