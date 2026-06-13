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

### Print stages (`stg_cur`)

`stg_cur` is a small integer naming the printer's **current activity** — the one
report field that tracks ad-hoc motion (homing, bed leveling, calibration
sweeps, filament changes) independently of `gcode_state`. The id→name table is
spec-derived (OpenBambuAPI); names map to `src/core/stage.rs`.

**Two values mean "no special activity": `0` and `255` (`0xFF`).** A real A1
mini reports `stg_cur=0` both while laying down filament *and while idle* (the
idle `pushall` fixture has `gcode_state=IDLE`, `stg_cur=0`), and reports
`stg_cur=255` after a job ends (`gcode_state=FINISH`, `stg_cur=255` — the
calibration above finished at exactly this). So read `stg_cur` **together with
`gcode_state`**: neither sentinel tells you whether the printer is busy;
`gcode_state` does. **[observed]**

Stages confirmed on the A1 mini during a `bed_level + vibration` calibration
(`bambu calibrate --bed-level --vibration --confirm`), in order: **[observed]**

| `stg_cur` | name | seen during |
|---|---|---|
| 14 | `cleaning_nozzle_tip` | nozzle heated to ~168°C, then cooled |
| 3  | `sweeping_xy_mech_mode` | vibration-compensation XY sweep |
| 1  | `auto_bed_leveling` | bed-level probing (~minutes), nozzle held ~140°C |

This is concrete evidence that **motion/activity *is* observable** in the LAN
report: `stg_cur` transitions through these stages while `gcode_state` stays
`RUNNING` and `mc_percent` advances. The remaining names in the table are the
spec's claim, not yet device-verified.

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

Categories: `pushing` (pushall), `info` (get_version), `print` (pause/resume/
stop/gcode_line/project_file/…), `system` (ledctrl/reboot/…), `camera`
(ipcam_timelapse/…). `sequence_id` is a stringified counter.

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

**The ACK is necessary but not sufficient.** `result == "success"` only means the
command was *accepted*. A real fault can still follow: observed on the A1 mini, a
`project_file` ACKed `success` yet the print never started — `gcode_state` stayed
`IDLE` and `print_error` became `0x0500C010` (a failing SD card). So for commands
with an observable effect, also confirm the effect in the report and watch for a
**new** `print_error` (capture the baseline before sending). `subtask_name` is
**not** a reliable "started" signal — it updated to the new job even though the
print never began. **[observed]** (see `src/core/verify.rs`)

## Printing a job: on-printer file layout + `project_file`

The printer's FTP root has, among others, two dirs that matter for printing
**[observed]**:

- `/cache` — Bambu Studio's upload area. Holds project `.3mf` *and* extracted
  per-plate `*_plate_1.gcode` files.
- `/model` — the user-visible model library: proper sliced `*.gcode.3mf`.

A sliced `.gcode.3mf` is a zip containing `Metadata/plate_N.gcode`,
`Metadata/plate_N.gcode.md5`, `Metadata/plate_N.json`, `3D/3dmodel.model`, plate
PNGs, `project_settings.config`, etc. **[observed]**

To start one: `print.project_file` with `url = ftp:///<dir>/<file>.gcode.3mf`,
`param = Metadata/plate_N.gcode`, and the LAN ids set to `"0"`. A real print was
started this way from `/model` with a **space-containing** url and `md5 = ""`
(skip) — both accepted: `ftp:///model/Bed scraper by JernejP.gcode.3mf`.
**[observed]**

`print.gcode_file` (raw on-printer `.gcode`, e.g. a `*_plate_1.gcode`) was
**rejected** on the A1 mini (`result: "fail", reason: "error string"`) across
every param form tried (`/cache/…`, basename, short no-space name, `+url`).
**But this test was confounded by the failing SD card** (all file reads were
unreliable at the time), so whether `gcode_file` is genuinely unsupported on the
A1 is **inconclusive** — retest on a healthy SD card before concluding. We do
**not** register a "gcode_file unsupported" quirk. **[observed, inconclusive]**

## Version inventory (`info.get_version`)

