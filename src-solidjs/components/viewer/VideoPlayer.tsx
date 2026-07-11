import { Show, createSignal, createEffect, onMount, onCleanup } from "solid-js";
import { settings } from "../../stores/settingsStore";

// Custom video chrome for the media viewer, replacing the native `controls`
// attribute. Native controls swallow every touch that lands on the video, so
// none of the viewer's gestures (swipe-to-navigate, swipe-down-to-dismiss,
// swipe-up-for-info) worked over videos; with our own chrome the video surface
// is gesture-transparent and only the elements marked `data-gesture-exempt`
// (plus buttons) capture touches. Also gives one consistent look across
// WebKitGTK, desktop browsers, and iOS instead of three native skins.

interface VideoPlayerProps {
  src: string;
  /** Autoplay starts muted (browser policy + politeness); a tap-to-unmute
   *  pill is shown. A manual play tap starts unmuted. */
  startMuted: boolean;
  /** Viewer chrome visibility — the control bar fades with the rest. */
  chromeVisible: boolean;
  onLoaded: () => void;
  onError: () => void;
  /** Let the player drive the viewer chrome: show on pause / mouse move,
   *  hide after a few seconds of undisturbed playback. */
  onChromeShow: () => void;
  onChromeHide: () => void;
}

const IDLE_HIDE_MS = 3000;
/** Min ms between scrub-drag seeks. WebKitGTK's GStreamer pipeline handles
 *  discrete flushing seeks fine but chokes when they're issued every
 *  pointermove, so drags are throttled and the release always seeks. */
const SEEK_THROTTLE_MS = 150;

function formatTime(s: number): string {
  if (!isFinite(s) || s < 0) return "0:00";
  const t = Math.floor(s);
  const h = Math.floor(t / 3600);
  const m = Math.floor((t % 3600) / 60);
  const sec = t % 60;
  return h > 0
    ? `${h}:${String(m).padStart(2, "0")}:${String(sec).padStart(2, "0")}`
    : `${m}:${String(sec).padStart(2, "0")}`;
}

