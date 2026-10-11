/**
 * Two time cursors showing Δt and 1/Δt (SPECIFICATION.md §5, frontend
 * cursor requirement). Pure so the arithmetic — and its edge case, cursors
 * placed at the same instant — is testable without any renderer involved.
 */
export interface CursorPair {
  readonly a: number;
  readonly b: number;
}

export interface CursorReadout {
  readonly deltaSeconds: number;
  /** `null` when the cursors coincide — `1/0` is not a meaningful frequency. */
  readonly frequencyHz: number | null;
}

export function cursorReadout(cursors: CursorPair): CursorReadout {
  const deltaSeconds = Math.abs(cursors.b - cursors.a);
  return { deltaSeconds, frequencyHz: deltaSeconds > 0 ? 1 / deltaSeconds : null };
}
