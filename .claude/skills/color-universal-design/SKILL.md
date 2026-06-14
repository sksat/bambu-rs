---
name: color-universal-design
description: >-
  Apply Color Universal Design (CUD) so colors work for everyone, including color-vision
  deficiency (~1 in 20 men; red-green is by far the most common). ALWAYS use this when
  choosing, designing, or reviewing colors in any UI, dashboard, chart, map, or
  visualization — especially where color carries meaning (status / error / success /
  warning, categories, severity, on/off). It gives a colorblind-safe palette (with hex),
  the WCAG contrast rules, and an audit checklist. Triggers on asks like "is this
  colorblind-safe?", "pick an accessible color palette", "review these status colors",
  "make this chart readable", "color universal design / CUD / カラーユニバーサルデザイン",
  "色覚", or picking accent/status colors even when accessibility isn't named. Do NOT use
  for non-color styling alone (layout, spacing, typography) or when no color conveys meaning.
metadata:
  type: reference
---

# Color Universal Design (CUD)

Design so information survives color-vision differences. CVD ("color blindness")
is normal human variation, not a defect: **~5% of men (1 in 20), ~0.2% of women**
have it; world prevalence ≈ 2.6%. **Red-green types dominate**, so treat red/green
as the riskiest axis.

| Type | aka | What | Men |
|---|---|---|---|
| **D型** Deutan | green-weak | most common | ~3.7% |
| **P型** Protan | red-weak (reds look dark) | | ~1.5% |
| **T型** Tritan | blue-weak (blue↔yellow confuse) | rare | ~0.7% |
| Achromat | full | grayscale only | <0.001% |

## The 3 principles

1. **Choose distinguishable colors** — lean on **luminance (lightness) difference**,
   not hue. Two colors at the same brightness can vanish for some viewers even if
   their hues look very different to you.
2. **Don't rely on color alone** — reinforce with shape (●▲■), position, line style
   (solid/dashed/dotted), a text label, an icon, or size.
3. **Make it explicit** — legends, color names, state words. Never make the viewer
   infer meaning from hue.

These don't fight aesthetics — a constrained, high-contrast palette usually looks
cleaner, not worse.

## A safe palette (Okabe-Ito)

Qualitative, colorblind-safe, widely used (Nature). Good default for categorical
data and UI accents.

| Name | Hex | | Name | Hex |
|---|---|---|---|---|
| Orange | `#E69F00` | | Blue | `#0072B2` |
| Sky Blue | `#56B4E9` | | Vermillion (red) | `#D55E00` |
| Bluish Green | `#009E73` | | Reddish Purple | `#CC79A7` |
| Yellow | `#F0E442` | | Black | `#000000` |

Alternatives for **small UI sets (3-5 colors)**:
- **Paul Tol high-contrast**: Gold `#DDAA33` / Rose `#BB5566` / Blue `#004488`
- **CUDO accent set (Japan CUD standard)**: Red `#FF4B00` / Orange `#F6AA00` /
  Green `#03AF7A` / Blue `#005AFF` / Sky Blue `#4DC4FF`

### Picking the few colors a UI needs

- **Base axis on orange × blue**, not red × green. If you need "green", push it to
  blue-green (`#009E73`); if you need "red", push it to orange/vermillion
  (`#D55E00` / `#FF4B00`) and raise its luminance.
- **`ok=green / err=red` is the classic trap.** If both sit at similar brightness
  they merge for red-green and grayscale viewers. **Force a luminance gap** (make
  one clearly lighter), AND pair each with an icon/word (`✓ ok` / `✕ error`).
- A single accent color used alone (e.g. an amber highlight) is usually fine; the
  risk appears when **multiple meaning-colors sit together** — separate those by
  luminance + a non-color cue.

## WCAG rules to meet

- **1.4.1 Use of Color (A)** — color must not be the *only* way info is conveyed.
- **1.4.3 Contrast (AA)** — text ≥ **4.5:1** (large text 18pt/14pt-bold ≥ **3:1**).
- **1.4.11 Non-text Contrast (AA)** — UI component boundaries, state, icons, and
  meaningful graph marks ≥ **3:1** against their background.

## Audit checklist

For every place color carries meaning, ask:

1. **Color-only?** Is there also text / icon / shape / position / pattern? If not, add one.
2. **Red-green-only?** Re-map to orange×blue or add a luminance gap + non-color cue.
3. **Contrast?** Text 4.5:1 (3:1 large); UI state/borders/icons 3:1.
4. **Same-hue collisions?** Two states sharing one hue (e.g. two ambers) → split by
   line style, icon, or shape.

Fix by adding the cheapest non-color cue first (a label or icon), then adjust hue
toward the safe palette, then widen luminance.

## Verify

- **Chrome DevTools → Rendering → "Emulate vision deficiencies"** (Deuteranopia +
  Protanopia covers ~the red-green ~5%; also Tritanopia, Achromatopsia). Toggle
  while looking at the real screen.
- **WebAIM Contrast Checker** — paste fg/bg hex, read AA/AAA.
- Spot-check a screenshot with a simulator (DaltonLens / Color Oracle; the Brettel
  1997 method is accurate — avoid old "Coblis V1" style sims).

Always re-verify after changes: emulate D/P type → measure contrast → confirm no
signal is color-only.

## Sources

- CUDO (カラーユニバーサルデザイン機構): https://cudo.jp/ · recommended set https://cudo.jp/?page_id=1565
- Okabe & Ito palette: https://siegal.bio.nyu.edu/color-palette/ · Paul Tol: https://personal.sron.nl/~pault/
- WCAG: 1.4.1 https://www.w3.org/WAI/WCAG22/Understanding/use-of-color.html · 1.4.3 https://www.w3.org/WAI/WCAG22/Understanding/contrast-minimum · 1.4.11 https://www.w3.org/WAI/WCAG21/Understanding/non-text-contrast.html
- WebAIM contrast: https://webaim.org/resources/contrastchecker/ · Chrome CVD emulation: https://developer.chrome.com/blog/cvd/
