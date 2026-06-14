import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Build output goes to web/dist, which the Rust binary embeds via rust-embed.
// In dev (`pnpm dev`), proxy /api to the running `bambu dashboard` server.
export default defineConfig({
  plugins: [react()],
  build: { outDir: "dist", emptyOutDir: true },
  server: {
    proxy: {
      "/api": { target: "http://127.0.0.1:8088", changeOrigin: true, ws: true },
    },
  },
});
