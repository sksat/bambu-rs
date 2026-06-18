---
name: bambu-timelapse
description: Record a smooth per-layer timelapse of a Bambu A1 mini print — the "object grows, nozzle parked out of the way" look — using the printer's NATIVE Smooth timelapse (firmware-safe park + Z-lift), captured by `bambu serve`'s in-process grabber from one or more EXTERNAL cameras (the A1's built-in camera is dead on this unit). Use this whenever the user wants a timelapse, a "smooth"/"park-synced" timelapse, the head to step aside for the camera each layer, a multi-camera/multi-angle capture, or asks why a timelapse print detached / looks scraped / came out as a flat plate. Covers slicing for timelapse, arming it at print start, the serve capture API, and assembling the video. Do NOT use it for plain slicing with no timelapse (use bambu-slice), a one-off camera snapshot, or live print control (pause/resume/stop).
metadata:
  type: reference
---

# Smooth timelapse on the Bambu A1 mini (external camera + `bambu serve`)

All verified on a real A1 mini this project drives over LAN. The built-in
camera (TCP:6000) is **dead on this unit** — every timelapse here is an external
camera (an ATOM Cam and a `ustreamer` host) grabbed by `bambu serve` in-process
off the printer's MQTT layer feed (`POST /api/timelapse/start`). The hard part
isn't the capture — it's making the head **park cleanly each layer** so the frame
shows just the object, without wrecking the print.

## Use the printer's NATIVE Smooth timelapse — do NOT hand-roll the park

The A1's own `time_lapse_gcode` (in `Bambu Lab A1 mini 0.4 nozzle.json`) already
does the right thing, and it's what to use:

```gcode
M1002 judge_flag timelapse_record_flag
M622 J1                                   ; runs ONLY if timelapse armed at start
G1 Z{max_layer_z + 0.4}                   ; ABSOLUTE-Z lift — clears the print
G1 X0 Y{first_layer_center_no_wipe_tower[1]} F18000
G1 X-13.0 F3000                           ; park off the left X edge
M1004 S5 P1 ; external shutter            ; ← native external-camera shutter
M400 P300                                 ; short dwell
M971 S11 C11 O0
G1 X0 F18000                              ; return
M623
```

Why this is the answer:
- **Absolute-Z lift (`G1 Z{max_layer_z + 0.4}`)** lifts the nozzle clear before it
  travels to the park. Skip the lift and the nozzle drags across the top of the
  part every layer — see the failure below.
- It's evaluated by the **firmware** (machine gcode), so `{max_layer_z}` resolves
  correctly. This is the firmware-safe way to get a Z-hop — see the warning about
  doing it yourself.
- The A1 is an **I3-structure (bedslinger) machine**: this version needs **no prime
  tower / wipe tower** (lift + quick park + 300 ms is enough). The X1/P1 "Smooth"
  variant parks at the poop chute and *does* need a prime tower — don't copy that.

### Two things you MUST do

1. **Slice with `--enable-timelapse` + a brim.** `orca-slicer --enable-timelapse`
   is the real switch that inlines the A1's per-layer `time_lapse_gcode` (the
   `; SKIPTYPE: timelapse` block) at every layer. The block often appears without
   it too, but the process **`timelapse_type` (0/1) is just a UI label and does NOT
   gate it** — so pass the flag explicitly rather than relying on a default. Do
   **NOT** set `timelapse_type=1` on the A1: that's the X1/P1 wipe-tower variant; it
   parks at a tower position off the 180 mm bed and the slice dies with "found gcode
   in unprintable area". Add a **brim** (per-layer parking is hard on tiny
   first-layer footprints). The bundled helper does all this — flattens the profile
   inheritance (see the bambu-slice skill for why that matters), passes
   `--enable-timelapse`, adds the brim, and **fails loudly if the timelapse block
   isn't in the gcode**:
   ```bash
   scripts/slice_smooth.py model.stl out.gcode.3mf [scale] ["Bambu PETG ... @BBL A1M"]
   # prints e.g. "... timelapse_blocks=61 external_shutter=61 brim=True"  ← blocks must be >0
   ```

2. **Arm it at print start with `timelapse: true`.** The whole block is gated on
   `timelapse_record_flag` (the `M622 J1 … M623`). If the print starts with the
   flag off, the printer **skips the lift+park entirely and the nozzle scrapes the
   print** — a known OrcaSlicer behaviour, and exactly our verified detachment.
   The serve job-start carries the flag:
   ```bash
   curl -X POST "$B/api/job/start" -H 'content-type: application/json' \
     -d '{"file":"/out.gcode.3mf","plate":1,"use_ams":true,"ams_map":[2],"timelapse":true,"confirm":true}'
   ```
   (`bambu serve` must be the build with the `timelapse` field on `/api/job/start`.)

