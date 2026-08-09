# tools/doom — DOOM, played through a 3D printer's LAN interface

`bambu serve --emulate` presents the printer's own interfaces to a LAN client:
MQTT on 8883, and a chamber camera on 6000 that shows whatever frames it is
given. It also sees every control command a client sends before deciding what to
do with it. So a client — Bambu Studio, say — can be shown a game in its
liveview and have its movement panel play it.

This directory builds the other end: a DOOM that draws to a pipe instead of a
screen and reads its keyboard from another one.

```
Bambu Studio ──MQTT──▶ bambu serve --emulate-doom ──stdin──▶ ┐
             ◀─cam───                              ◀─stdout─ └ bambu-doom-engine
```

Nothing reaches a printer. `--emulate-doom` requires `--fake`, so the thing
behind the relay is the synthetic printer — a few hundred lines of JSON that has
never opened a socket — and a command taken for the game is structurally
incapable of also being forwarded (`ControlPolicy::Intercept`).

## Build it

Two halves, and the Rust one is **not in a default build**. `--emulate-doom`
exists only with the `doom` feature, which is off because a printer CLI should
not carry a game's plumbing — or the flag — unless somebody asked:

```bash
cargo build --features doom        # or: cargo run --features doom -- serve …
```

Then the engine:

```bash
./build.sh
```

Fetches [doomgeneric](https://github.com/ozkl/doomgeneric) (GPL-2.0, pinned to a
commit), `stb_image_write.h` (public domain) and the **shareware** `doom1.wad`
(freely redistributable; md5-checked, because a mirror answering an HTML error
page with a 200 is the usual failure). Everything lands in `./build/`, which is
not tracked: bambu-rs is MIT and its build must not need a C toolchain, so none
of this is vendored.

Needs `cc`, `make`, `git` and `curl`. If you own a retail WAD, point the engine
at it with `-iwad` instead.

Check the engine on its own before involving a printer:

```bash
./build/bambu-doom-engine -raw -iwad ./build/doom1.wad -warp 1 1 | ffplay -f mjpeg -
```

## Play it

```bash
bambu serve --fake --emulate \
  --serial DOOM00000000001 --access-code 12345678 \
  --emulate-host 0.0.0.0 --emulate-advertise <this machine's LAN IP> \
  --emulate-doom \
  --emulate-doom-engine "$PWD/build/bambu-doom-engine" \
  --emulate-doom-arg -workdir --emulate-doom-arg "$PWD/build/saves" \
  --emulate-doom-arg -iwad --emulate-doom-arg "$PWD/build/doom1.wad" \
  --emulate-doom-arg -warp --emulate-doom-arg 1 --emulate-doom-arg 1 \
  --emulate-doom-arg -maxfps --emulate-doom-arg 20
```

`-workdir` is worth passing. DOOM keeps `.default.cfg` and `.savegame/` in its
working directory, and the engine inherits `bambu serve`'s — so without it the
game drops those into whatever directory you started the relay from. Everything
after it that is a relative path is relative to *that* directory, which is why
the WAD above is absolute.

Then add a printer in Bambu Studio by IP, with that serial and access code.
(Studio verifies certificates: `tools/trust_relay_in_studio.sh` is what gets a
relay past that.) `--emulate-host 127.0.0.1` and no `--emulate-advertise` is
enough if the client is on this machine.

No FTP server is started in this mode — there is nothing to print — so no
privileged port and no `setcap`.

## The controls

| what you press | what the client sends | DOOM |
| --- | --- | --- |
| Jog **Y+** / **Y−** | `G1 Y±d` | forward / back |
| Jog **X+** / **X−** | `G1 X±d` | turn right / left |
| Jog **Z+** / **Z−** | `G1 Z±d` | strafe right / left |
| **Home** | `G28` | fire |
| **Chamber light** (either way) | `system.ledctrl` | use — doors, switches |
| **Extrude / retract** | `G1 E±d` | fire / use |
| **Print speed 1–4** | `print.print_speed` | weapon slots 1–4 |
| **Pause / resume** | `print.pause` | pause |
| **Stop** | `print.stop` | the menu key |

A jog's *distance* is how long the key is held — 25 ms per millimetre — so
Studio's 1 mm button is a nudge and its 10 mm button is a stride. The mapping
itself is `src/core/doom.rs`, which is pure and unit-tested: G-code in, key
presses out.

## The readout

The game comes back the other way too, and not only as pictures:

