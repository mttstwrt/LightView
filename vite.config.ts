import { defineConfig } from "vite";
import { resolve } from "path";
import { execSync } from "node:child_process";
import { readFileSync } from "node:fs";
import solid from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";
import devtools from 'solid-devtools/vite';

// Build stamp baked into the SPA so a running client can report exactly which
// build it is — the "did docker compose actually pull/rebuild?" question. The
// timestamp always changes per build (the reliable signal, since .git is
// dockerignored). The short commit is best-effort: read from git when the repo
// is present (dev/desktop builds), else from a VITE_GIT_SHA env/build-arg so
// container builds can still carry a real commit id if the caller supplies one.
const pkgVersion: string = JSON.parse(
  readFileSync(resolve(__dirname, "package.json"), "utf-8"),
).version;
function gitSha(): string {
  const fromEnv = process.env.VITE_GIT_SHA?.trim();
  if (fromEnv) return fromEnv;
  try {
    return execSync("git rev-parse --short HEAD", {
      stdio: ["ignore", "pipe", "ignore"],
    })
      .toString()
      .trim();
  } catch {
    return "";
  }
}

export default defineConfig({
  define: {
    __APP_VERSION__: JSON.stringify(pkgVersion),
    __GIT_SHA__: JSON.stringify(gitSha()),
    __BUILD_TIME__: JSON.stringify(new Date().toISOString()),
  },
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
