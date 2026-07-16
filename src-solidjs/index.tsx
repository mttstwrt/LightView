import 'solid-devtools';
import { render } from "solid-js/web";
import "./styles/global.css";
import { App } from "./App";
import { PairApp } from "./components/auth/PairApp";
import { initMediaServer } from "./lib/ipc";
import { isWeb } from "./lib/runtime";

// Prime the media server URL cache so `videoSrc()` can resolve synchronously
// by the time the viewer renders. Non-blocking: fails silently if the server
// has not finished starting (fallback is the existing external-player UI).
void initMediaServer().catch((e) =>
  console.warn("Media server init failed:", e),
);

// Service worker: web client only (the desktop webview loads from Tauri's
// own protocol, not the axum server). Registered after load so it never
// competes with the boot waterfall for bandwidth. Gives cache-first assets,
// stale-while-revalidate thumbnails, and an offline-capable shell — see
// public/sw.js for the policy.
if (isWeb() && typeof navigator !== "undefined" && "serviceWorker" in navigator) {
  window.addEventListener("load", () => {
    navigator.serviceWorker
      .register("/sw.js")
      .catch((e) => console.warn("Service worker registration failed:", e));
  });
}

// Tiny path-based router. The remote SPA's pair page lives at /pair so the
// QR code can deep-link to a known URL. Everything else is the gallery app.
const path = typeof window !== "undefined" ? window.location.pathname : "/";
const isPairRoute = isWeb() && path.startsWith("/pair");

render(
  () => (isPairRoute ? <PairApp /> : <App />),
  document.getElementById("root")!,
);
