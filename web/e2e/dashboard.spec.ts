import { test, expect } from "@playwright/test";

test.describe("dashboard (fake mode)", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    // Wait for the first telemetry frame to render the panels.
    await expect(page.getByTestId("state")).toBeVisible();
  });

  test("streams live telemetry", async ({ page }) => {
    await expect(page.getByTestId("conn")).toContainText("live");
    await expect(page.getByTestId("state")).toHaveText(/RUNNING|FINISH|PAUSE|IDLE/);
    await expect(page.getByTestId("nozzle-temp")).toContainText("°C");
    await expect(page.getByTestId("bed-temp")).toContainText("°C");
  });

  test("wifi signal shows as a tiered meter in the maintenance panel", async ({ page }) => {
    // WiFi + nozzle spec live in the machine panel's maintenance block (status you
    // check during upkeep), not the job overview band.
    const wifi = page.getByTestId("machine-hw").getByTestId("wifi");
    // The fake reports -58dBm → a "fair" tier (warn tone), dBm shown as text.
    await expect(wifi).toBeVisible();
    await expect(wifi).toContainText("-58dBm");
    await expect(wifi).toHaveClass(/wifi--warn/);
  });

  test("nozzle spec shows beside the WiFi meter in the maintenance panel", async ({ page }) => {
    const hw = page.getByTestId("machine-hw");
    const n = hw.getByTestId("nozzle-spec");
    await expect(n).toBeVisible();
    await expect(n).toContainText("0.4");
    await expect(n).toContainText("stainless");
    await expect(hw.getByTestId("wifi")).toBeVisible(); // beside the wifi meter
  });

  test("RFID reader state shows in the AMS header", async ({ page }) => {
    const r = page.getByTestId("ams-rfid");
    await expect(r).toBeVisible();
    // The fake reports the reader present → ✓, so no warn tone (class is exactly
    // "amslink", not "amslink amslink--warn").
    await expect(r).toContainText("rfid");
    await expect(r).toHaveClass("amslink");
  });

  test("renders the AMS trays", async ({ page }) => {
    await expect(page.getByTestId("tray-0")).toBeVisible();
    await expect(page.getByTestId("tray-3")).toBeVisible();
  });

  test("pause reports verified", async ({ page }) => {
    await page.getByRole("button", { name: "pause", exact: true }).click();
    await expect(page.getByTestId("toast")).toContainText("verified");
  });

  test("light toggle reflects reported state and toggles", async ({ page }) => {
    const toggle = page.getByTestId("light-toggle");
    await expect(toggle).toHaveAttribute("data-state", "off");
    await expect(toggle).toHaveAttribute("aria-checked", "false");
    await toggle.click();
    await expect(page.getByTestId("toast")).toContainText("verified");
  });

  test("the footer no longer duplicates relocated machine status", async ({ page }) => {
    // WiFi + nozzle moved to the maintenance panel, RFID to the AMS header, and the
    // chamber light keeps only its controls toggle. In fake mode that leaves the
    // footer with no chips, so it isn't rendered at all.
    await expect(page.getByTestId("light-toggle")).toBeVisible();
    await expect(page.getByTestId("foot")).toHaveCount(0);
  });

  test("speed shows the active tier", async ({ page }) => {
    await expect(page.getByTestId("speed-standard")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("speed-silent")).toHaveAttribute("aria-pressed", "false");
  });

  test("job controls are gated by the printer state", async ({ page }) => {
    // pause/resume is one flip-button that only reads "resume" while PAUSE. The
    // fake status stream only ever shows RUNNING/FINISH (never PAUSE), so the
    // resume affordance is never offered — the button stays "pause".
    await expect(page.getByTestId("job-resume")).toHaveCount(0);
    // The fake source cycles RUNNING → FINISH, so pause/stop availability must
    // track the *displayed* state, not a fixed assumption. Poll state + the
    // buttons together so we evaluate a single consistent frame.
    await expect
      .poll(async () => {
        const state = ((await page.getByTestId("state").textContent()) ?? "").toUpperCase().trim();
        const pause = await page.getByRole("button", { name: "pause", exact: true }).isEnabled();
        const stop = await page.getByRole("button", { name: "stop", exact: true }).isEnabled();
        const running = state === "RUNNING";
        return pause === running && stop === running;
      })
      .toBeTruthy();
  });

  test("stop requires confirmation, then verifies", async ({ page }) => {
    await page.getByRole("button", { name: "stop", exact: true }).click();
    await expect(page.getByTestId("confirm")).toBeVisible();
    await page.getByTestId("confirm-stop").click();
    await expect(page.getByTestId("toast")).toContainText("verified");
  });

  test("lists files and directories", async ({ page }) => {
    await expect(page.getByTestId("files")).toBeVisible();
    await expect(page.getByTestId("file").first()).toContainText(".3mf");
    await expect(page.getByTestId("dir").first()).toBeVisible();
    await expect(page.getByTestId("sd-chip")).toContainText("present");
  });

  test("navigates into a directory and back", async ({ page }) => {
    await expect(page.getByTestId("files-path")).toHaveText("/");
    await page.getByTestId("dir").first().click();
    await expect(page.getByTestId("files-path")).not.toHaveText("/");
    await page.getByTestId("updir").click();
    await expect(page.getByTestId("files-path")).toHaveText("/");
  });

  test("shows file thumbnails (embedded plate preview)", async ({ page }) => {
    await expect(page.getByTestId("thumb").first()).toBeVisible();
  });

  test("clicking a file opens its detail with the 3D viewer", async ({ page }) => {
    // Click the row (top-left, away from the print button) to open details.
    await page
      .getByTestId("file")
      .filter({ hasText: ".3mf" })
      .first()
      .click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("file-detail")).toBeVisible();
    await expect(page.getByTestId("viewer-canvas")).toBeVisible();
    // Mode toggle is present (mesh / toolpath).
    await expect(page.getByTestId("viewer-mode-toolpath")).toBeVisible();
    await page.getByRole("button", { name: "close" }).click();
    await expect(page.getByTestId("file-detail")).toHaveCount(0);
  });

  test("has a GitHub repo link", async ({ page }) => {
    await expect(page.getByTestId("github")).toHaveAttribute("href", /github\.com/);
  });

  test("print → preview shows a plan; start on a busy printer is refused", async ({ page }) => {
    await page.getByTestId("print").first().click();
    await expect(page.getByTestId("start-dialog")).toBeVisible();
    await page.getByRole("button", { name: "preview" }).click();
    await expect(page.getByTestId("start-result")).toContainText("plate");
    // Timelapse arm: off by default, so the plan's clean-timelapse clause says so;
    // arming it updates the clause (fake files aren't inspectable, so it's the
    // arm-state wording, not the has-blocks verdict).
    await expect(page.getByTestId("start-result")).toContainText("timelapse off");
    await page.getByTestId("start-timelapse").check();
    await page.getByRole("button", { name: "preview" }).click();
    await expect(page.getByTestId("start-result")).toContainText("timelapse armed");
    // The fake source streams RUNNING, so the idle guard refuses a new print.
    await page.getByTestId("start-confirm").click();
    await expect(page.getByTestId("start-result")).toContainText("busy");
  });

  test("start dialog shows the file's clean-timelapse capability on open", async ({ page }) => {
    // The open /api/files/inspect read drives an inline capability line the moment the
    // dialog opens (fake files aren't inspectable, so mock a capable file).
    await page.route("**/api/files/inspect**", (r) =>
      r.fulfill({ json: { inspected: true, has_timelapse_blocks: true } }),
    );
    await page.getByTestId("print").first().click();
    await expect(page.getByTestId("start-dialog")).toBeVisible();
    const cap = page.getByTestId("start-tl-capability");
    await expect(cap).toContainText("per-layer park moves");
    await expect(cap).toHaveClass(/start__tlcap--ok/);
  });

  test("gcode console sends a line", async ({ page }) => {
    await page.getByTestId("gcode-input").fill("G28");
    await page.getByTestId("gcode-send").click();
    await expect(page.getByTestId("toast")).toContainText("verified");
  });

  test("theme toggle cycles auto → dark → light", async ({ page }) => {
    const btn = page.getByTestId("theme");
    await expect(page.locator("html")).not.toHaveAttribute("data-theme", /.+/); // auto
    await btn.click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
    await btn.click();
    await expect(page.locator("html")).toHaveAttribute("data-theme", "light");
  });

  test("machine panel renders the concentric jog dial + Z ladder", async ({ page }) => {
    await expect(page.getByTestId("machine")).toBeVisible();
    await expect(page.getByTestId("jog-dial")).toBeVisible();
    await expect(page.getByTestId("jog-zstack")).toBeVisible();
    // a sample of the 12 wedge×ring segments + the centre home (all SVG)
    for (const id of ["jog-yplus-1", "jog-xminus-10", "jog-xplus-.1", "home-all"]) {
      await expect(page.getByTestId(id)).toBeAttached();
    }
    // The SVG segments/home carry button semantics so keyboard users can jog/home
    // (they aren't real <button>s — role + key handling is wired manually).
    await expect(page.getByTestId("jog-yplus-1")).toHaveAttribute("role", "button");
    await expect(page.getByTestId("home-all")).toHaveAttribute("role", "button");
    // Z ladder rungs: magnitude is the position (.1/1/10 each way), no separate step picker.
    for (const id of ["jog-zplus-10", "jog-zplus-.1", "jog-zminus-1", "jog-zminus-10"]) {
      await expect(page.getByTestId(id)).toBeAttached();
    }
  });

  test("jog/home are disabled while the printer is busy", async ({ page }) => {
    // The fake source streams RUNNING (a busy state), so motion is gated off: the dial
    // goes aria-disabled (pointer-events off) and the Z ladder rungs disable.
    await expect
      .poll(async () => {
        const state = ((await page.getByTestId("state").textContent()) ?? "").toUpperCase().trim();
        if (state !== "RUNNING") return null;
        const dialOff = (await page.getByTestId("jog-dial").getAttribute("aria-disabled")) === "true";
        const zplus = await page.getByTestId("jog-zplus-1").isDisabled();
        return dialOff && zplus;
      })
      .toBeTruthy();
  });

  test("setting a nozzle temperature reports verified (allowed while busy)", async ({ page }) => {
    const set = page.getByTestId("temp-nozzle-set");
    // Temperature changes are allowed even mid-job. The set button gates on a
    // non-empty value (like gcode-send), so fill first, then it's enabled.
    await page.getByTestId("temp-nozzle-input").fill("210");
    await expect(set).toBeEnabled();
    await set.click();
    await expect(page.getByTestId("toast")).toContainText("verified");
  });

  test("temperature control lives inside the temperature card", async ({ page }) => {
    const card = page.getByTestId("temperature");
    // Card title is singular (not "temperatures").
    await expect(card.getByText("temperature", { exact: true })).toBeVisible();
    // The set/cool controls now sit in the temperature card, beside the readouts.
    for (const id of ["temp-nozzle-input", "temp-nozzle-set", "temp-bed-input", "temp-bed-set"]) {
      await expect(card.getByTestId(id)).toBeVisible();
    }
    // …and no longer in the machine/controls panel.
    await expect(page.getByTestId("machine").getByTestId("temp-nozzle-input")).toHaveCount(0);
  });

  test("camera tabs render and switch the active source", async ({ page }) => {
    // The E2E server is launched with two --camera-url + one --cameras-config (park
    // cam), so three tabs show.
    const tabs = page.locator('[data-testid^="camera-tab-"]');
    await expect(tabs).toHaveCount(3);
    await expect(page.getByTestId("camera-view")).toBeVisible();
    await expect(tabs.nth(0)).toHaveAttribute("aria-selected", "true");
    await tabs.nth(1).click();
    await expect(tabs.nth(1)).toHaveAttribute("aria-selected", "true");
    await expect(tabs.nth(0)).toHaveAttribute("aria-selected", "false");
  });

  test("a park-capable camera offers the live↔park toggle and start control", async ({
    page,
  }) => {
    // The seeded "park cam" (a stream + a calibrated park_tuning) is park-capable, so
    // its view gains a toggle and the timelapse bar a park start control. The other
    // (snapshot-only) cameras do not.
    await page.getByTestId("camera-tab-ext-0").click(); // a snapshot-only camera
    await expect(page.getByTestId("camera-view-toggle")).toHaveCount(0);
    await expect(page.getByTestId("timelapse-park-start")).toHaveCount(0);

    await page.getByTestId("camera-tab-ext-2").click(); // the park cam
    await expect(page.getByTestId("camera-view-toggle")).toBeVisible();
    await expect(page.getByTestId("timelapse-park-start")).toBeVisible();

    // No park run is active, so there's nothing to show in the park view — the park
    // toggle is disabled until a run is started (you can't switch to an empty preview).
    await expect(page.getByTestId("camera-view-park")).toBeDisabled();
    await expect(page.getByTestId("camera-view-live")).toBeEnabled();
  });

  test("park player scrubs the captured filmstrip (mocked run)", async ({ page }) => {
    // Fake mode has no real ffmpeg park run, so drive the player off mocked endpoints: a
    // live run that owns the park cam (ext-2) with a 3-frame filmstrip. This exercises the
    // toggle gate, the player render, live-tip follow, and frame stepping.
    const run = (over: Record<string, unknown>) => ({
      running: false,
      mode: "park",
      cameras: [] as string[],
      camera: null,
      every: 0,
      interval_ms: null,
      frames: 0,
      failures: 0,
      current_layer: null,
      out_dir: null,
      last_error: null,
      ...over,
    });
    await page.route("**/api/timelapse", (r) =>
      r.fulfill({
        json: {
          ...run({ running: true }),
          smooth: run({}),
          plain: run({}),
          park: run({ running: true, cameras: ["ext-2"], frames: 3 }),
        },
      }),
    );
    // Deliberately SPARSE frame numbers (0, 2, 5 — a skipped/malformed line leaves a gap):
    // the player must address frames by their real `n`, not the scrubber position.
    await page.route("**/api/cameras/ext-2/park", (r) =>
      r.fulfill({
        json: {
          running: true,
          count: 3,
          parks: [
            { n: 0, t: 0.0, confidence: 0.91 },
            { n: 2, t: 5.2, confidence: 0.88 },
            { n: 5, t: 10.4, confidence: 0.95 },
          ],
        },
      }),
    );
    // Any indexed frame → a stand-in JPEG (the bytes don't matter; we assert data-n).
    await page.route("**/api/cameras/ext-2/park/*", (r) =>
      r.fulfill({ contentType: "image/jpeg", body: "jpeg" }),
    );
    await page.goto("/"); // re-navigate so the routes apply from a clean load

    await page.getByTestId("camera-tab-ext-2").click();
    // A run owns ext-2, so the toggle is enabled → switch to the player.
    await expect(page.getByTestId("camera-view-park")).toBeEnabled();
    await page.getByTestId("camera-view-park").click();

    // It opens on the live tip: last position (3/3) → real frame n=5, live badge showing.
    await expect(page.getByTestId("park-player")).toBeVisible();
    await expect(page.getByTestId("park-count")).toHaveText("3 / 3");
    await expect(page.getByTestId("park-frame")).toHaveAttribute("data-n", "5");
    await expect(page.getByTestId("park-live")).toBeVisible();

    // Step back: prev → position 2/3 → real frame n=2; leaving the tip clears the badge.
    await page.getByTestId("park-prev").click();
    await expect(page.getByTestId("park-count")).toHaveText("2 / 3");
    await expect(page.getByTestId("park-frame")).toHaveAttribute("data-n", "2");
    await expect(page.getByTestId("park-live")).toHaveCount(0);

    // Jump to first (n=0), then back to latest (n=5).
    await page.getByTestId("park-first").click();
    await expect(page.getByTestId("park-frame")).toHaveAttribute("data-n", "0");
    await page.getByTestId("park-latest").click();
    await expect(page.getByTestId("park-count")).toHaveText("3 / 3");
    await expect(page.getByTestId("park-frame")).toHaveAttribute("data-n", "5");
  });

  test("a dead camera reports the offline state", async ({ page }) => {
    // Both configured cameras point at dead URLs, so the snapshot 502s and the
    // active view reports offline.
    await expect(page.getByTestId("camera-offline")).toBeVisible({ timeout: 15000 });
  });

  test("camera manage modal: prefilled, adds a row, closes on scrim", async ({ page }) => {
    await page.getByTestId("cameras-manage").click();
    await expect(page.getByTestId("cameras-modal")).toBeVisible();
    // Three configured cameras prefill three rows; wait for them before adding.
    await expect(page.getByTestId("camera-url-2")).toBeVisible();
    await page.getByTestId("camera-add").click();
    await expect(page.getByTestId("camera-url-3")).toBeVisible();
    // Close by clicking the scrim (outside the box) — no save, so the shared
    // server's camera list is untouched.
    await page.getByTestId("cameras-modal").click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("cameras-modal")).toHaveCount(0);
  });

  test("camera manage modal: per-camera park tuning editor", async ({ page }) => {
    // The live-park preview is enabled by a per-camera park_tuning. Verify the manage
    // form exposes a collapsible JSON editor for it and that it's editable. Like the
    // sibling manage test, this does NOT save — the shared --fake server's camera list
    // stays untouched (the park toggle/start need a saved capable camera, which can't be
    // configured here without racing the parallel camera tests).
    await page.getByTestId("cameras-manage").click();
    await expect(page.getByTestId("cameras-modal")).toBeVisible();
    await expect(page.getByTestId("camera-url-0")).toBeVisible();
    // expand the collapsed editor, then type some tuning JSON
    await page.getByTestId("camera-park-toggle-0").click();
    const editor = page.getByTestId("camera-park-0");
    await expect(editor).toBeVisible();
    await editor.fill('{"fps":4,"left_frac":0.33}');
    await expect(editor).toHaveValue('{"fps":4,"left_frac":0.33}');
    await page.getByTestId("cameras-modal").click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("cameras-modal")).toHaveCount(0);
  });

  test("file detail and start dialog close on scrim (outside) click", async ({ page }) => {
    await page
      .getByTestId("file")
      .filter({ hasText: ".3mf" })
      .first()
      .click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("file-detail")).toBeVisible();
    await page.getByTestId("file-detail").click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("file-detail")).toHaveCount(0);

    await page.getByTestId("print").first().click();
    await expect(page.getByTestId("start-dialog")).toBeVisible();
    await page.getByTestId("start-dialog").click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("start-dialog")).toHaveCount(0);
  });

  test("stop confirm closes on scrim (outside) click", async ({ page }) => {
    await page.getByRole("button", { name: "stop", exact: true }).click();
    await expect(page.getByTestId("confirm")).toBeVisible();
    await page.getByTestId("confirm").click({ position: { x: 6, y: 6 } });
    await expect(page.getByTestId("confirm")).toHaveCount(0);
  });

  test("renders on a phone viewport", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 850 });
    await expect(page.getByTestId("state")).toBeVisible();
    await expect(page.getByTestId("tray-0")).toBeVisible();
  });
});