export function VideoPlayer(props: VideoPlayerProps) {
  let videoRef: HTMLVideoElement | undefined;
  let barRef: HTMLDivElement | undefined;

  const [playing, setPlaying] = createSignal(false);
  const [muted, setMuted] = createSignal(props.startMuted);
  const [time, setTime] = createSignal(0);
  const [duration, setDuration] = createSignal(0);
  const [bufferedEnd, setBufferedEnd] = createSignal(0);
  // The unmute pill goes away for good once the user has touched mute state
  // anywhere (pill or control-bar button) — they know where the control is.
  const [pillDismissed, setPillDismissed] = createSignal(!props.startMuted);

  createEffect(() => { if (videoRef) videoRef.muted = muted(); });

  const togglePlay = () => {
    const v = videoRef;
    if (!v) return;
    if (v.paused) void v.play().catch(() => {});
    else v.pause();
  };
  const toggleMute = () => {
    setPillDismissed(true);
    setMuted((m) => !m);
  };

  // --- Smooth progress: rAF ticker while playing ---------------------------
  let raf: number | null = null;
  let scrubbing = false;
  const tick = () => {
    const v = videoRef;
    if (v && !scrubbing) {
      setTime(v.currentTime);
      const b = v.buffered;
      setBufferedEnd(b.length ? b.end(b.length - 1) : 0);
    }
    raf = requestAnimationFrame(tick);
  };
  const stopTicker = () => {
    if (raf !== null) cancelAnimationFrame(raf);
    raf = null;
  };
  createEffect(() => {
    if (playing()) { if (raf === null) raf = requestAnimationFrame(tick); }
    else stopTicker();
  });
  onCleanup(stopTicker);

  // --- Scrubbing ------------------------------------------------------------
  let lastSeekApply = 0;
  const applySeek = (t: number, final: boolean) => {
    const v = videoRef;
    if (!v) return;
    const now = performance.now();
    if (final || now - lastSeekApply > SEEK_THROTTLE_MS) {
      lastSeekApply = now;
      try { v.currentTime = t; } catch { /* not seekable yet */ }
    }
  };
  const scrubTo = (e: PointerEvent, final: boolean) => {
    if (!barRef || duration() <= 0) return;
    const r = barRef.getBoundingClientRect();
    const frac = Math.min(1, Math.max(0, (e.clientX - r.left) / r.width));
    const t = frac * duration();
    setTime(t);
    applySeek(t, final);
  };
  const onBarDown = (e: PointerEvent) => {
    scrubbing = true;
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    scrubTo(e, false);
  };
  const onBarMove = (e: PointerEvent) => { if (scrubbing) scrubTo(e, false); };
  const onBarUp = (e: PointerEvent) => {
    if (!scrubbing) return;
    scrubbing = false;
    scrubTo(e, true);
  };

  // --- Idle auto-hide of the viewer chrome during playback ------------------
  // Mouse movement re-shows chrome (desktop convention). Touch is left to the
  // viewer's tap-to-toggle — driving chrome from raw touch events here would
  // fight that toggle — but any pointer contact re-arms the idle timer.
  let idleTimer: number | undefined;
  const clearIdle = () => { if (idleTimer) { clearTimeout(idleTimer); idleTimer = undefined; } };
  const armIdle = () => {
    clearIdle();
    idleTimer = window.setTimeout(() => props.onChromeHide(), IDLE_HIDE_MS);
  };
  createEffect(() => {
    if (playing()) armIdle();
    else { clearIdle(); props.onChromeShow(); }
  });
  onMount(() => {
    const onMove = (e: PointerEvent) => {
      if (e.pointerType !== "mouse") return;
      props.onChromeShow();
      if (playing()) armIdle();
    };
    const onDown = () => { if (playing()) armIdle(); };
    const onKey = (e: KeyboardEvent) => {
      const t = e.target as HTMLElement;
      if (t.closest("input, textarea")) return;
      if (e.key === " " || e.key === "k") { e.preventDefault(); togglePlay(); }
      else if (e.key === "m") toggleMute();
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerdown", onDown);
    window.addEventListener("keydown", onKey);
    onCleanup(() => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerdown", onDown);
      window.removeEventListener("keydown", onKey);
      clearIdle();
    });
  });

  // Desktop: click on the video toggles play/pause (player convention). Touch
  // taps are excluded — they bubble to the viewer, whose tap handler toggles
  // the chrome, and would otherwise double-fire here via the synthetic click.
  let lastPointerType = "";
  const onVideoClick = () => {
    if (lastPointerType === "mouse") togglePlay();
  };

  const progressPct = () => (duration() > 0 ? (time() / duration()) * 100 : 0);
  const bufferedPct = () => (duration() > 0 ? (bufferedEnd() / duration()) * 100 : 0);

  return (
    <>
      <video
        ref={(el) => { videoRef = el; el.muted = props.startMuted; }}
        src={props.src}
        class="max-w-[100vw] max-h-[100vh] outline-none pointer-events-auto"
        autoplay
        // Keep playback inside our viewer instead of iOS's native fullscreen
        // player, which autoplay otherwise triggers.
        playsinline
        attr:webkit-playsinline="true"
        preload="auto"
        onPointerDown={(e) => { lastPointerType = e.pointerType; }}
        onClick={onVideoClick}
        onPlay={() => setPlaying(true)}
        onPause={() => setPlaying(false)}
        onLoadedMetadata={(e) => setDuration(e.currentTarget.duration)}
        onDurationChange={(e) => setDuration(e.currentTarget.duration)}
        onLoadedData={() => props.onLoaded()}
        onEnded={(e) => {
          // Auto-replay. We deliberately avoid the native `loop` attribute:
          // on WebKitGTK/GStreamer with our streamed HTTP source, looping
          // does a non-flushing segment seek that drifts the clock (playback
          // speeds up after the first pass) and re-issues a Range request
          // that can stall the pipeline (freeze / jump to a random spot). A
          // manual flushing seek to 0 followed by play() restarts cleanly.
          if (!settings().display.video_autoplay_loop) return;
          const v = e.currentTarget;
          try { v.currentTime = 0; } catch { /* not seekable yet */ }
          void v.play().catch(() => {});
        }}
        onError={(e) => {
          const v = e.currentTarget as HTMLVideoElement;
          const codeNames: Record<number, string> = {
            1: "ABORTED",
            2: "NETWORK",
            3: "DECODE",
            4: "SRC_NOT_SUPPORTED",
          };
          console.error(
            "Video load failed:",
            "code=" + (v.error?.code ?? "null"),
            codeNames[v.error?.code ?? -1] ?? "",
            "message=" + JSON.stringify(v.error?.message ?? ""),
            "src=" + v.src,
          );
          props.onError();
        }}
      />

      {/* Center play button — only while paused (playback needs a visible
          affordance even with the chrome hidden). */}
      <Show when={!playing()}>
        <button
          class="absolute left-1/2 top-1/2 -translate-x-1/2 -translate-y-1/2 w-20 h-20 rounded-full bg-black/40 hover:bg-black/60 flex items-center justify-center cursor-pointer transition-colors pointer-events-auto"
          onClick={togglePlay}
          aria-label="Play"
        >
          <svg width="32" height="32" viewBox="0 0 24 24" fill="currentColor" class="text-white ml-1">
            <path d="M8 5v14l11-7z" />
          </svg>
        </button>
      </Show>

      {/* Control bar. `data-gesture-exempt` keeps viewer swipe gestures off
          the scrubber; it fades with the viewer chrome. */}
      <div
        data-gesture-exempt
        class="absolute inset-x-0 bottom-0 z-10 pointer-events-auto transition-opacity duration-200"
        style={{
          background: "linear-gradient(transparent, rgba(0, 0, 0, 0.7))",
          "padding-top": "24px",
          "padding-bottom": "calc(env(safe-area-inset-bottom, 0px) + 8px)",
          opacity: props.chromeVisible ? "1" : "0",
          "pointer-events": props.chromeVisible ? "auto" : "none",
        }}
      >
        <div class="flex items-center gap-3 px-4">
          <button
            class="shrink-0 w-8 h-8 flex items-center justify-center text-white/90 hover:text-white cursor-pointer"
            onClick={togglePlay}
            aria-label={playing() ? "Pause" : "Play"}
          >
            <Show
              when={playing()}
              fallback={
                <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor"><path d="M8 5v14l11-7z" /></svg>
              }
            >
              <svg width="18" height="18" viewBox="0 0 24 24" fill="currentColor"><path d="M6 5h4v14H6zM14 5h4v14h-4z" /></svg>
            </Show>
          </button>

          <span class="shrink-0 text-xs font-mono tabular-nums text-white/70">{formatTime(time())}</span>

          {/* Seek bar — tall touch target around a thin track. */}
          <div
            ref={barRef}
            class="relative flex-1 h-8 flex items-center cursor-pointer touch-none"
            onPointerDown={onBarDown}
            onPointerMove={onBarMove}
            onPointerUp={onBarUp}
            onPointerCancel={onBarUp}
          >
            <div class="relative h-1 w-full rounded-full bg-white/20 overflow-hidden">
              <div class="absolute inset-y-0 left-0 bg-white/25" style={{ width: `${bufferedPct()}%` }} />
              <div class="absolute inset-y-0 left-0 bg-white/90" style={{ width: `${progressPct()}%` }} />
            </div>
            <div
              class="absolute w-3 h-3 rounded-full bg-white -translate-x-1/2 pointer-events-none"
              style={{ left: `${progressPct()}%` }}
            />
          </div>

          <span class="shrink-0 text-xs font-mono tabular-nums text-white/70">{formatTime(duration())}</span>

          <button
            class="shrink-0 w-8 h-8 flex items-center justify-center text-white/90 hover:text-white cursor-pointer"
            onClick={toggleMute}
            aria-label={muted() ? "Unmute" : "Mute"}
          >
            <Show when={muted()} fallback={<VolumeIcon size={18} />}>
              <MutedIcon size={18} />
            </Show>
          </button>
        </div>
      </div>

      {/* Tap-to-unmute pill for muted autoplay. Top-centre, above everything
          (z-20): the bottom edge is crowded (control bar + its gradient hit
          box + the viewer's filename bar), and anything rendered inside the
          bar's padding area loses hit-testing to it — the tap would fall
          through to the viewer and just toggle chrome. */}
      <Show when={muted() && playing() && !pillDismissed()}>
        <button
          class="absolute left-1/2 -translate-x-1/2 z-20 flex items-center gap-2 px-4 py-2.5 rounded-full text-white text-xs cursor-pointer pointer-events-auto"
          style={{
            top: "calc(env(safe-area-inset-top, 0px) + 16px)",
            background: "rgba(0, 0, 0, 0.65)",
            "backdrop-filter": "blur(8px)",
            border: "1px solid rgba(255, 255, 255, 0.15)",
          }}
          onClick={toggleMute}
          aria-label="Unmute"
        >
          <MutedIcon size={16} />
          Tap to unmute
        </button>
      </Show>
    </>
  );
}

function VolumeIcon(props: { size: number }) {
  return (
    <svg width={props.size} height={props.size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M11 5L6 9H2v6h4l5 4V5z" fill="currentColor" stroke="none" />
      <path d="M15.5 8.5a5 5 0 0 1 0 7M18.4 5.6a9 9 0 0 1 0 12.8" />
    </svg>
  );
}

function MutedIcon(props: { size: number }) {
  return (
    <svg width={props.size} height={props.size} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
      <path d="M11 5L6 9H2v6h4l5 4V5z" fill="currentColor" stroke="none" />
      <path d="M23 9l-6 6M17 9l6 6" />
    </svg>
  );
}
