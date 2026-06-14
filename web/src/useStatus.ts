import { useEffect, useRef, useState } from "react";
import type { PrinterStatus } from "./types";

export type Conn = "connecting" | "live" | "offline";

export interface TempPoint {
  nozzle: number | null;
  bed: number | null;
}

const HISTORY = 180; // ~3 min at 1 Hz; the sparkline window

/**
 * Subscribe to `/api/ws` (open — reads need no auth), auto-reconnecting. Returns
 * the latest status, the connection state, and a rolling history of nozzle/bed
 * temperatures for the sparkline.
 */
export function useStatus(): { status: PrinterStatus | null; conn: Conn; history: TempPoint[] } {
  const [status, setStatus] = useState<PrinterStatus | null>(null);
  const [conn, setConn] = useState<Conn>("connecting");
  const [history, setHistory] = useState<TempPoint[]>([]);
  // Guard against pushing a history point for an unchanged frame we re-render.
  const lastTemp = useRef<string>("");

  useEffect(() => {
    let ws: WebSocket | null = null;
    let retry: ReturnType<typeof setTimeout> | undefined;
    let closed = false;

    const connect = () => {
      const proto = location.protocol === "https:" ? "wss" : "ws";
      ws = new WebSocket(`${proto}://${location.host}/api/ws`);
      ws.onopen = () => setConn("live");
      ws.onmessage = (ev) => {
        let s: PrinterStatus;
        try {
          s = JSON.parse(ev.data as string) as PrinterStatus;
        } catch {
          return;
        }
        setStatus(s);
        const key = `${s.nozzle_temper ?? ""}/${s.bed_temper ?? ""}`;
        if (key !== lastTemp.current) {
          lastTemp.current = key;
          setHistory((h) => {
            const next = [...h, { nozzle: s.nozzle_temper ?? null, bed: s.bed_temper ?? null }];
            return next.length > HISTORY ? next.slice(next.length - HISTORY) : next;
          });
        }
      };
      ws.onclose = () => {
        if (closed) return;
        setConn("offline");
        retry = setTimeout(connect, 2000); // fixed-backoff reconnect
      };
      ws.onerror = () => ws?.close();
    };
    connect();

    return () => {
      closed = true;
      if (retry) clearTimeout(retry);
      ws?.close();
    };
  }, []);

  return { status, conn, history };
}