`{"info":{"sequence_id":"N","command":"get_version"}}` is answered (under
`/info`) with a `module[]` array — one entry per hardware/software component,
each `{name, hw_ver, sw_ver, product_name, …}`. The **`ota`** module's `sw_ver`
is the printer's user-facing **firmware** version (what the capability registry
keys on). On our A1 mini: `ota` sw_ver `01.07.02.00` (product_name `Bambu Lab A1
mini`), `esp32` hw `AP05`, and `ams_f1/0` hw `AMS_F102` product_name `AMS Lite`.
`bambu info` reads this and resolves `(model, firmware)` capabilities.
**[observed]** (fixture `tests/fixtures/get_version-a1mini.json`).

## Camera / timelapse settings (`ipcam` node, `camera.ipcam_timelapse`)

The report's `print.ipcam` object carries the camera/timelapse settings, e.g.
`{"timelapse":"disable","ipcam_record":"enable","resolution":"1080p",
"tutk_server":"disable","mode_bits":3}`. The `timelapse` field is the printer's
**actual** timelapse setting. **[observed]**

`camera.ipcam_timelapse` toggles it:
`{"camera":{"command":"ipcam_timelapse","control":"enable","sequence_id":"N"}}`
(`control` = `enable`/`disable`). Verify like the chamber light — read back
`ipcam.timelapse`, not just the ACK (a faulty/absent camera may ACK but not take
effect). **[spec]** — shape is OpenBambuAPI-derived; **not device-confirmed**,
because this unit's built-in camera is hardware-dead (front-cover FPC). Recorded
videos land in FTPS `/timelapse`. The "smooth/cinematic" per-layer-park look is
produced by the **slicer's** timelapse gcode, not by a printer command.

> Because the built-in camera can't capture here, `bambu timelapse capture`
> drives an **external** camera instead: it watches the LAN report and runs a
> user-supplied capture command (argv, no shell) on each new `layer_num`. Pure
> orchestration — the printer's own layer events are the trigger.

## Errors: `print_error` is a separate channel from HMS

A device fault can surface via **`print_error`** (a single 32-bit code under
`/print`) entirely independently of `hms[]`. A failing A1 mini SD card reported
`print_error = 0x0500C010` (module `0x05` = mainboard/storage) while `hms` was
`[]`. So a status view must surface `print_error` in its own right; rendered as
`0x{:08X}`. `sdcard: true` means a card is *present*, not that it is healthy.
We don't bundle a `print_error`→text table (same rationale as HMS). **[observed]**

## Reboot (undocumented but accepted)

`{"system":{"command":"reboot","sequence_id":"N"}}` is **not** in the
OpenBambuAPI spec, but the A1 mini **accepts and acts on it**: after sending,
MQTT:8883 (and every other port) dropped and the printer restarted, returning
~1–2 min later. No ACK is observed — the connection just drops — so verify by
reconnection, not by an ACK. **[observed]**

Two caveats, both observed when we used it to recover a wedged SD mount:

- The printer may rejoin via DHCP on a **different IP** than before. A static
  lease / DHCP reservation avoids this.
- A reboot **clears the stale `print_error`** (`0x0500C010`) and idle reads work
  again — but on a genuinely failing SD card the fault **recurs on the next
  large read**: a print attempt set `0x0500C010` again and never started. So
  reboot is a *transient* clear, not a fix; a recurring `0x0500C010` means the
  SD card needs reseating/reformatting/replacing.

## FTP control/data both usable

Beyond `LIST` + `STOR`, the A1 mini's implicit-FTPS server also supports
`RNFR`/`RNTO` (rename, control-channel only) and `RETR` (download). `RETR` backs
`bambu file download` / `bambu timelapse get` — verified by downloading a real
`*.gcode.3mf` byte-identical (valid zip, plate gcode intact). `DELE` backs
`bambu file rm` (standard, not yet device-exercised here). **[observed]**

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

- Is `print.gcode_file` genuinely unsupported on the A1 mini, or did the failing
  SD card cause the rejection? Retest on a healthy SD card.
- Does `project_file` require a matching `md5` on a healthy SD card, or is `""`
  (skip) always accepted? (We only got to confirm the ACK; the SD fault stopped
  the print before it ran.)
- Camera TCP:6000 handshake details on the A1; per-AMS-variant codes.
- `home_flag` bit layout (which bit = which axis).
