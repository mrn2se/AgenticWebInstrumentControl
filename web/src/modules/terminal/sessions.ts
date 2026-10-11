import type { TerminalKind, TerminalState } from "../../proto/types";

/** One open (or waiting/closed) terminal session's own view state — the
 * xterm instance itself lives in `index.ts`'s DOM layer; this is the pure
 * part: what tab to show, its badge, and whether input is accepted. */
export interface SessionViewModel {
  readonly sessionId: number;
  readonly source: string;
  readonly kind: TerminalKind;
  readonly label: string;
  readonly state: TerminalState;
}

/** Whether this session accepts `term.write` — a `log` session never does
 * (SPEC §11.2.1), and neither does one that isn't `open` right now. */
export function isWritable(session: SessionViewModel): boolean {
  return session.kind !== "log" && session.state === "open";
}

/** Short badge text for a session's sub-tab. */
export function stateBadge(state: TerminalState): string {
  switch (state) {
    case "open":
      return "open";
    case "waiting":
      return "waiting…";
    case "closed":
      return "closed";
  }
}

export type SessionMap = ReadonlyMap<number, SessionViewModel>;

export function upsertSession(sessions: SessionMap, session: SessionViewModel): SessionMap {
  const next = new Map(sessions);
  next.set(session.sessionId, session);
  return next;
}

export function removeSession(sessions: SessionMap, sessionId: number): SessionMap {
  if (!sessions.has(sessionId)) return sessions;
  const next = new Map(sessions);
  next.delete(sessionId);
  return next;
}

/** Applies a `term.state` event's new state to the session it names, if this
 * connection has it open — a `term.state` for a session opened by a
 * different tab or connection is not this module's to track. */
export function applySessionState(
  sessions: SessionMap,
  sessionId: number,
  state: TerminalState,
): SessionMap {
  const existing = sessions.get(sessionId);
  if (!existing) return sessions;
  return upsertSession(sessions, { ...existing, state });
}
