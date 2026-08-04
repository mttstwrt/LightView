// Service-worker escape hatches for the remote web client.
//
// The worker (public/sw.js) is what makes the app open instantly and work
// offline, but it also sits between the browser and the network on every
// navigation. When the server becomes unreachable for a reason the *browser*
// would normally surface — most often a lapsed click-through exception for the
// self-signed TLS cert on iOS — the worker's cached-shell fallback hides the
// prompt that would fix it: the navigation never reaches the network, so there
// is no interstitial to click through.
//
// `resetServiceWorker()` is the cure. Unregistering leaves the next navigation
// uncontrolled, so it goes to the network and the browser gets to show its own
// certificate UI. Cookies and Cache Storage are untouched, so the device stays
// paired — which is the whole point, since the previous workaround (clear all
// site data) dropped `lv_device` and forced a re-pair.

/** Unregister every service worker for this origin, then reload so the
 *  navigation hits the network unmediated. Resolves only if the reload
 *  somehow doesn't happen. */
export async function resetServiceWorker(): Promise<void> {
  try {
    if ("serviceWorker" in navigator) {
      const regs = await navigator.serviceWorker.getRegistrations();
      await Promise.all(regs.map((r) => r.unregister()));
    }
  } catch {
    // Unregistration is best-effort; reload regardless — a plain reload still
    // helps when the worker was never the problem.
  }
  window.location.replace("/");
}
