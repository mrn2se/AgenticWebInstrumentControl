/**
 * A minimal observable store: holds one immutable state value, notifies
 * subscribers on change. Deliberately tiny — no framework, per Q-06 — and
 * generic so it isn't coupled to `AppState`.
 */
export class Store<T> {
  private state: T;
  private readonly listeners = new Set<(state: T) => void>();

  constructor(initial: T) {
    this.state = initial;
  }

  get(): T {
    return this.state;
  }

  /** Replaces the state and notifies subscribers. */
  set(next: T): void {
    this.state = next;
    this.emit();
  }

  /** Derives the next state from the current one; the common case. */
  update(fn: (state: T) => T): void {
    this.set(fn(this.state));
  }

  /** Subscribes and immediately invokes `listener` with the current state. */
  subscribe(listener: (state: T) => void): () => void {
    this.listeners.add(listener);
    listener(this.state);
    return () => {
      this.listeners.delete(listener);
    };
  }

  private emit(): void {
    for (const listener of this.listeners) {
      listener(this.state);
    }
  }
}
