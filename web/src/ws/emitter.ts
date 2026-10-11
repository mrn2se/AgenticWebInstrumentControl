/**
 * A minimal typed pub/sub channel. `WsClient` uses one of these per event
 * kind (server events, RPC errors, sample frames, sequence gaps, connection
 * state) instead of a single untyped `EventTarget`, so subscribers get a
 * concrete payload type with no casting.
 */
export class Emitter<T> {
  private readonly listeners = new Set<(value: T) => void>();

  /** Subscribes and returns an unsubscribe function. */
  on(listener: (value: T) => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  emit(value: T): void {
    for (const listener of this.listeners) {
      listener(value);
    }
  }
}
