// Presentation helpers (pure). The wire stays raw; formatting lives here.

export function fmtEta(min: number): string {
  if (min <= 0) return "done";
  if (min < 60) return `${min}m`;
  return `${Math.floor(min / 60)}h ${min % 60}m`;
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

export function remainText(remain?: number | null): string {
  // A1 spools report 0 (no RFID weight) and -1 means unknown — neither is "0%".
  return remain != null && remain > 0 ? `${remain}%` : "—";
}
