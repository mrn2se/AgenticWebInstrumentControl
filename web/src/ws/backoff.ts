/**
 * Reconnect backoff schedule. Pulled out as a pure function so it is
 * testable without spinning up sockets or timers.
 */
export interface BackoffOptions {
  /** Delay before the first reconnect attempt (attempt 0), in ms. */
  readonly initialMs: number;
  /** Upper bound on the delay, in ms. */
  readonly maxMs: number;
  /** Multiplier applied per attempt. */
  readonly factor: number;
  /** Fraction of the raw delay to randomize by, in either direction. */
  readonly jitterRatio: number;
}

export const DEFAULT_BACKOFF: BackoffOptions = {
  initialMs: 500,
  maxMs: 15_000,
  factor: 2,
  jitterRatio: 0.2,
};

/**
 * Computes the delay before reconnect attempt number `attempt` (0-indexed:
 * the first retry after a drop is attempt 0). Exponential growth capped at
 * `maxMs`, with symmetric jitter of up to `jitterRatio` of the raw value so a
 * fleet of clients dropped by the same event doesn't reconnect in lockstep.
 *
 * `rand` defaults to `Math.random` and is injectable so tests can make the
 * jitter deterministic.
 */
export function computeBackoffDelay(
  attempt: number,
  opts: BackoffOptions = DEFAULT_BACKOFF,
  rand: () => number = Math.random,
): number {
  const raw = Math.min(opts.maxMs, opts.initialMs * Math.pow(opts.factor, attempt));
  const jitter = raw * opts.jitterRatio * (rand() * 2 - 1);
  return Math.max(0, Math.round(raw + jitter));
}
