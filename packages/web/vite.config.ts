import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Dev server proxies the API to a local `pit up` / `pit server`.
const api = "http://127.0.0.1:7070";

export default defineConfig({
  base: "/",
  plugins: [react()],
  build: { outDir: "dist", emptyOutDir: true, assetsDir: "assets" },
  server: { proxy: { "/api": { target: api, changeOrigin: true } } },
  preview: { proxy: { "/api": { target: api, changeOrigin: true } } },
});
