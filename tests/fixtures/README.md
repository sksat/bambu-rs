# Test fixtures

Golden fixtures captured by **direct observation of real hardware** — the
clean-room source of truth (see the project plan). They are *not* derived from
any third-party Bambu library.

Each fixture carries a `_meta` block recording provenance (`model_code`,
`firmware`, `captured_at`, `mode`, `printer_state`, `source`, what was
`scrubbed`). The actual report payload is under `message`.

## Scrubbing

Network identifiers (SSID / IP / MAC, Wi-Fi signal) are redacted before a
capture is committed. Access codes are never part of a report payload. Raw,
unscrubbed captures live in the git-ignored `captures/` directory.

## Files

- `pushall-n1-idle.json` — a real `pushall` response from an A1-class printer
  (device model code `N1`) while idle. Notable observed facts: the response
  `command` is `push_status`; the report's `sequence_id` is the printer's own
  counter (not an echo of the request) — so command/effect correlation must use
  state predicates, not `sequence_id` matching; fan speeds are strings; `hms` is
  an empty array; `lights_report` is an array.

## Re-capturing

`tools/capture_pushall.py` (Python stdlib only) connects over MQTT-over-TLS,
sends one `pushall`, and writes the raw snapshot. It reads `BAMBU_IP`,
`BAMBU_SERIAL` and `BAMBU_CODE` from the environment and is read-only (it never
sends control commands). It is a bootstrap helper; the real tool is `bambu`.
