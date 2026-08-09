import { useEffect, useState } from "react";
import { listPrinters, selectedPrinter, switchPrinter } from "../api";
import type { PrinterEntry } from "../api";
import type { Conn } from "../useStatus";
import { useTheme } from "../useTheme";
import type { Theme } from "../useTheme";

const ICON: Record<Theme, string> = { auto: "◐", dark: "●", light: "○" };

/**
 * The printer picker — rendered when this server has more than one, or when the
 * one it has is not the one the URL asks for.
 *
 * A single-printer server normally shows nothing at all: a select with one
 * option is a decision the user does not have, and the header is the same as it
 * was. The exception is a `?printer=` naming a machine that is gone — every
 * request on the page is then going to a prefix that 404s, and hiding the
 * picker would leave hand-editing the URL as the only way back.
 */
function PrinterPicker() {
  const [printers, setPrinters] = useState<PrinterEntry[]>([]);
  useEffect(() => {
    let live = true;
    let timer: ReturnType<typeof setTimeout>;
    // Re-read rather than snapshot at mount: the labels carry each printer's
    // state, and a dashboard left open would otherwise keep calling a finished
    // print "running". Chained rather than on an interval so a slow response
    // can't stack requests.
    const poll = async () => {
      const ps = await listPrinters();
      if (!live) return;
      setPrinters(ps);
      timer = setTimeout(() => void poll(), 5000);
    };
    void poll();
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, []);
  const selected = selectedPrinter();
  const current = printers.find((p) => p.id === selected);
  const fallback = printers.find((p) => p.default);
  // A stale or mistyped `?printer=` points the whole page at a prefix the
  // server does not serve. Showing the default as selected would hide that —
  // the select would already read "correct" and there would be nothing to pick
  // to recover. Say it instead, and let picking a real printer fix the URL.
  //
  // An empty list is the list request failing or not having answered yet, which
  // says nothing about the selection — so it is not "unknown", and the picker
  // stays away rather than flashing an error on every load.
  const unknown = selected !== null && printers.length > 0 && current === undefined;
  // Decided after `unknown`, not before: this is the one case where a
  // one-option select is worth showing, because it is the way out.
  if (printers.length < 2 && !unknown) return null;
  return (
    <select
      className={unknown ? "printer printer--unknown" : "printer"}
      value={current?.id ?? (unknown ? "" : (fallback?.id ?? ""))}
      onChange={(e) => {
        const next = printers.find((p) => p.id === e.target.value);
        if (next) switchPrinter(next);
      }}
      title={
        unknown
          ? `no printer called ${selected} — pick one`
          : "which printer this page is showing"
      }
      aria-label="printer"
      data-testid="printer"
    >
      {unknown && (
        <option value="" disabled>
          {`${selected}? — not served here`}
        </option>
      )}
      {printers.map((p) => (
        <option key={p.id} value={p.id}>
          {p.name}
          {p.status?.gcode_state ? ` — ${p.status.gcode_state.toLowerCase()}` : ""}
        </option>
      ))}
    </select>
  );
}

export function Header({ conn }: { conn: Conn }) {
  const { theme, cycle } = useTheme();
  return (
    <header className="hdr">
      <span className="hdr__brand">
        bambu<span className="dim"> / dashboard</span>
      </span>
      <div className="hdr__right">
        <PrinterPicker />
        <a
          className="ghlink"
          href="https://github.com/sksat/bambu-rs"
          target="_blank"
          rel="noreferrer"
          title="GitHub repository"
          aria-label="GitHub repository"
          data-testid="github"
        >
          <svg viewBox="0 0 16 16" width="18" height="18" aria-hidden="true">
            <path
              fill="currentColor"
              d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z"
            />
          </svg>
        </a>
        <button
          className="theme"
          onClick={cycle}
          title={`theme: ${theme} (click to change)`}
          aria-label={`theme: ${theme}`}
          data-testid="theme"
        >
          {ICON[theme]} {theme}
        </button>
        <span className={`conn conn--${conn}`} data-testid="conn">
          <i className="dot" />
          {conn}
        </span>
      </div>
    </header>
  );
}
