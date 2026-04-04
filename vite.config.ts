import { defineConfig } from "vite";
import { resolve } from "path";
import solid from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";
import devtools from 'solid-devtools/vite';

export default defineConfig({
  plugins: [
    devtools({ autostructure: true }),
    solid(),
    tailwindcss(),
  ],
  root: "src-solidjs",
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "esnext",
    outDir: "../dist",
    emptyOutDir: true,
    minify: !process.env.TAURI_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_DEBUG,
    rollupOptions: {
      input: {
        main: resolve(__dirname, "src-solidjs/index.html"),
                            devtools: resolve(__dirname, "src-solidjs/devtools.html"),
      },
    },
  },
});
