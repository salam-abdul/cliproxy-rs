import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Development only: point the proxy at another backend with CPA_BACKEND=http://127.0.0.1:8318.
const backend = process.env.CPA_BACKEND || "http://127.0.0.1:8317";

export default defineConfig({
  base: "./",
  plugins: [svelte({ compilerOptions: { discloseVersion: false } })],
  server: { port: 5173, proxy: { "/v8": backend, "/healthz": backend } },
  // Terser is build-time only; it shrinks Svelte's output about 8% more than esbuild.
  build: {
    target: "es2022",
    sourcemap: false,
    minify: "terser",
    terserOptions: { compress: { passes: 3 }, format: { comments: false } },
  },
});
