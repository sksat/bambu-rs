// Presentation helpers (pure). The wire stays raw; formatting lives here.

import type { Ams } from "./bindings/Ams";

// How many AMS spools have actually been RFID-identified.
//
// 誤報について: the dashboard used to key its RFID indicator off the report's
// `online.rfid` flag. On the A1 mini (AMS lite) that flag — like `online.ahb` — is an
// X1/P1-style "AMS hub / RFID bus present" bit that this hardware ALWAYS reports false
// for, even though the reader works fine (every Bambu spool gets a tag read). The result
// was a permanent "rfid ✕ offline" amber warning: a pure false alarm. So we derive the
// state from real evidence instead — a spool whose RFID tag was read has a non-empty
// `uuid`. ≥1 read ⇒ the reader provably works (✓). Zero reads is ambiguous (empty AMS,
// all generic/non-RFID filament, or a genuine fault), so the caller shows ✕ WITHOUT a
// warn tone rather than crying "offline" about something it can't actually distinguish.
export function amsRfidReads(ams: Ams): number {
  const trays = [
    ...(ams.units ?? []).flatMap((u) => u.trays ?? []),
    ...(ams.external ? [ams.external] : []),
  ];
  return trays.filter((t) => (t.uuid ?? "") !== "").length;
}

export function fmtEta(min: number): string {
  if (min <= 0) return "done";
  if (min < 60) return `${min}m`;
  return `${Math.floor(min / 60)}h ${min % 60}m`;
}

/** Wall-clock time the job is expected to finish: `now + min` as `HH:MM`. A job
 *  spanning midnight gets a `+Nd` suffix so 02:15 tomorrow isn't read as today.
 *  `now` is a parameter (not read inside) to keep this pure and testable. */
export function fmtFinishTime(min: number, now: Date): string {
  if (min <= 0) return "now";
  const finish = new Date(now.getTime() + min * 60_000);
  const hh = String(finish.getHours()).padStart(2, "0");
  const mm = String(finish.getMinutes()).padStart(2, "0");
  const startOfDay = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((startOfDay(finish) - startOfDay(now)) / 86_400_000);
  return days > 0 ? `${hh}:${mm} +${days}d` : `${hh}:${mm}`;
}

export function speedName(lvl: number): string {
  return (
    ({ 1: "silent", 2: "standard", 3: "sport", 4: "ludicrous" } as Record<number, string>)[lvl] ??
    `speed ${lvl}`
  );
}

// Bambu colours are RGBA hex (e.g. DE4343FF); drop the alpha for CSS.
export function swatch(color?: string | null, cols?: string[]): string | null {
  const c = color ?? cols?.[0];
  if (!c) return null;
  return `#${c.length >= 6 ? c.slice(0, 6) : c}`;
}

/** Tray ids are 0-based on the wire, but the AMS unit is physically labelled
 *  1-based — so show tray N as N+1 to match the number printed on the hardware.
 *  The sentinels (255 none / 254 external) and anything non-numeric pass through
 *  unchanged. Presentation only: the wire value (ams_map, commands) stays 0-based. */
export function trayLabel(id: string | number | null | undefined): string {
  const n = Number(id);
  return Number.isInteger(n) && n >= 0 && n < 254 ? String(n + 1) : String(id);
}

/** The AMS activity line, in plain words. The raw pointers (`tray_now` /
 *  `tray_tar`) use 255 for "none" and 254 for the external spool, so the old
 *  "swapping {now} → {tar}" rendered nonsense like "swapping 256 → 3" while
 *  loading from empty. Resolve to what's actually happening — loading /
 *  unloading / a tray-to-tray swap / idle — with 1-based tray numbers.
 *  `active` is true while an operation is in flight (for styling). */
export function amsActivity(
  now: string | null,
  tar: string | null,
): { text: string; active: boolean } {
  const name = (v: string | null): string | null =>
    v == null || v === "255" ? null : v === "254" ? "ext" : `tray ${trayLabel(v)}`;
  const nowName = name(now);
  const tarName = name(tar);
  // An operation is in flight when there's a target that differs from current.
  if (tar != null && tar !== now) {
    if (nowName == null && tarName != null) return { text: `loading ${tarName}`, active: true };
    if (tarName == null && nowName != null) return { text: `unloading ${nowName}`, active: true };
    if (nowName != null && tarName != null)
      return { text: `${nowName} → ${tarName}`, active: true };
  }
  return { text: nowName ? `${nowName} loaded` : "idle", active: false };
}

export function remainText(remain?: number | null): string {
  // A1 spools report 0 (no RFID weight) and -1 means unknown — neither is "0%".
  return remain != null && remain > 0 ? `${remain}%` : "—";
}

/** Wi-Fi signal strength, parsed from the wire string (`wifi_signal`, e.g.
 *  `-62dBm`) into a display tier: a bar count (1–4), a colour tone, and a word.
 *  The three together encode strength redundantly so it doesn't rely on colour
 *  (CUD). Returns null when absent or unparseable, so the indicator simply
 *  doesn't render. Bands are the usual dBm ranges; tune against the device. */
export function wifiTier(
  signal: string | null,
): { dbm: number; bars: 1 | 2 | 3 | 4; tone: "ok" | "warn" | "err"; word: string } | null {
  if (!signal) return null;
  const dbm = parseInt(signal, 10);
  if (!Number.isFinite(dbm)) return null;
  if (dbm >= -55) return { dbm, bars: 4, tone: "ok", word: "strong" };
  if (dbm >= -67) return { dbm, bars: 3, tone: "warn", word: "fair" };
  if (dbm >= -75) return { dbm, bars: 2, tone: "warn", word: "fair" };
  return { dbm, bars: 1, tone: "err", word: "weak" };
}
