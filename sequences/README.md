# Control sequences

`.gcode` macros for accessories — a plate changer, a park position, whatever is
bolted to a particular machine. Run them with `bambu gcode --sequence <name>`
once they are named in that printer's profile.

## These are per-machine, not per-model

The tool deliberately has no built-in knowledge of any accessory: a macro's
coordinates depend on what is bolted to **that unit**, so naming a `.gcode` file
in the profile is all the support an accessory needs
(`src/config.rs`, `Profile::sequences`).

What is here is therefore a **reference**, not a default. Nothing loads it
automatically. Read it, check the numbers against your own machine, and run it
the first time with the bed clear and a hand near the power switch.

## Installing one

Copy it where your config can reach it and name it in the profile. Relative
paths resolve against the **config file's directory**, never the cwd:

```bash
mkdir -p ~/.config/bambu-rs/sequences
cp sequences/a1mini-swapmod-swap.gcode ~/.config/bambu-rs/sequences/swap.gcode
cp sequences/a1mini-swapmod-load.gcode ~/.config/bambu-rs/sequences/load.gcode
```

```toml
[printers.<name>.sequences]
swap = "sequences/swap.gcode"
load = "sequences/load.gcode"
```

Then, always with `--wait` — see `docs/plate-changer.md` for why that word is
load-bearing:

```bash
bambu gcode --sequence swap --wait --confirm
```

## What is here

| file | machine | accessory | what |
|---|---|---|---|
| `a1mini-swapmod-load.gcode` | Bambu Lab A1 mini | Swapmod plate changer | load a plate, eject nothing — for the **first** plate of a run |
| `a1mini-swapmod-swap.gcode` | Bambu Lab A1 mini | Swapmod plate changer | eject the current plate and load the next |

Load first, swap thereafter: the swap assumes there is already a plate on the
bed and ejects it. `docs/plate-changer.md` has the rest.

The Swapmod's **models are not** in this repository and should not be: they are
the vendor's, and you need to have bought them. This is the motion macro only —
coordinates lifted from the vendor's own self-test project, with the provenance
recorded in the file's header.
