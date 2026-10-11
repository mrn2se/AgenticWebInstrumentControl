import type { PatternMatch, RecSearchResult, RecWaitResult } from "../../proto/types";
import { formatSiDuration } from "../../format";
import { toErrorInfo } from "../../ws/errors";

/**
 * Pattern search/wait formatting (SPEC §2.9, §5.5): "A pattern search box
 * lists matches as clickable timestamps ... Show parse errors inline,
 * exactly as the server words them, since the server's messages are written
 * to be actionable." This module turns the wire results into what the panel
 * renders; it never re-derives or rewords a server message.
 */

export interface SearchOutcomeMatches {
  readonly status: "matches";
  readonly matches: readonly PatternMatch[];
  readonly truncated: boolean;
}

export interface SearchOutcomeError {
  readonly status: "error";
  /** The server's own `error.msg` (SPEC §2.9.1: "an agent should be able to fix its own pattern from the message alone"), unmodified. */
  readonly message: string;
}

export type SearchOutcome = SearchOutcomeMatches | SearchOutcomeError;

/** Turns a `rec.search` result (success) into the panel's outcome shape. */
export function searchOutcomeFromResult(result: RecSearchResult): SearchOutcome {
  return { status: "matches", matches: result.matches, truncated: result.truncated };
}

/** Turns a failed `rec.search` request (e.g. an `invalid_pattern` error, or any other transport failure) into the panel's outcome shape — the message shown is always the server's own wording (`toErrorInfo`), when there is one. */
export function searchOutcomeFromError(err: unknown): SearchOutcomeError {
  return { status: "error", message: toErrorInfo(err).msg };
}

export function formatMatchTimestamp(match: PatternMatch): string {
  return formatSiDuration(match.t_s);
}

export function searchResultSummary(outcome: SearchOutcomeMatches): string {
  if (outcome.matches.length === 0) return "no matches";
  const count = `${outcome.matches.length} match${outcome.matches.length === 1 ? "" : "es"}`;
  return outcome.truncated ? `${count} (truncated — narrow the range or pattern)` : count;
}

export type WaitOutcome =
  | { readonly status: "waiting" }
  | { readonly status: "matched"; readonly match: PatternMatch }
  | { readonly status: "timed-out" }
  | { readonly status: "error"; readonly message: string };

export function waitOutcomeFromResult(result: RecWaitResult): WaitOutcome {
  if ("match" in result) return { status: "matched", match: result.match };
  return { status: "timed-out" };
}

export function waitOutcomeFromError(err: unknown): WaitOutcome {
  return { status: "error", message: toErrorInfo(err).msg };
}

/** Renders a `WaitOutcome` as the one line the panel shows for it. */
export function formatWaitOutcome(outcome: WaitOutcome): string {
  switch (outcome.status) {
    case "waiting":
      return "waiting…";
    case "matched":
      return `matched at ${formatMatchTimestamp(outcome.match)}`;
    case "timed-out":
      return "timed out";
    case "error":
      return outcome.message;
    default: {
      const exhaustive: never = outcome;
      throw new Error(`formatWaitOutcome: unhandled outcome ${JSON.stringify(exhaustive)}`);
    }
  }
}
