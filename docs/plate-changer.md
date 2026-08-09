# Driving a plate changer

Notes from running a **Swapmod** changer on a **Bambu Lab A1 mini**. The
mechanism is generic — a macro that moves the toolhead, run before a print —
so most of this applies to any accessory driven the same way.

Everything measured is marked as such. `sequences/a1mini-swapmod-swap.gcode` is
the macro these notes describe.

## The changer has no electronics

The toolhead drives it. The trigger is the **Z axis dropping 186 → 180 at X=188
and again at X=170**; the rest of the macro is positioning, and the `G4 S3`
dwells let the plate settle rather than padding the runtime.

Which is why the coordinates are per-machine and the tool has no built-in
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
