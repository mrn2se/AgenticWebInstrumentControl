/**
 * Per-stream sequence tracking for sample frames. A gap must be visible to
 * the user, not silently absorbed (SPECIFICATION.md §3.2) — this is the
 * piece that turns a raw `sequence` number into
 * an explicit "N frames were dropped" fact.
 */
export interface FrameGapNotice {
  readonly streamId: number;
  readonly expectedSequence: number;
  readonly actualSequence: number;
  /** 0 for an out-of-order/duplicate frame rather than a forward gap. */
  readonly droppedCount: number;
}

export class SequenceTracker {
  private readonly lastSequence = new Map<number, number>();

  /**
   * Records an observed sequence number for `streamId`. Returns `null` for
   * the first frame on a stream, or for an exactly-contiguous sequence.
   * Otherwise returns a {@link FrameGapNotice} describing the discontinuity.
   * `sequence` is a wire `u32`, so contiguity is checked modulo 2^32.
   */
  observe(streamId: number, sequence: number): FrameGapNotice | null {
    const last = this.lastSequence.get(streamId);
    this.lastSequence.set(streamId, sequence);

    if (last === undefined) return null;

    const expected = (last + 1) >>> 0;
    if (sequence === expected) return null;

    const droppedCount = sequence > expected ? sequence - expected : 0;
    return { streamId, expectedSequence: expected, actualSequence: sequence, droppedCount };
  }

  /** Forgets a stream, e.g. once its capture has stopped. */
  reset(streamId: number): void {
    this.lastSequence.delete(streamId);
  }
}
