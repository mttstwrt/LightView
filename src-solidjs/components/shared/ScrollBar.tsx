import { createSignal, createEffect, on, onMount, onCleanup, Show, For } from "solid-js";
import { markScrollBarActive, markScrollBarReleased } from "../../lib/scrollDynamics";

export interface ScrollIndicator {
  /** Position along the track as a fraction 0–1. */
  position: number;
  /** Short label to display (e.g. "Jan 2024", "A", "10 MB"). */
  label: string;
}

interface ScrollBarProps {
  /**
   * If provided, track this element's scroll. Otherwise track window scroll.
   * Must be a stable reference (not recreated each render).
   */
  container?: HTMLElement;
  /** Total scrollable content height. Only needed for window mode. */
  contentHeight?: number;
  /** CSS class for positioning the scrollbar track. */
  class?: string;
  /** Fixed indicators shown along the track. */
  indicators?: ScrollIndicator[];
  /** Called with scroll fraction (0–1) to get a label shown next to the thumb. */
  getThumbLabel?: (fraction: number) => string;
}

/**
 * Custom scrollbar that replaces the broken WebKitGTK default.
 * Works in two modes:
 *   - Window mode (no container): tracks window.scrollY
 *   - Element mode (container ref): tracks element.scrollTop
 *
 * The thumb sits inside a larger invisible "grip" that owns the drag; see the
 * comment on it in the markup for why the drag lives there and not on the
 * track. Pressing the bare track still jumps.
 */
