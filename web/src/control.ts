// POST a control action to the API. Reads are open; writes carry the optional
// password as a Bearer header (only when the server has one configured).
export type ControlResult =
  | { kind: "ok" } // 200 verified
  | { kind: "accepted" } // 202 unverified
  | { kind: "rejected"; reason: string } // 409
  | { kind: "needPassword" } // 401
  | { kind: "error"; message: string }; // 400 / 428 / 502 / network

function readField(data: unknown, key: string): string | null {
  if (data && typeof data === "object" && key in data) {
    const v = (data as Record<string, unknown>)[key];
    return typeof v === "string" ? v : null;
  }
  return null;
}

export async function sendControl(
  path: string,
  body: Record<string, unknown> | null,
  password: string | null,
): Promise<ControlResult> {
  const headers: Record<string, string> = {};
  if (body) headers["Content-Type"] = "application/json";
  if (password) headers["Authorization"] = `Bearer ${password}`;
  let r: Response;
  try {
    r = await fetch(path, {
      method: "POST",
      headers,
      body: body ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    return { kind: "error", message: e instanceof Error ? e.message : "network error" };
  }
  let data: unknown = null;
  try {
    data = await r.json();
  } catch {
    /* some responses have no body */
  }
  switch (r.status) {
    case 200:
      return { kind: "ok" };
    case 202:
      return { kind: "accepted" };
    case 401:
      return { kind: "needPassword" };
    case 409:
      return { kind: "rejected", reason: readField(data, "reason") ?? "rejected" };
    default:
      return { kind: "error", message: readField(data, "error") ?? `HTTP ${r.status}` };
  }
}