| DOOM | the printer says |
| --- | --- |
| health 100 | nozzle **220 °C** — a plausible PLA temperature |
| health 50 | nozzle 122.5 °C |
| health 0 | nozzle 25 °C — a machine that has gone cold |
| armour | the bed, on the same scale: 0 → 25 °C, 100 → 60 °C |

The *targets* are set to the full-health values (220 and 60) rather than to
anything the game is doing, so a client drawing current against target is
drawing a health bar without having been told it is one. Above 100 health a
soulsphere runs the nozzle hot, capped at the 300 °C ceiling `core::safety`
puts on a nozzle — a client is never shown a temperature this crate would
refuse to command.

Ammo deliberately goes nowhere. There is no field on a printer's face where a
count of bullets reads as anything but a wrong number, and the game already
draws it.

The reading moves at the printer's own report rate, so a hit shows up on the
next status report rather than instantly.

Anything with no button behind it (`project_file`, a temperature command) is
consumed and does nothing. The relay says so on stderr, one line per press —
which is the only way to tell a mapping that missed from a button that never
arrived:

```
emulate-doom: print.gcode_line "G91\nG1 Y10 F3000\nG90" — forward for 250ms
emulate-doom: print.project_file — nothing bound
```

One visible consequence of consuming commands: a client that checks a command
*took effect* will call it unverified, because the printer's report never
changes. `bambu speed sport` through this exits 6 for exactly that reason — the
level was pressed as weapon 3 and `spd_lvl` stayed where it was. That is the
honest answer; making the report agree would mean sending the command upstream,
which is the one thing this mode may not do.

## The engine protocol

`--emulate-doom-engine` can be any program that speaks this; DOOM is just the
first one.

- **stdout**: frames, in the printer's own camera framing — a 16-byte header
  (little-endian `u32` length, then `0`, `1`, `0`) followed by a baseline
  4:2:0 JPEG of at least 1000 bytes. That is exactly what the relay serves on
  port 6000, so a frame is never re-encoded on its way to a client.
  With `-raw` the headers are dropped and the output is plain concatenated
  JPEGs, which `ffplay -f mjpeg -` will play.
- **stdout, also**: status records, saying how the player is doing. Same
  16-byte header, but the length word is **zero** — which no frame may be, so a
  reader that knows only about frames refuses one outright instead of handing
  four bytes of binary to a JPEG decoder. The magic goes in the word a frame
  leaves at zero, where it is readable in a hexdump:

  ```text
  0..4    0                     not a frame length, and cannot become one
  4..8    "DOOM"                the magic
  8..12   payload length        4 today
  12..16  0
  payload health int16le, armour int16le    negative = no player
  ```

  Sent only when a number changes, and only inside a level — at the title
  screen there is no player, and a health bar that lied while the game was not
  running would be worse than none. A payload longer or shorter than this one
  is read as far as it goes, so the record can grow a field later without a
  flag day. The Rust side is `status_header` / `parse_vitals` in
  `src/core/doom.rs`.
- **stdin**: key events, two bytes each — `[pressed (0|1), DOOM key code]`.
  That is `DG_GetKey`'s own shape. EOF means the relay has gone; exit.
- **stderr**: everything else. A single stray byte on stdout is a corrupt frame.

`doomgeneric_bambu.c` is that program's platform layer: it renders at 1280x800
(DOOM's 320x200 at a whole 4x), letterboxes into 1280x720 — the size Bambu
Studio is known to display — and encodes with `stb_image_write` at quality 70,
which is below the 90 where stb switches away from 4:2:0.

Its own options, before DOOM's: `-raw`, `-quality 1..89`, `-maxfps N`,
`-workdir <dir>`.

## Licences

doomgeneric and the DOOM source it carries are GPL-2.0; `stb_image_write.h` is
public domain / MIT. Neither is redistributed here — `build.sh` fetches them at
build time. `doomgeneric_bambu.c` and the Makefile are part of bambu-rs and MIT,
and are only ever compiled *with* GPL sources by you, on your machine.

**The binary that comes out is GPL-2.0**, whatever the licence on the file you
started from: linking MIT code into a GPL work makes the combined work GPL, and
`bambu-doom-engine` is a combined work. It is yours to run and yours to keep;
handing it to somebody else means handing over the corresponding source under
the GPL too. Nothing here builds it for you or ships it, which is why the crate
stays MIT — `serve --emulate-doom` spawns a program it does not link against,
and finds nothing at all unless you have built one.
