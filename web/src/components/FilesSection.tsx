import { useEffect, useState } from "react";

// Files on the printer: list + upload. Listing is open; upload carries the
// control password (set in Controls) when the server requires one.
export function FilesSection() {
  const [files, setFiles] = useState<string[] | null>(null);
  const [msg, setMsg] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

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
    const pw = sessionStorage.getItem("bambu_pw");
    const headers: Record<string, string> = {};
    if (pw) headers["Authorization"] = `Bearer ${pw}`;
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
            <span className="fname">{f}</span>
          </li>
        ))}
        {files?.length === 0 && <li className="dim">no files</li>}
      </ul>
    </section>
  );
}
