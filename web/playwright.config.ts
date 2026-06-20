import { defineConfig, devices } from "@playwright/test";

// E2E against a real `bambu serve --fake` (no printer needed). The webServer
// builds the binary + frontend, then runs the server; tests drive the embedded
// SPA. Reads are open and the fake controller verifies every control action.
const PORT = 8099;

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: "list",
  use: {
    baseURL: `http://127.0.0.1:${PORT}`,
    trace: "on-first-retry",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    // Three (deliberately dead) external cameras so the dashboard renders camera tabs
    // to exercise — their snapshots/streams 502, which also covers the offline state.
    // The third ("park cam") is seeded with a stream + park_tuning so it's live-park
    // capable, exercising the park toggle / start control / preview.
    command: `bash -c "cd .. && cargo build --features dashboard --bin bambu && pnpm -C web build && exec ./target/debug/bambu serve --fake --host 127.0.0.1 --port ${PORT} --interval 1 --camera-url 'cam a=http://127.0.0.1:59995/a.jpg' --camera-url 'cam b=http://127.0.0.1:59996/b.jpg' --cameras-config web/e2e/fixtures/park-camera.json"`,
    url: `http://127.0.0.1:${PORT}/api/status`,
    reuseExistingServer: !process.env.CI,
    timeout: 180_000,
    stdout: "pipe",
    stderr: "pipe",
  },
});
