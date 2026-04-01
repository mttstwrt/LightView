import { createSignal, createEffect, on, onMount, onCleanup, Show, For } from "solid-js";

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
 */
export function ScrollBar(props: ScrollBarProps) {
  let trackRef: HTMLDivElement | undefined;
  const [thumbTop, setThumbTop] = createSignal(0);
  const [thumbHeight, setThumbHeight] = createSignal(0);
  const [scrollFraction, setScrollFraction] = createSignal(0);
  const [visible, setVisible] = createSignal(false);
  const [dragging, setDragging] = createSignal(false);
  const [hovering, setHovering] = createSignal(false);

  let hideTimeout: ReturnType<typeof setTimeout> | undefined;
  let dragStartY = 0;
  let dragStartScroll = 0;

  const MIN_THUMB = 24;

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

    setScrollFraction(Math.max(0, Math.min(1, frac)));
    setThumbHeight(rawThumb);
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

  // Drag handling
  const onThumbMouseDown = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    setDragging(true);
    dragStartY = e.clientY;
    dragStartScroll = getMetrics().scrollTop;

    const onMove = (ev: MouseEvent) => {
      const { viewportH, contentH } = getMetrics();
      const trackH = trackRef?.clientHeight ?? viewportH;
      const rawThumb = Math.max(MIN_THUMB, trackH * (viewportH / contentH));
      const maxTravel = trackH - rawThumb;
      if (maxTravel <= 0) return;

      const dy = ev.clientY - dragStartY;
      const scrollRange = contentH - viewportH;
      const newScroll = dragStartScroll + (dy / maxTravel) * scrollRange;

      if (props.container) {
        props.container.scrollTop = Math.max(0, Math.min(newScroll, scrollRange));
      } else {
        window.scrollTo(0, Math.max(0, Math.min(newScroll, scrollRange)));
      }
    };

    const onUp = () => {
      setDragging(false);
      showTemporarily();
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };

    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };

  // Click on track to jump
  const onTrackClick = (e: MouseEvent) => {
    if (e.target !== trackRef) return;
    const rect = trackRef!.getBoundingClientRect();
    const clickY = e.clientY - rect.top;
    const { viewportH, contentH } = getMetrics();
    const trackH = trackRef!.clientHeight;
    const scrollRange = contentH - viewportH;
    const fraction = clickY / trackH;
    const target = fraction * scrollRange;

    if (props.container) {
      props.container.scrollTop = Math.max(0, Math.min(target, scrollRange));
    } else {
      window.scrollTo(0, Math.max(0, Math.min(target, scrollRange)));
    }
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

        {/* Thumb */}
        <div
          style={{
            position: "absolute",
            top: `${thumbTop()}px`,
            left: "1px",
            right: "1px",
            height: `${thumbHeight()}px`,
            "border-radius": "4px",
            background: dragging() || hovering()
              ? "rgba(255, 255, 255, 0.55)"
              : "rgba(255, 255, 255, 0.35)",
            "box-shadow": "0 0 2px rgba(0, 0, 0, 0.6), inset 0 0 0 0.5px rgba(255, 255, 255, 0.15)",
            transition: dragging() ? "none" : "background 0.15s ease",
            cursor: "grab",
          }}
          onMouseDown={onThumbMouseDown}
        />

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
