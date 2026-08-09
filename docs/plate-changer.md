# Driving a plate changer

Notes from running a **Swapmod** changer on a **Bambu Lab A1 mini**. The
mechanism is generic — a macro that moves the toolhead, run before a print —
so most of this applies to any accessory driven the same way.

Everything measured is marked as such, with the date it was measured. The two
macros are in `sequences/`.

## Two sequences, not one

| | what | trigger | verified |
|---|---|---|---|
| `a1mini-swapmod-load.gcode` | load a plate, eject nothing | **not used** | 2026-08-06 |
| `a1mini-swapmod-swap.gcode` | eject the current plate, load the next | used | 2026-08-07 |

The **load** one is for the first plate of a run: the swap assumes a plate is
already on the bed and ejects it. It is also the gentler of the two — the head
parks out of the way at X=-10 / Z=30 and stays there, and the bed's Y travel
alone carries the plate in. About 40 s.

Both come from the vendor's self-test project, which contains eleven blocks: one
"plate load only", then ten swap cycles. The shipped swap is the **first** of
those ten; the later nine are the same motion with `G4 S1` and fewer dwells, and
the self-test completes all ten, so the longer dwells are the cautious setting
rather than a requirement.

## The changer has no electronics

The toolhead drives it, and the trigger is a **rope**, not a rod. It is pulled
by the **Z axis dropping 186 → 180 at X=188 and again at X=170**; the rest of
the macro is positioning. `G0 Y150 F200` is deliberately slow — that is where
the load is taken.

Per the kit's own guidance, a **slack rope is the failure to look for**: the
ejector lifter then does not rise far enough to catch the plate's hook. The
slack shows as a gap under the trigger glider; take it up until the gap closes.

This is also why the coordinates are per-machine and the tool has no built-in
knowledge of any accessory: naming a `.gcode` file in the profile is the whole
integration (`Profile::sequences`).

## The part that will bite you: a print does not wait

**Measured on the machine**, and the reason [`core::settle`](../src/core/settle.rs)
exists at all:

| | |
|---|---|
| last `gcode_line` acknowledgement | **~1 s** in |
| machine still moving for | **~51 s** after that |
| does `project_file` queue behind `gcode_line`? | **no** |

So the obvious sequence — send the swap, watch every line get acknowledged,
start the print — starts the print into a changer that is still moving. An
acknowledgement means *received*, never *finished*.

### How completion is actually observed

Two commands, because neither alone is enough:

1. **`M400`** drains the motion planner. Without it a following line executes
   **34 s into a 51 s motion**. It is not exotic here — the A1 mini's own machine
   profile uses `M400` in `machine_start_gcode`, `machine_end_gcode`,
   `time_lapse_gcode` and `change_filament_gcode`, and an ordinary sliced plate
   from this printer contained 172 of them.
2. **`M1002 gcode_claim_action : N`** sets `stg_cur` in the status report and
   does nothing physical. Sent after `M400`, seeing `stg_cur == N` come back is
   the printer saying it reached the end of the queue.

`N` has to be **out of band**: vendor gcode claims values up to **75**, so the
sentinels are `200..=205` and rotate between runs — the printer echoes whatever
it is given, so a stale report would otherwise read as a fresh completion.

> `stg_cur` is not limited to 0–35. That is only the range the status decoder
> has *names* for, not what the printer accepts.

## Running the macro by hand

```bash
bambu gcode --sequence swap --wait --confirm      # named in the profile
bambu gcode --from-file ./swap.gcode --wait --confirm
```

`--wait` is the load-bearing word: without it the command returns at the last
acknowledgement, i.e. while the machine is still moving, and "it succeeded"
tells you nothing about whether the plate moved. `--wait-timeout` defaults to
600 s.

`--sequence` needs a **profile** — sequences are per printer. With an env-only
connection (`BAMBU_IP` and friends, no saved profile) use `--from-file`; the CLI
says so rather than silently doing nothing.

## As a pre-print hook

The swap runs at the **start of the next print**, not the end of the previous
one. That gives continuous printing with no post-print machinery, and leaves the
finished part on the bed until you ask for the next one.

