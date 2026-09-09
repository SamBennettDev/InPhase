import { defineConfig } from "vite";

// Build to web/dist/, which the host embeds via rust-embed (§16, §17).
// No external CDNs — the play origin serves only same-origin assets (§14.1 CSP).
export default defineConfig({
  // Stamped by tools/ship.sh with the same value it gives the Rust build, so a
  // page can tell whether it was built alongside the host serving it
  // (web/src/buildid.ts). "dev" for any build not made by ship.sh.
  define: {
    __INPHASE_BUILD_ID__: JSON.stringify(process.env.INPHASE_BUILD_ID ?? "dev"),
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2022",
    rollupOptions: {
      input: {
        main: "index.html",
      },
    },
  },
  server: {
    port: 5173,
    proxy: {
      "/api": { target: "http://127.0.0.1:47800", ws: true, changeOrigin: false },
    },
  },
});