## Capture from `bambu serve` (one or many cameras)

The capture is in-process off the MQTT layer feed — no second printer connection,
low latency — and writes frames to `captures/<epoch>_<name>/<cam-id>/`.

**Smooth fires a BURST per layer, not a single frame.** The native park reaches the
far-left X-min only ~0.4–1.2 s *after* `layer_num` increments (and holds it
~300 ms), so a lone grab at the layer edge catches the head still over the print
(device-verified: 0/241 frames parked). So each layer grabs at several offsets
after the edge — default `400,600,800,1000,1200` ms — saved as
`frame_<n>_layer_<L>_t<offset>.jpg`. One offset lands the parked, object-only shot.

```bash
# register the external cameras once (runtime only — never commit these URLs)
curl -X POST "$B/api/cameras/config" -H 'content-type: application/json' \
  -d '{"external":[{"label":"atom cam","url":"http://<ATOM_IP>/cgi-bin/get_jpeg.cgi"},
                   {"label":"ustreamer","url":"http://<USTREAMER_HOST>/snapshot","stream_url":"http://<USTREAMER_HOST>/stream"}]}'

# multi-camera, per-layer burst from BOTH angles at once
curl -X POST "$B/api/timelapse/start" -H 'content-type: application/json' \
  -d '{"cameras":["ext-0","ext-1"],"every":1}'
# narrow/shift the burst for calibration (ms after the edge; <=16 offsets, <=10000):
#   -d '{"cameras":["ext-0"],"burst_offsets_ms":[700,800,900]}'
curl "$B/api/timelapse"          # {running, cameras, frames, failures, current_layer}
curl -X POST "$B/api/timelapse/stop"
```

Start the capture right after the job (it waits through preheat/calibration and
stops at FINISH). `frames` is the **total** written = layers × offsets × cameras.
Re-register the cameras after any serve restart (config is in-memory).

## Assemble the video

The burst leaves several frames per layer, so pick the offset whose `_tNNNN` frames
show the head **parked at the far-left** — eyeball a few of one mid-print layer
(`frame_*_layer_00050_t*.jpg`) — then glob just that offset:

```bash
cd captures/<run>/<cam-id>
ffmpeg -y -framerate 12 -pattern_type glob -i '*_t0800.jpg' \
  -vf "scale='min(1280,iw)':-2,format=yuv420p" -c:v libx264 -crf 23 timelapse.mp4
```
If no offset reliably parks, shift/widen `burst_offsets_ms` and reprint — the tags
are the calibration.

## VERIFIED failures — don't repeat these

- **Custom `layer_change_gcode` with a *relative* Z-hop (`G91` / `G1 Z0.3` / `G90`)
  → a FLAT PLATE.** The A1 firmware mishandles relative Z in layer-change gcode and
  drops every layer to the hop height. This is why you must NOT hand-roll the park;
  use the native `time_lapse_gcode` (absolute `{max_layer_z}`) above. OrcaSlicer
  also rejects `max_layer_z`/`first_layer_center_no_wipe_tower` if you put them in a
  *custom* `layer_change_gcode` — another reason to stay on the native path.
- **Custom park with NO Z-hop → the part detaches mid-print.** On a 12 mm cube the
  nozzle, travelling to the X-13 park at layer height each layer, scraped/snagged
  the top; combined with extra per-layer cooling (the park dwell) weakening the bed
  bond, the part popped loose around layer 40 and got dragged off. The native lift
  fixes the scrape; a **brim** (and PETG's good textured-PEI grip) fixes adhesion.
- **A long un-retracted dwell (~2.5 s) clogged the nozzle** within ~50 layers
  (heat-creep + ooze). The native 300 ms dwell avoids this; don't lengthen it.
- **Built-in camera reboot doesn't fix it** on this unit (tried twice). Treat the
  built-in camera as gone; use external cameras.

## Gotchas
- **Aim the external camera at the bed.** Our ATOM Cam sat too high (bed off-frame)
  so the early footage missed the first layers — check framing before a real run.
- Per-layer parking is **proportionally brutal on tiny parts** (the park overhead
  dwarfs a short layer). For a clean result prefer a larger/taller model, a brim,
  and a clean plate; the native 300 ms dwell is already minimal.
- The whole "smooth" path is **gated on starting with `timelapse:true`** — if a
  run comes out scraped/detached, check that first.
