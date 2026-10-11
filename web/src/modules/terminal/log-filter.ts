/**
 * Client-side level filter for the `log` terminal source. The server side
 * (`awic-server::terminal::log_source`) already cuts at `INFO` and above
 * regardless of `RUST_LOG` — this narrows *within* that fixed floor, using
 * the plain, uncolored `[LEVEL]` prefix every line carries for exactly this
 * purpose (ANSI color codes aside).
 */

export type LogFilterLevel = "all" | "info" | "warn" | "error";

const SEVERITY: Record<string, number> = {
  trace: 0,
  debug: 1,
  info: 2,
  warn: 3,
  error: 4,
};

const FILTER_FLOOR: Record<Exclude<LogFilterLevel, "all">, number> = {
  info: 2,
  warn: 3,
  error: 4,
};

/** Extracts the level word from a line's leading `[LEVEL]` tag (xterm's
 * writes always carry `\r\n`-terminated lines shaped like `[ INFO] ...`,
 * padded to a fixed width — see `log_source::format_line`). `null` for a
 * line with no recognizable tag (e.g. a client-side `[session ...]` note),
 * which {@link lineMatchesFilter} always keeps rather than silently drops. */
export function lineLevel(line: string): string | null {
  const match = /^\[\s*([A-Za-z]+)\]/.exec(line);
  return match ? match[1]!.toLowerCase() : null;
}

/** Whether `line` should be shown under `filter`. Lines with no recognized
 * level tag always pass — filtering only ever narrows *recognized* levels,
 * never hides something it can't classify. */
export function lineMatchesFilter(line: string, filter: LogFilterLevel): boolean {
  if (filter === "all") return true;
  const level = lineLevel(line);
  if (level === null) return true;
  const severity = SEVERITY[level];
  if (severity === undefined) return true;
  return severity >= FILTER_FLOOR[filter];
}

/** Filters a buffer of complete lines (no trailing `\r\n`) under `filter`. */
export function filterLines(lines: readonly string[], filter: LogFilterLevel): string[] {
  return lines.filter((line) => lineMatchesFilter(line, filter));
}
