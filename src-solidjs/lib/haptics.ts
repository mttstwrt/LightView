// Best-effort haptic feedback for touch interactions.
//
// `navigator.vibrate` covers Android browsers (it needs sticky user
// activation, which the pointerdown that started the gesture provides —
// firing from a long-press timer is fine). iOS Safari has no vibration API
// at all, so long-presses there are silent; the visual pop is the only
// feedback. Keep calls short — this is a tick, not a buzz.

/** One short tick, e.g. when a long-press crosses its threshold. */
export function hapticTick(): void {
  try {
    navigator.vibrate?.(15);
  } catch {
    // Some engines throw on vibrate without activation — feedback is
    // best-effort, never let it break the gesture.
  }
}
