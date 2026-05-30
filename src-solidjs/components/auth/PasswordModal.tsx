import { Show, createSignal, onCleanup, onMount } from "solid-js";
import { PASSWORD_CHALLENGE_EVENT } from "../../lib/runtime";

/** Modal shown to the remote web client when the server returns 401 with
 *  `WWW-Authenticate: LV-Password`, i.e. the device cookie is valid but
 *  too much time has passed since the last password check.
 *
 *  Listens for `PASSWORD_CHALLENGE_EVENT`; on submit, POSTs to
 *  `/auth/password` and emits `lightview:password-resolved` so the pending
 *  fetch in `_httpInvoke` can retry. */
export function PasswordModal() {
  const [open, setOpen] = createSignal(false);
  const [password, setPassword] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [error, setError] = createSignal("");

  let inputRef: HTMLInputElement | undefined;

  const onChallenge = () => {
    setError("");
    setPassword("");
    setOpen(true);
    // Defer focus until the input is in the DOM.
    queueMicrotask(() => inputRef?.focus());
  };

  const resolve = (accepted: boolean) => {
    setOpen(false);
    setBusy(false);
    window.dispatchEvent(
      new CustomEvent<boolean>("lightview:password-resolved", { detail: accepted }),
    );
  };

  const submit = async (e?: Event) => {
    e?.preventDefault();
    if (busy() || !password()) return;
    setBusy(true);
    setError("");
    try {
      const res = await fetch("/auth/password", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ password: password() }),
      });
      if (res.ok) {
        resolve(true);
      } else if (res.status === 403) {
        setError("Wrong password.");
        setBusy(false);
      } else {
        setError(`Auth failed (${res.status})`);
        setBusy(false);
      }
    } catch (err) {
      setError(String(err));
      setBusy(false);
    }
  };

  onMount(() => {
    window.addEventListener(PASSWORD_CHALLENGE_EVENT, onChallenge);
  });
  onCleanup(() => {
    window.removeEventListener(PASSWORD_CHALLENGE_EVENT, onChallenge);
  });

  return (
    <Show when={open()}>
      <div class="fixed inset-0 z-[300] flex items-center justify-center bg-black/70 backdrop-blur-sm p-6">
        <form
          onSubmit={submit}
          class="w-full max-w-xs flex flex-col gap-3 px-5 py-5 rounded-lg bg-neutral-950 border border-neutral-800"
        >
          <div class="flex flex-col gap-0.5">
            <span class="text-sm text-neutral-200">Gallery password</span>
            <span class="text-[11px] text-neutral-500">
              Session expired — re-enter the password to continue.
            </span>
          </div>
          <input
            ref={(el) => (inputRef = el)}
            type="password"
            value={password()}
            onInput={(e) => setPassword(e.currentTarget.value)}
            disabled={busy()}
            class="px-3 py-2 rounded bg-neutral-900 border border-neutral-800 text-sm outline-none focus:border-teal-700"
            autocomplete="current-password"
          />
          <Show when={error()}>
            <span class="text-xs text-red-400">{error()}</span>
          </Show>
          <div class="flex gap-2 mt-1">
            <button
              type="button"
              onClick={() => resolve(false)}
              disabled={busy()}
              class="flex-1 px-3 py-1.5 rounded text-xs bg-neutral-800 hover:bg-neutral-700 text-neutral-300 transition-colors disabled:opacity-40"
            >
              Cancel
            </button>
            <button
              type="submit"
              disabled={busy() || !password()}
              class="flex-1 px-3 py-1.5 rounded text-xs bg-teal-700/70 hover:bg-teal-700 text-teal-50 transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
            >
              {busy() ? "Checking…" : "Unlock"}
            </button>
          </div>
        </form>
      </div>
    </Show>
  );
}
