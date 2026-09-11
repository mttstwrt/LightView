// Entry point: redeem, route, mount — in that order.
//
// **Redemption has to happen before anything authenticated goes out.** The
// launch URL carries `?t=<token>`; it is single-use and rotates the moment it
// is exchanged, so the token is spent here, the query string is cleared from
// the address bar with `history.replaceState`, and only then does the app
// mount. Clearing it is not cosmetic: a `?t=` left in place survives into
// session restore and history, where it is a spent credential at best and a
// live one at worst if the tab is duplicated before the app boots.
//
// **The auth status is fetched before mount for the same reason.** It is the
// only thing that tells a client whether the bind it is talking to has a
// pairing flow at all, and that answer decides what a later 401 means:
// "go and pair" on a served bind, "this session has ended" on loopback. A 401
// can arrive with the app's first request, so learning it afterwards would be
// a race the user loses by being shown the wrong dead end.
//
// The service worker is gone with `sw.js`: the browser's own HTTP cache
// revalidates thumbnails against their `ETag`, which is the same behaviour with
// no second cache to invalidate. `solid-devtools` went with `devtools.html`.

import { render } from "solid-js/web";

import "./styles/global.css";
import { App } from "./App";
import { PairApp } from "./components/auth/PairApp";
import { authStatus, redeemLaunchToken } from "./lib/ipc";

const LAUNCH_PARAM = "t";

/** Spend `?t=` if it is there, and strip it either way. */
async function redeemLaunch(): Promise<void> {
  const url = new URL(window.location.href);
  const token = url.searchParams.get(LAUNCH_PARAM);
  if (!token) return;

  url.searchParams.delete(LAUNCH_PARAM);
  // Replace before awaiting: the token is already in flight and must not stay
  // in the bar for the duration of the round trip.
  history.replaceState(null, "", url.pathname + url.search + url.hash);

  // A refusal is not fatal and not worth a dialog. Either the cookie from an
  // earlier redemption is still good, in which case nothing was needed, or it
  // is not, in which case the first request 401s and the dead-end banner says
  // so with the right words.
  await redeemLaunchToken(token).catch(() => false);
}

async function boot() {
  const root = document.getElementById("root")!;

  // `/pair` is reachable without any credential by design — it is where a
  // device goes to get one — so it neither redeems nor waits on status.
  if (window.location.pathname.startsWith("/pair")) {
    render(() => <PairApp />, root);
    return;
  }

  await redeemLaunch();
  await authStatus().catch(() => undefined);
  render(() => <App />, root);
}

void boot();
