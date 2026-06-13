# Bambu Lab LAN protocol — observations

Clean-room notes for `bambu-rs`. Everything here is from **the protocol
documentation** ([OpenBambuAPI](https://github.com/Doridian/OpenBambuAPI)) plus
**direct observation of a real printer**; nothing is copied from another
implementation. Where the device and the docs disagree, the device wins.

Each fact is tagged:

- **[observed]** — confirmed on the hardware we have (an **A1 mini**, model code
  `N1`, firmware `01.07.02.00`, `hw_ver AP05`, an AMS-Lite-class unit attached),
  on 2026-06-13.
- **[spec]** — from OpenBambuAPI / Bambu's vendor data; not (yet) device-verified.

Reproduce captures with `tools/capture_pushall.py` / `tools/capture_get_version.py`
(stdlib only, read-only). Scrubbed fixtures live in `tests/fixtures/`.

## Transport

- **MQTT over TLS, port 8883.** Username `bblp`, password = the 8-digit LAN
  access code (also the FTPS and camera password). **[observed]**
- The printer's TLS cert is **self-signed X.509 *version 1*** (CN = serial,
  issuer `BBL CA`, RSA-2048/SHA-256). rustls/webpki reject v1 certs
  (`UnsupportedCertVersion`), so the client accepts any cert and skips handshake
  signature validation (OpenSSL `CERT_NONE` equivalent) — acceptable only for
  this LAN-direct, self-signed case. **[observed]**
- Topics: `device/{serial}/report` (printer → client) and
  `device/{serial}/request` (client → printer). **[observed]**
- **FTPS:** implicit TLS, port 990, same `bblp` + access code. **[spec]**
- **Camera:** A1/P1 use a proprietary JPEG stream on TCP 6000; X1/X1E/H2D use
  RTSP over TLS on 322. **[spec]**

## Access modes (decides what works)

| Printer setting | LAN MQTT :8883 | reads | control (writes) |
| --- | --- | --- | --- |
| LAN/Local Mode **OFF** | **connection refused** | ✗ | ✗ |
| LAN Mode ON, Developer Mode **OFF** | connects | ✓ | **silently dropped** (ACS) |
| LAN Mode ON **+ Developer Mode ON** | connects | ✓ | ✓ |

All three rows are **[observed]**. Note `pushing.pushall` is a *read* and works
whenever connected, so a working pushall does **not** prove control works.
Control requires **both** LAN Mode and Developer Mode.

## Discovery (SSDP)

The printer answers/announces over SSDP (multicast `239.255.255.250`, UDP ports
1990/2021). The `DevModel.bambu.com` header carries the model code; `USN`
carries the serial. **[observed]**

## Model codes

Vendor-canonical `model_id` (from `BambuStudio/resources/printers/<code>.json`),
which for the A1 family also matches the SSDP `DevModel`:

| Model | code |
| --- | --- |
| A1 mini | `N1` **[observed]** |
| A1 | `N2S` |
| P1P / P1S | `C11` / `C12` |
| X1 Carbon / X1 | `BL-P001` / `BL-P002` |
| X1E | `C13` |
| H2D | `O1D` |

`N1 = A1 mini` and `N2S = A1` — the once-"common" inverse is wrong. The legacy
SSDP strings `3DPrinter-X1-Carbon` / `3DPrinter-X1` normalise to `BL-P001` /
`BL-P002`. `BL-A001` is the **AMS** firmware code, *not* the A1 printer. **[spec]**
`hw_ver AP05` is shared across the A1 family (incl. the A1 mini), so it does not
distinguish A1 from A1 mini. **[observed]**

## Report (`push_status`)

A `pushall` request (`{"pushing":{"sequence_id":"0","command":"pushall"}}`) is
answered by a message whose `print.command == "push_status"` with ~64 fields.
The A1/P1 class then pushes **deltas** (only the changed fields); the client must
cache and merge. **[observed]**

Field notes **[observed]**:

- Fan speeds are **strings** (`cooling_fan_speed: "0"`); temperatures are floats.
- `hms` is an array of `{attr, code}` (empty = no active alerts).
- `lights_report` is an array (e.g. `[{node:"chamber_light", mode:"off"}]`).
- `chamber_temper` is reported (`5`) on the A1 but is **not a real sensor** —
  only X1/X1E/H2D have one. Surface it only for those models.
- `home_flag` is a bitfield of per-axis homed state; it changes during a home,
  so it (and `mc_print_sub_stage`) reflect ad-hoc motion that `gcode_state`
  doesn't.
- `gcode_state` tokens: `IDLE`, `PREPARE`, `RUNNING`, `PAUSE`, `FINISH`,
  `FAILED`, `SLICING`, `INIT`, `OFFLINE`.

### Two kinds of `sequence_id`

- The periodic `push_status` report carries the **printer's own** counter (not
  an echo of our request). **[observed]**
- A **command ACK** echoes the request's `sequence_id` and adds
  `result`/`reason`. **[observed]**

## Commands and verification

Request payloads are a single-key envelope keyed by **category**:

```json
{ "print": { "sequence_id": "1", "command": "gcode_line", "param": "G28" } }
```

Categories: `pushing` (pushall), `print` (pause/resume/stop/gcode_line/
project_file/…), `system` (ledctrl/…). `sequence_id` is a stringified counter.

**Verify-by-reread via the ACK** (the primary verify signal): after sending a
command with `sequence_id = N`, watch for a report under the **same category**
where `sequence_id == N`, then read `result`:

```
sent : { "print":  { "sequence_id":"1", "command":"gcode_line", "param":"G28" } }
ack  : { "print":  { "sequence_id":"1", "param":"G28", "result":"success", "reason":"success" } }
```

`system.ledctrl` is ACKed under `/system/...`, not `/print/...`. **[observed]**

A raw `gcode_line` (e.g. `G28`) executes but does **not** change
`gcode_state`/`stg_cur` — those track print *jobs*. Verify it via the ACK
(and, for a home, `home_flag`); raw coordinates are not in the report. **[observed]**

## HMS decode

`hms[]` entries are `{attr, code}` 32-bit ints. Format
`HMS_AAAA_BBBB_CCCC_DDDD` with `AAAA=attr>>16`, `BBBB=attr&0xFFFF`,
`CCCC=code>>16`, `DDDD=code&0xFFFF`; skip entries with a zero `attr` or `code`.
`severity = code>>16`; `module = (attr>>24)&0xFF` (0x03 MC, 0x05 mainboard,
0x07 AMS, 0x08 toolhead, 0x0C XCAM/LiDAR — X1-only). Severity *labels* are
unresolved (pybambu vs wiki conflict), so we expose the raw value. Code text
comes from Bambu's wiki, not a bundled table. **[spec]** (worked example
`HMS_0300_0100_0001_0007` confirmed against the wiki).

## Open questions

- Exact `print.project_file` field encodings for a LAN SD print (RE-derived;
  confirm by capturing a Bambu Studio → A1 mini session).
- Camera TCP:6000 handshake details on the A1; per-AMS-variant codes.
- `home_flag` bit layout (which bit = which axis).
