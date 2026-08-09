// Which printer this page is talking to.
//
// The server mounts every printer at `/api/printers/<id>/…` and the default one
// at `/api/…` as well, so a page with no printer selected keeps using exactly
// the paths it always did.

/** The `?printer=` in the address bar, if there is one. */
export function selectedPrinter(): string | null {
  const p = new URLSearchParams(location.search).get("printer");
  return p && p.trim() ? p : null;
}

/**
 * The API root every request is built from — `/api`, or one printer's prefix.
 *
 * Computed once at load rather than tracked as state: switching printers
 * navigates, and that is not a shortcut. A live WebSocket, a temperature
 * history, a file listing and a camera stream all belong to the machine that
 * produced them, so carrying any of them across a switch would show one
 * printer's readings under another's name. Reloading throws all of it away,
 * which is what changing machines means.
 */
export const API = (() => {
  const p = selectedPrinter();
  return p ? `/api/printers/${encodeURIComponent(p)}` : "/api";
})();

/** One printer in the switcher. */
export interface PrinterEntry {
  name: string;
  id: string;
  model: string | null;
  default: boolean;
  /** The printer's own status, as of the moment the list was taken. */
  status: { gcode_state?: string | null } | null;
}

/**
 * Every printer this server serves, each with its status.
 *
 * Server-level, so it is NOT under `API` — asking one printer for the list of
 * printers would be a strange thing to do, and would break the moment the
 * selected one was gone.
 */
export async function listPrinters(): Promise<PrinterEntry[]> {
  try {
    const r = await fetch("/api/printers");
    if (!r.ok) return [];
    const body = (await r.json()) as { printers?: PrinterEntry[] };
    return body.printers ?? [];
  } catch {
    return [];
  }
}

/**
 * Switch the page to another printer.
 *
 * The default printer drops the parameter entirely, so the common case keeps a
 * clean URL and a bookmark made before this existed still works.
 */
export function switchPrinter(entry: PrinterEntry): void {
  const url = new URL(location.href);
  if (entry.default) {
    url.searchParams.delete("printer");
  } else {
    url.searchParams.set("printer", entry.id);
  }
  location.assign(url.toString());
}
