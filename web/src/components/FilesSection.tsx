import { useEffect, useState } from "react";
import { Thumb } from "./widgets";

// Files on the printer: list + upload + start a print. Listing is open; upload
// and start carry the control password (set in Controls) when required.
export function FilesSection() {
  const [files, setFiles] = useState<string[] | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [printing, setPrinting] = useState<string | null>(null);

  const refresh = async () => {
    try {
      const r = await fetch("/api/files");
      const d = (await r.json()) as { files?: string[]; error?: string };
      if (r.ok) setFiles(d.files ?? []);
      else setMsg(d.error ?? `HTTP ${r.status}`);
    } catch {
      setMsg("network error");
    }
  };

  useEffect(() => {
    void refresh();
  }, []);

  const onUpload = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0];
    e.target.value = "";
    if (!file) return;
    setBusy(true);
    setMsg(`uploading ${file.name}…`);
    const headers = authHeaders();
    try {
      const r = await fetch(`/api/files/upload?name=${encodeURIComponent(file.name)}`, {
        method: "POST",
        headers,
        body: file,
      });
      const d = (await r.json().catch(() => ({}))) as { error?: string };
      if (r.ok) {
        setMsg(`uploaded ${file.name}`);
        void refresh();
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

  return (
    <section className="panel span-all" data-testid="files">
      <div className="secline">
        <span className="lbl">files</span>
        <button className="btn btn--sm" onClick={() => void refresh()}>
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
        {(files ?? []).map((f) => (
          <li key={f} className="filerow" data-testid="file">
            <Thumb file={f} className="thumb thumb--row" />
            <span className="fname">{f}</span>
            {printable(f) && (
              <button className="btn btn--sm" onClick={() => setPrinting(f)} data-testid="print">
                print
              </button>
            )}
          </li>
        ))}
        {files?.length === 0 && <li className="dim">no files</li>}
      </ul>
      {printing && <StartDialog file={printing} onClose={() => setPrinting(null)} />}
    </section>
  );
}

function StartDialog({ file, onClose }: { file: string; onClose: () => void }) {
  const [plate, setPlate] = useState(1);
  const [useAms, setUseAms] = useState(false);
  const [amsMap, setAmsMap] = useState("");
  const [result, setResult] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const is3mf = file.toLowerCase().endsWith(".3mf");

  const run = async (dryRun: boolean) => {
    setBusy(true);
    setResult(dryRun ? "resolving plan…" : "starting…");
    const ams_map = useAms
      ? amsMap
          .split(",")
          .map((s) => s.trim())
          .filter(Boolean)
          .map(Number)
      : [];
    const body = {
      file: `/${file}`,
      plate,
      use_ams: useAms,
      ams_map,
      dry_run: dryRun,
      confirm: !dryRun,
    };
    try {
      const r = await fetch("/api/job/start", {
        method: "POST",
        headers: { "Content-Type": "application/json", ...authHeaders() },
        body: JSON.stringify(body),
      });
      const d = (await r.json().catch(() => ({}))) as {
        error?: string;
        plan?: { plate: number; use_ams: boolean; ams_map: number[]; bed_type: string };
      };
      if (dryRun && r.status === 200 && d.plan) {
        setResult(
          `plan — plate ${d.plan.plate}, ams ${
            d.plan.use_ams ? d.plan.ams_map.join(",") || "(none)" : "off"
          }, bed ${d.plan.bed_type}`,
        );
      } else if (r.status === 200) setResult("print started (verified)");
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
      <div className="modal__box">
        <p className="modal__msg">
          start print: <strong>{file}</strong>
        </p>
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
              disabled={!is3mf}
            />
          </label>
          {is3mf && (
            <label className="startrow">
              <input type="checkbox" checked={useAms} onChange={(e) => setUseAms(e.target.checked)} />
              <span className="lbl">use AMS</span>
            </label>
          )}
          {is3mf && useAms && (
            <label className="startrow">
              <span className="lbl">tray map</span>
              <input
                className="pw"
                placeholder="e.g. 0,3 (-1 = external)"
                value={amsMap}
                onChange={(e) => setAmsMap(e.target.value)}
                data-testid="start-amsmap"
              />
            </label>
          )}
        </div>
        {result && (
          <div className="dim files__msg" data-testid="start-result">
            {result}
          </div>
        )}
        <div className="btns">
          <button className="btn" onClick={onClose}>
            close
          </button>
          <button className="btn btn--sm" disabled={busy} onClick={() => void run(true)}>
            dry-run
          </button>
          <button
            className="btn btn--danger"
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

function authHeaders(): Record<string, string> {
  const pw = sessionStorage.getItem("bambu_pw");
  return pw ? { Authorization: `Bearer ${pw}` } : {};
}
