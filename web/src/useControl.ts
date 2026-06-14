import { useEffect, useState } from "react";
import { sendControl } from "./control";

export interface Toast {
  kind: "ok" | "warn" | "err";
  msg: string;
}

/** Control state + actions: posts to the API, surfaces a toast, handles the
 *  stop confirm dialog and the optional write password. */
export function useControl() {
  const [password, setPassword] = useState<string>(() => sessionStorage.getItem("bambu_pw") ?? "");
  const [needPassword, setNeedPassword] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [toast, setToast] = useState<Toast | null>(null);
  const [confirmStop, setConfirmStop] = useState(false);

  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), 4000);
    return () => clearTimeout(t);
  }, [toast]);

  const act = async (label: string, path: string, body: Record<string, unknown> | null) => {
    setBusy(label);
    const res = await sendControl(path, body, password || null);
    setBusy(null);
    switch (res.kind) {
      case "ok":
        setToast({ kind: "ok", msg: `${label}: verified` });
        break;
      case "accepted":
        setToast({ kind: "warn", msg: `${label}: sent (unverified)` });
        break;
      case "rejected":
        setToast({ kind: "err", msg: `${label}: rejected — ${res.reason}` });
        break;
      case "needPassword":
        setNeedPassword(true);
        setToast({ kind: "err", msg: "control needs a password" });
        break;
      case "error":
        setToast({ kind: "err", msg: `${label}: ${res.message}` });
        break;
    }
  };

  return {
    password,
    needPassword,
    busy,
    toast,
    confirmStop,
    setPassword: (pw: string) => {
      setPassword(pw);
      sessionStorage.setItem("bambu_pw", pw);
    },
    pause: () => act("pause", "/api/job/pause", { confirm: true }),
    resume: () => act("resume", "/api/job/resume", { confirm: true }),
    requestStop: () => setConfirmStop(true),
    cancelStop: () => setConfirmStop(false),
    confirmAndStop: () => {
      setConfirmStop(false);
      void act("stop", "/api/job/stop", { confirm: true });
    },
    light: (on: boolean) => act(`light ${on ? "on" : "off"}`, "/api/light", { node: "chamber", on }),
    speed: (level: string) => act(`speed ${level}`, "/api/speed", { level }),
  };
}

export type Control = ReturnType<typeof useControl>;