export function ScrollBar(props: ScrollBarProps) {
  let trackRef: HTMLDivElement | undefined;
  let gripRef: HTMLDivElement | undefined;
  const [thumbTop, setThumbTop] = createSignal(0);
  const [thumbHeight, setThumbHeight] = createSignal(0);
  const [scrollFraction, setScrollFraction] = createSignal(0);
  const [visible, setVisible] = createSignal(false);
  const [dragging, setDragging] = createSignal(false);
  const [hovering, setHovering] = createSignal(false);

  let hideTimeout: ReturnType<typeof setTimeout> | undefined;
  let dragStartY = 0;
  let dragStartThumbTop = 0;
  let dragPointerId: number | undefined;

  const MIN_THUMB = 24;
  // Grip dimensions — the draggable box around the thumb. 44px is the smallest
  // reliable touch target, and the width reaches inwards from the rail so a
  // fingertip has somewhere to land without the rail itself getting wider.
  const GRIP_MIN_H = 44;
  const GRIP_W = 34;

  const getMetrics = () => {
    if (props.container) {
      const el = props.container;
      return {
        scrollTop: el.scrollTop,
        viewportH: el.clientHeight,
        contentH: el.scrollHeight,
      };
    }
    return {
      scrollTop: window.scrollY,
      viewportH: window.innerHeight,
      contentH: props.contentHeight ?? document.documentElement.scrollHeight,
    };
  };

  const scrollTo = (y: number, range: number) => {
    const clamped = Math.max(0, Math.min(y, range));
    if (props.container) {
      props.container.scrollTop = clamped;
    } else {
      window.scrollTo(0, clamped);
    }
  };

  const recalc = () => {
    const { scrollTop, viewportH, contentH } = getMetrics();
    if (contentH <= viewportH) {
      setThumbHeight(0);
      setVisible(false);
      return;
    }

    const trackH = trackRef?.clientHeight ?? viewportH;
    const ratio = viewportH / contentH;
    const rawThumb = Math.max(MIN_THUMB, trackH * ratio);
    const maxTravel = trackH - rawThumb;
    const frac = scrollTop / (contentH - viewportH);
    const top = maxTravel * frac;

    setThumbHeight(rawThumb);
    // While a drag is in progress the thumb follows the finger, not the scroll
    // position it asked for. Those are the same number on a desktop, but not on
    // iOS: scrolling is committed on the compositor and `scrollY` reads back a
    // frame or more stale, so feeding it into the thumb's position mid-gesture
    // makes the thumb chase the finger and visibly jitter. `onGripPointerMove`
    // owns these two while dragging.
    if (dragging()) return;
    setScrollFraction(Math.max(0, Math.min(1, frac)));
    setThumbTop(Math.max(0, Math.min(top, maxTravel)));
  };

  const showTemporarily = () => {
    setVisible(true);
    if (hideTimeout) clearTimeout(hideTimeout);
    if (!dragging() && !hovering()) {
      hideTimeout = setTimeout(() => setVisible(false), 1200);
    }
  };

  const onScroll = () => {
    recalc();
    showTemporarily();
  };

  // --- Dragging the thumb (cursor and finger alike) -------------------------
  //
  // Pointer events rather than mouse events, so one code path serves both. That
  // is not cosmetic: with mouse handlers only, a touch-drag on the thumb was
  // never a scrollbar drag at all — the browser claimed the gesture and panned
  // the page under the finger — so the only thing that worked on a phone was
  // tapping the track, and finding a spot meant a rapid series of full-distance
  // jumps. `touch-action: none` on the grip is what takes the gesture back.

  const onGripPointerDown = (e: PointerEvent) => {
    // Middle/right buttons keep their native meaning.
    if (e.button !== 0 && e.pointerType === "mouse") return;
    // preventDefault only — the press must keep bubbling to window, where
    // `createOpenAtBottom` watches for it to know the user has taken over.
    e.preventDefault();

    markScrollBarActive();
    setDragging(true);
    dragPointerId = e.pointerId;
    dragStartY = e.clientY;
    dragStartThumbTop = thumbTop();
    try { gripRef?.setPointerCapture(e.pointerId); } catch {}
    showTemporarily();
  };

  const onGripPointerMove = (e: PointerEvent) => {
    if (dragPointerId !== e.pointerId) return;
    e.preventDefault();
    markScrollBarActive();

    const { viewportH, contentH } = getMetrics();
    const trackH = trackRef?.clientHeight ?? viewportH;
    const rawThumb = Math.max(MIN_THUMB, trackH * (viewportH / contentH));
    const maxTravel = trackH - rawThumb;
    if (maxTravel <= 0) return;

    const dy = e.clientY - dragStartY;
    const scrollRange = contentH - viewportH;

    // Drive the thumb from the pointer and the scroll from the thumb, rather
    // than both from the scroll position — see the note in `recalc`. The thumb
    // is then pinned under the finger for the whole gesture whatever the
    // scroller does behind it.
    const top = Math.max(0, Math.min(dragStartThumbTop + dy, maxTravel));
    setThumbTop(top);
    setScrollFraction(maxTravel > 0 ? top / maxTravel : 0);
    scrollTo((top / maxTravel) * scrollRange, scrollRange);
  };

  const onGripPointerUp = (e: PointerEvent) => {
    if (dragPointerId !== e.pointerId) return;
    dragPointerId = undefined;
    try { gripRef?.releasePointerCapture(e.pointerId); } catch {}
    setDragging(false);
    // A finger leaves no cursor behind, so the hover affordances must not
    // outlive the gesture — otherwise the indicator rail and the thumb label
    // stay pinned on screen for the rest of the session.
    if (e.pointerType !== "mouse") setHovering(false);
    // Re-sync to where the scroller actually ended up: `recalc` has been
    // leaving the thumb alone for the length of the drag, and the two can
    // differ at the ends of the range or if the content height changed.
    recalc();
    markScrollBarReleased();
    showTemporarily();
  };

  /** Press on the bare track: jump there. */
  const onTrackClick = (e: MouseEvent) => {
    if (e.target !== trackRef) return;
    markScrollBarActive();
    const rect = trackRef!.getBoundingClientRect();
    const { viewportH, contentH } = getMetrics();
    const trackH = trackRef!.clientHeight;
    const scrollRange = contentH - viewportH;
    scrollTo(((e.clientY - rect.top) / trackH) * scrollRange, scrollRange);
    markScrollBarReleased();
  };

  onMount(() => {
    const target = props.container ?? window;
    target.addEventListener("scroll", onScroll, { passive: true });
    window.addEventListener("resize", recalc);
    recalc();

    onCleanup(() => {
      target.removeEventListener("scroll", onScroll);
      window.removeEventListener("resize", recalc);
      if (hideTimeout) clearTimeout(hideTimeout);
      if (dragPointerId !== undefined) markScrollBarReleased();
    });
  });

  // Recalc when contentHeight changes (window mode)
  createEffect(on(() => props.contentHeight, () => {
    recalc();
    showTemporarily();
  }));

  const opacity = () => {
    if (dragging() || hovering()) return 1;
    return visible() ? 0.7 : 0;
  };

  const showIndicators = () => (dragging() || hovering()) && (props.indicators?.length ?? 0) > 0;

  const thumbLabel = () => {
    if (!props.getThumbLabel || (!dragging() && !hovering())) return "";
    return props.getThumbLabel(scrollFraction());
  };

  return (
    <Show when={thumbHeight() > 0}>
      <div
        ref={trackRef}
        class={props.class ?? "fixed right-0.5 top-0 bottom-0 z-[60]"}
        style={{
          width: "10px",
          cursor: "pointer",
          opacity: opacity(),
          transition: dragging() ? "none" : "opacity 0.25s ease",
          "pointer-events": opacity() > 0 ? "auto" : "none",
        }}
        onClick={onTrackClick}
        onMouseEnter={() => { setHovering(true); showTemporarily(); }}
        onMouseLeave={() => { setHovering(false); showTemporarily(); }}
      >
        {/* Track indicators */}
        <Show when={showIndicators()}>
          <For each={props.indicators}>
            {(ind) => (
              <div
                style={{
                  position: "absolute",
                  top: `${ind.position * 100}%`,
                  right: "14px",
                  transform: "translateY(-50%)",
                  "white-space": "nowrap",
                  padding: "1px 5px",
                  "border-radius": "3px",
                  background: "rgba(0, 0, 0, 0.5)",
                  "font-size": "10px",
                  "line-height": "1.3",
                  "font-weight": "500",
                  color: "rgba(255, 255, 255, 0.7)",
                  "text-shadow": "0 1px 2px rgba(0, 0, 0, 0.8)",
                  "pointer-events": "none",
                  "user-select": "none",
                }}
              >
                {ind.label}
              </div>
            )}
          </For>
        </Show>

        {/* The grip: an invisible box around the thumb that owns the drag.
            Deliberately larger than the thumb it wraps — on a long gallery the
            thumb is only MIN_THUMB tall, which no fingertip can hit — and it is
            the *only* element that sets `touch-action: none`. Claiming the whole
            track would mean any swipe along the right edge of the screen warped
            the gallery instead of scrolling it, which on a phone is where a
            thumb naturally rests. */}
        <div
          ref={gripRef}
          style={{
            position: "absolute",
            top: `${thumbTop() + thumbHeight() / 2 - Math.max(GRIP_MIN_H, thumbHeight()) / 2}px`,
            // Anchored to the track's right edge and grown leftwards, so none
            // of it falls off the side of the screen.
            right: "0",
            width: `${GRIP_W}px`,
            height: `${Math.max(GRIP_MIN_H, thumbHeight())}px`,
            display: "flex",
            "align-items": "center",
            "justify-content": "flex-end",
            "padding-right": "1px",
            "box-sizing": "border-box",
            "touch-action": "none",
            cursor: "grab",
          }}
          onPointerDown={onGripPointerDown}
          onPointerMove={onGripPointerMove}
          onPointerUp={onGripPointerUp}
          onPointerCancel={onGripPointerUp}
        >
          {/* Thumb */}
          <div
            style={{
              width: "8px",
              height: `${thumbHeight()}px`,
              "border-radius": "4px",
              background: dragging() || hovering()
                ? "rgba(255, 255, 255, 0.55)"
                : "rgba(255, 255, 255, 0.35)",
              "box-shadow": "0 0 2px rgba(0, 0, 0, 0.6), inset 0 0 0 0.5px rgba(255, 255, 255, 0.40)",
              transition: dragging() ? "none" : "background 0.15s ease",
              "pointer-events": "none",
            }}
          />
        </div>

        {/* Thumb label (tooltip) */}
        <Show when={thumbLabel()}>
          <div
            style={{
              position: "absolute",
              top: `${thumbTop() + thumbHeight() / 2}px`,
              right: "14px",
              transform: "translateY(-50%)",
              "white-space": "nowrap",
              padding: "3px 6px",
              "border-radius": "4px",
              background: "rgba(0, 0, 0, 0.75)",
              "font-size": "11px",
              "line-height": "1.2",
              color: "rgba(255, 255, 255, 0.85)",
              "pointer-events": "none",
              "user-select": "none",
              "backdrop-filter": "blur(8px)",
              border: "1px solid rgba(255, 255, 255, 0.1)",
            }}
          >
            {thumbLabel()}
          </div>
        </Show>
      </div>
    </Show>
  );
}
