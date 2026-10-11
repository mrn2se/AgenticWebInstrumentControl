/**
 * Live mode's view-state machine (SPECIFICATION.md §5.5): "zooming or
 * panning pauses following without pausing the recording: the view switches
 * to a fixed range served from history (`rec.view.update`), and a *Follow*
 * button resumes the tail." This module is the pure state half of that —
 * what the *next* state is given a user action — kept free of any `ws.send`
 * call so it can be unit-tested without a socket, exactly like
 * `acquisition.ts` keeps `applyAcqStateEvent` free of the send side.
 */
export interface LiveViewState {
  /** `true`: the view follows the recording's tail (`rec.view`'s `follow_s` form). `false`: a fixed `[t0, t1)` served from history. */
  readonly following: boolean;
  /** The fixed range while paused; `null` while following. */
  readonly t0: number | null;
  readonly t1: number | null;
}

export function initialLiveViewState(): LiveViewState {
  return { following: true, t0: null, t1: null };
}

/** A user zoom/pan reported by the renderer's `onViewportChange` (SPEC §5.3) — pauses following. */
export function pauseAtViewport(t0: number, t1: number): LiveViewState {
  return { following: false, t0, t1 };
}

/** The toolbar's *Follow* button. */
export function resumeFollowing(): LiveViewState {
  return initialLiveViewState();
}

/**
 * How many follow seconds to request for `rec.view`'s `follow_s` (SPEC
 * §3.4) from a window-control value in seconds. Clamped to a sane minimum —
 * a zero or negative window would ask the server for an empty view.
 */
export function followWindowSeconds(requestedSeconds: number): number {
  return Number.isFinite(requestedSeconds) && requestedSeconds > 0 ? requestedSeconds : 1;
}

/**
 * `rec.view`'s `columns` (SPEC §3.4) from the plot's own pixel width — "one
 * pair per screen column" (§3.2's backpressure note applies to the same
 * idea here): more columns than the plot can show wastes bandwidth on
 * resolution nothing draws, fewer than the plot's width would draw a
 * coarser signal than the screen can display. Clamped so a momentarily
 * zero-width container (mid-layout) never asks for zero columns.
 */
export function columnsForWidth(pixelWidth: number): number {
  return Math.max(1, Math.round(pixelWidth));
}

/**
 * A trailing-edge debouncer: `call()` schedules `fn(arg)` after `delayMs` of
 * no further calls, restarting the timer on each call so only the last
 * argument in a burst is ever delivered — exactly what "debounce viewport
 * updates" (SPEC §5.5 task instructions) means for a `rec.view.update` that
 * would otherwise fire on every intermediate wheel-zoom tick. `cancel()` is
 * for teardown (`deactivate()`), so a debounced call never lands after the
 * module that scheduled it has gone away.
 */
export interface Debouncer<T> {
  call(arg: T): void;
  cancel(): void;
}

export function debounce<T>(delayMs: number, fn: (arg: T) => void): Debouncer<T> {
  let timer: ReturnType<typeof setTimeout> | null = null;
  return {
    call(arg: T): void {
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        fn(arg);
      }, delayMs);
    },
    cancel(): void {
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
    },
  };
}