```toml
[printers.a1mini.sequences]
swap = "sequences/swap.gcode"      # relative to the CONFIG FILE's directory

[printers.a1mini.hooks]
pre_print = "swap"
pre_print_timeout_secs = 600       # optional; 600 is the default
```

`hooks.pre_print` is a **profile** field. Driving the CLI from environment
variables alone means there is no profile for it to live on and **the hook can
never fire** — nothing warns you, so check `config show`.

```bash
bambu job start --file /m.gcode.3mf --plate 1 --dry-run   # the plan names the hook
bambu job start --file /m.gcode.3mf --plate 1 --confirm   # swap, then print
bambu job start ... --no-hooks --confirm                  # skip it (plate already right)
```

The dry run **discloses the hook**: a preview that omitted hardware motion would
under-report what a confirmed start does. If the motion is not observed to
finish, the print is **not started** — a failed swap leaves a stale plate, which
is the better of the two failures.

## Several prints back to back

There is no queue, deliberately. Sequencing is an outer loop:

```bash
for f in a b c; do
  bambu job start --file "/$f.gcode.3mf" --plate 1 --confirm --watch || break
done
```

`--watch` blocks until the job completes and exits non-zero on a device error or
a `FAILED` state, so a bad plate stops the batch. (`--watch-timeout` defaults to
6 h. There is no `bambu watch` subcommand; `status --watch` deliberately does
*not* stop at completion.)

## What a full cycle looked like

Run by hand on 2026-08-07, two hooked plates in the magazine:

```
21:55 → 22:36   hook bar #1   (66 layers, PETG, 9.58 g, 41 min)
22:37           swap: all 32 commands sent, every one verified   (~1 min)
22:38 → 23:19   hook bar #2   completed normally
```

The eject is confirmed by the second print rather than by watching: both bars
print at the same coordinates, so if #1 were still on the bed the nozzle would
have hit it. #2 running to completion means the bed was empty — the plate had
been swapped.

### Calibration is not needed per plate change

It is already in the start command — `bambu job start --dry-run` shows it:

```
bed_leveling   = True     # re-meshes the bed, absorbing plate-to-plate thickness
flow_cali      = True
vibration_cali = True
```

The separate `bambu calibrate` (motor noise, resonance) measures the **machine**,
not the plate. Re-run it when the machine changes — a part swapped, the Swapmod
installed, a new noise, an axis HMS — not between plates.

## Operational notes

- **The PTFE tube works loose.** It came out twice during swap runs — the
  full-stroke Y moves stress it. The symptom is `print_error 0x12008006`
  (filament feed failure) at layer 0. Reseat it and `bambu job resume
  --confirm`; the print recovers (`active_tray` 255 → 0). Worth checking before
  an unattended batch.
- **One MQTT connection at a time.** A second concurrent client makes the A1
  mutually disconnect. With `bambu serve` running, go through it rather than
  opening a second connection — `serve --emulate` relays every client over the
  single link.
- **The printer must be idle** and the bed clear of anything you want to keep.
- **`--dry-run` first**, especially with a hook configured: it is the only
  preview that says the swap is about to run.
- **`FTP error: [550]` or an empty `md5=` on a dry run**, typically just after a
  calibration: pass `--upload` and the local bytes are verified directly instead
  of reading the file back off the printer.

### Error codes seen while running this

| code | means | what it actually was |
|---|---|---|
| `0x03008019` | no plate detected | the plate was **seated crooked** |
| `0x12008006/07/16` | filament feed failure | the **PTFE tube had come out** |
| `0x03008015` | no filament source | a wrong `--ams-map` |
| `0x0300400C` | task cancelled | a manual stop (normal) |
| `0x0300800B` | cutter jam | unexplained — see below |

`bambu status` prints a lookup URL for anything not in that list.

### Open, and worth knowing before an unattended run

- **`HMS_0300_1900_0002_0002`** (severity 2) is outstanding: *"the eddy current
  sensor of the Y axis is not sensitive enough; remove foreign objects from the
  Y-axis linear rail."* The Y axis is the one the changer drives the plate along,
  so debris or interference there is worth checking first.
- **`0x0300800B` (cutter jam)** happened twice right after a self-test and has
  never reproduced during ordinary printing. The head only reaches X=188 during
  a swap, which is the obvious suspect and is not confirmed.
