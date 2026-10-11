import type { McpLogEntry } from "../../proto/types";

/**
 * The MCP tab's local copy of the server's message log (SPECIFICATION.md
 * §8.5): entries arrive in `seq` order from `mcp.log after=<lastSeq>`, and
 * the server drops its oldest ones to stay inside its memory bound, so the
 * local copy mirrors that by keeping only as many as the server says it
 * still holds. Pure; the DOM layer is `index.ts`.
 */
export class McpLogModel {
  private items: McpLogEntry[] = [];
  private last = 0;

  constructor(private readonly maxEntries = 2000) {}

  get entries(): readonly McpLogEntry[] {
    return this.items;
  }

  /** The highest `seq` seen; the `after` for the next fetch. */
  get lastSeq(): number {
    return this.last;
  }

  /**
   * Adds what a fetch returned. `serverLastSeq` is the result's `last_seq`;
   * `serverRetained` its `logging.entries` — entries the server has since
   * dropped (or cleared) are dropped here too. Returns whether anything changed.
   */
  merge(fetched: readonly McpLogEntry[], serverLastSeq: number, serverRetained: number): boolean {
    const before = this.items.length;
    const newestKnown = this.items.at(-1)?.seq ?? 0;
    for (const entry of fetched) {
      if (entry.seq > newestKnown && !this.items.some((e) => e.seq === entry.seq)) {
        this.items.push(entry);
      }
    }
    this.items.sort((a, b) => a.seq - b.seq);
    const keep = Math.min(serverRetained, this.maxEntries);
    const dropped = Math.max(0, this.items.length - keep);
    if (dropped > 0) this.items.splice(0, dropped);
    const changed = this.items.length !== before || fetched.length > 0 || dropped > 0;
    this.last = Math.max(this.last, serverLastSeq);
    return changed;
  }

  clear(): void {
    this.items = [];
  }
}

export interface LogFilter {
  /** Case-insensitive; matches tool, method, client, session and summary. */
  readonly text: string;
  readonly errorsOnly: boolean;
  /** Session label, or "" for every session. */
  readonly session: string;
  readonly imagesOnly: boolean;
}

export const NO_FILTER: LogFilter = { text: "", errorsOnly: false, session: "", imagesOnly: false };

export function matchesFilter(entry: McpLogEntry, filter: LogFilter): boolean {
  if (filter.errorsOnly && !entry.is_error) return false;
  if (filter.imagesOnly && entry.images.length === 0) return false;
  if (filter.session && entry.session !== filter.session) return false;
  const needle = filter.text.trim().toLowerCase();
  if (!needle) return true;
  return [entry.tool ?? "", entry.method, entry.client, entry.session ?? "", entry.summary]
    .some((field) => field.toLowerCase().includes(needle));
}

/** What the list shows as an entry's name: the tool, else the method or event. */
export function entryTitle(entry: McpLogEntry): string {
  switch (entry.kind) {
    case "session_start":
      return "session start";
    case "session_end":
      return "session end";
    case "rejected":
      return `rejected ${entry.method}`;
    default:
      if (entry.method === "initialize") return "initialize";
      return entry.tool ?? entry.method;
  }
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

export function formatDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return "";
  if (ms < 1) return "<1 ms";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(2)} s`;
}

/** Local wall-clock time with milliseconds, `HH:MM:SS.mmm`. */
export function formatClock(tMs: number): string {
  const d = new Date(tMs);
  const pad = (n: number, w = 2) => String(n).padStart(w, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

/** "3 s ago", "4 min ago", … for the sessions table. */
export function formatAgo(tMs: number, nowMs: number): string {
  const s = Math.max(0, Math.round((nowMs - tMs) / 1000));
  if (s < 60) return `${s} s ago`;
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 48) return `${h} h ago`;
  return `${Math.round(h / 24)} d ago`;
}

/** `1 error`, `2 errors`, `0 entries`. */
export function plural(n: number, word: string): string {
  if (n === 1) return `${n} ${word}`;
  return word.endsWith("y") ? `${n} ${word.slice(0, -1)}ies` : `${n} ${word}s`;
}
