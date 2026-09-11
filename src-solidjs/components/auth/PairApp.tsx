import { Show, createSignal, onMount } from "solid-js";

/** The `/pair` page for the remote web client. Two enrollment paths land
 *  here:
 *   - QR code → URL has `?code=<32-byte hex>`; auto-fills the code field.
 *   - PIN → URL has no code; user types the 6 digits shown on the desktop.
 *
 *  Both redeem via the same POST. The server's response sets the
 *  `lv_device` cookie; on success we hop to `/` and the gallery loads
 *  normally. */
export function PairApp() {
  const initialCode = (() => {
    const params = new URLSearchParams(window.location.search);
    return params.get("code") ?? "";
  })();
  const initialName = (() => {
    // Best-effort device label so users can tell their phones apart in the
    // device list. They can always rename.
    const ua = navigator.userAgent;
    if (/iPhone/i.test(ua)) return "iPhone";
    if (/iPad/i.test(ua)) return "iPad";
    if (/Android/i.test(ua)) return "Android";
    if (/Mac OS/i.test(ua)) return "Mac";
    if (/Windows/i.test(ua)) return "Windows PC";
    return "Browser";
  })();

  const [code, setCode] = createSignal(initialCode);
  const [name, setName] = createSignal(initialName);
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");
  const [done, setDone] = createSignal(false);

  const submit = async (e?: Event) => {
    e?.preventDefault();
    if (busy()) return;
    setBusy(true);
    setError("");
    try {
      const res = await fetch("/pair/redeem", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          code: code().trim(),
          device_name: name().trim(),
        }),
      });
      if (!res.ok) {
        const detail = await res.text().catch(() => res.statusText);
        // Map the canonical errors to friendlier copy.
        if (res.status === 404) setError("Code not found. Generate a new one from the desktop app.");
        else if (res.status === 409) setError("That code was already used.");
        else if (res.status === 410) setError("Code expired. Generate a new one.");
        else if (res.status === 503) setError("No gallery is open on the desktop yet.");
        else setError(detail || `Pairing failed (${res.status})`);
        setBusy(false);
        return;
      }
      setDone(true);
      // Brief pause so the user sees the success state before the gallery
      // app takes over.
      setTimeout(() => {
        window.location.replace("/");
      }, 600);
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  };

  // If the URL provided a QR code, attempt it immediately. The device name
  // pre-fill above is good enough that the user can just see "connected" and
  // hop to the gallery.
  onMount(() => {
    if (initialCode) {
      void submit();
    }
  });

  return (
    <div class="min-h-screen flex items-center justify-center bg-neutral-950 text-neutral-200 p-6">
      <div class="w-full max-w-sm flex flex-col gap-4">
        <div class="flex flex-col items-center gap-1 mb-2">
          <div class="text-2xl font-light tracking-wide text-teal-300">LightView</div>
          <div class="text-xs text-neutral-500">Pair this device</div>
        </div>

        <Show
          when={!done()}
          fallback={
            <div class="px-4 py-6 rounded-lg bg-neutral-900/60 border border-teal-700/40 text-center">
              <div class="text-teal-300 text-sm">Paired — loading gallery&hellip;</div>
            </div>
          }
        >
          <form onSubmit={submit} class="flex flex-col gap-3">
            <label class="flex flex-col gap-1">
              <span class="text-[11px] text-neutral-500 uppercase tracking-wide">
                Pairing code or PIN
              </span>
              <input
                type="text"
                inputMode="text"
                autocomplete="off"
                autocapitalize="off"
                spellcheck={false}
                value={code()}
                onInput={(e) => setCode(e.currentTarget.value)}
                disabled={busy()}
                class="px-3 py-2 rounded bg-neutral-900 border border-neutral-800 text-sm font-mono outline-none focus:border-teal-700"
                placeholder="6-digit PIN or code from QR"
              />
            </label>

            <label class="flex flex-col gap-1">
              <span class="text-[11px] text-neutral-500 uppercase tracking-wide">
                Device name
              </span>
              <input
                type="text"
                value={name()}
                onInput={(e) => setName(e.currentTarget.value)}
                disabled={busy()}
                class="px-3 py-2 rounded bg-neutral-900 border border-neutral-800 text-sm outline-none focus:border-teal-700"
                placeholder="e.g. Pixel, iPad, work laptop"
              />
            </label>

            <Show when={error()}>
              <div class="px-3 py-2 rounded bg-red-950/50 border border-red-900/60 text-xs text-red-300">
                {error()}
              </div>
            </Show>

            <button
              type="submit"
              disabled={busy() || !code().trim()}
              class="mt-1 px-4 py-2 rounded bg-teal-700/70 hover:bg-teal-700 text-sm text-teal-50 transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
            >
              {busy() ? "Pairing…" : "Pair device"}
            </button>
          </form>

          <p class="text-[11px] text-neutral-600 leading-relaxed mt-2">
            Open the LightView desktop app, go to Settings &rarr; Remote
            Access, and scan the QR code or read off the PIN. Codes are
            one-time and expire after a few minutes.
          </p>

          {/* The security warning you clicked through to get here is a
              short-lived per-origin exception — on iOS especially, it lapses
              and (in a home-screen app) can't be re-accepted. Installing the
              certificate replaces it with real trust for the cert's lifetime.
              Plain <a>, no `download`: iOS routes the response to the profile
              installer off the navigation. */}
          <p class="text-[11px] text-neutral-600 leading-relaxed">
            Saw a security warning?{" "}
            <a href="/cert" class="text-teal-500/80 hover:text-teal-400 underline">
              Install this server's certificate
            </a>{" "}
            to stop it coming back. On iPhone/iPad, finish in Settings &rarr;
            Profile Downloaded, then enable it under General &rarr; About &rarr;
            Certificate Trust Settings.
          </p>
        </Show>
      </div>
    </div>
  );
}
