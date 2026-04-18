import 'solid-devtools';
import { render } from "solid-js/web";
import "./styles/global.css";
import { App } from "./App";
import { initMediaServer } from "./lib/ipc";

// Prime the media server URL cache so `videoSrc()` can resolve synchronously
// by the time the viewer renders. Non-blocking: fails silently if the server
// has not finished starting (fallback is the existing external-player UI).
void initMediaServer().catch((e) =>
  console.warn("Media server init failed:", e),
);

render(() => <App />, document.getElementById("root")!);
