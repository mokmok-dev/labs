import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // kumo ships Tailwind v4 `@theme` rules that neither lightningcss nor
    // esbuild can minify. The UI is a local desktop app, so ship unminified.
    cssMinify: false,
  },
});