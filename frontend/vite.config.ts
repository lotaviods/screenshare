import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tuned for Tauri: fixed dev port so `devUrl` in tauri.conf.json is stable,
// and the src-tauri directory is excluded from the dev watcher.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
});
