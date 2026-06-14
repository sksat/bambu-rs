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

  test("speed shows the active tier", async ({ page }) => {
    await expect(page.getByTestId("speed-standard")).toHaveAttribute("aria-pressed", "true");
    await expect(page.getByTestId("speed-silent")).toHaveAttribute("aria-pressed", "false");
  });

  test("job controls are gated by the printer state", async ({ page }) => {
    // resume is valid only while PAUSE — the fake never pauses, so it stays
    // disabled regardless of phase (the clearest proof state-gating is on).
    await expect(page.getByRole("button", { name: "resume", exact: true })).toBeDisabled();
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

  test("3D button opens the viewer", async ({ page }) => {
    await page.getByTestId("view3d").first().click();
    await expect(page.getByTestId("viewer")).toBeVisible();
    await expect(page.getByTestId("viewer-canvas")).toBeVisible();
    await page.getByRole("button", { name: "close" }).click();
    await expect(page.getByTestId("viewer")).toHaveCount(0);
  });

  test("has a GitHub repo link", async ({ page }) => {
    await expect(page.getByTestId("github")).toHaveAttribute("href", /github\.com/);
  });

  test("print → preview shows a plan; start on a busy printer is refused", async ({ page }) => {
    await page.getByTestId("print").first().click();
    await expect(page.getByTestId("start-dialog")).toBeVisible();
    await page.getByRole("button", { name: "preview" }).click();
    await expect(page.getByTestId("start-result")).toContainText("plate");
    // The fake source streams RUNNING, so the idle guard refuses a new print.
    await page.getByTestId("start-confirm").click();
    await expect(page.getByTestId("start-result")).toContainText("busy");
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

  test("renders on a phone viewport", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 850 });
    await expect(page.getByTestId("state")).toBeVisible();
    await expect(page.getByTestId("tray-0")).toBeVisible();
  });
});
