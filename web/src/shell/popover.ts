/**
 * A small reusable popover helper (SPECIFICATION.md §11.3): the toolbar's
 * settings buttons (channels/ranges, trigger, acquisition, display) each
 * open one of these instead of a stacked settings section. A popover must
 * "close on Escape and outside click" and must "never push the plot down" —
 * the second half is satisfied by the caller positioning the panel
 * `absolute` (see `modules/graph/index.ts`), not by anything here.
 *
 * The open/close *decision* is a pure reducer (`nextOpenState`,
 * `isOutsideAll`) so it can be unit-tested without a DOM — the vitest
 * config here runs in a plain Node environment (see `theme-controller.ts`'s
 * same split between a pure core and injectable DOM-ish ports). `Popover`
 * itself is the thin real-DOM wiring on top, exercised by the e2e suite
 * instead (same division as `render/webgl2.ts` vs. its `webgl2-*.ts`
 * siblings).
 */

/** The events that can change whether a popover is open. */
export type PopoverAction =
  | { readonly type: "toggle" }
  | { readonly type: "open" }
  | { readonly type: "close" }
  | { readonly type: "escape" }
  | { readonly type: "outside-click" };

/** Pure state transition — no DOM, no side effects. */
export function nextOpenState(open: boolean, action: PopoverAction): boolean {
  switch (action.type) {
    case "toggle":
      return !open;
    case "open":
      return true;
    case "close":
    case "escape":
    case "outside-click":
      return false;
    default: {
      const exhaustive: never = action;
      throw new Error(`nextOpenState: unhandled action ${JSON.stringify(exhaustive)}`);
    }
  }
}

export function isEscapeKey(key: string): boolean {
  return key === "Escape";
}

/** Minimal structural shape `isOutsideAll` needs — real `Node`s and test fakes both satisfy it. */
export interface ContainmentCheckable {
  contains(other: unknown): boolean;
}

/**
 * `true` when `target` sits outside every one of `containers` (the trigger
 * button and the popover panel — a click on the button itself must not
 * immediately reopen what its own listener just closed, and a click inside
 * the panel must not close it).
 */
export function isOutsideAll(target: unknown, containers: readonly ContainmentCheckable[]): boolean {
  return containers.every((c) => !c.contains(target));
}

export interface PopoverOptions {
  /** Called after every state change, with the new `open` value — apply it (e.g. `panel.hidden = !open`) here. */
  readonly onChange: (open: boolean) => void;
}

/**
 * Wires one anchor/panel pair to document-level `keydown`/`pointerdown`
 * listeners implementing the reducer above. Each popover gets its own
 * instance; `destroy()` removes the listeners (call it from the owning
 * module's `deactivate()`, exactly like every other subscription there).
 */
export class Popover {
  private open = false;

  private readonly handleKeydown = (e: KeyboardEvent): void => {
    if (!this.open || !isEscapeKey(e.key)) return;
    this.apply(nextOpenState(this.open, { type: "escape" }));
  };

  private readonly handlePointerDown = (e: PointerEvent): void => {
    if (!this.open) return;
    if (isOutsideAll(e.target, [this.anchor, this.panel])) {
      this.apply(nextOpenState(this.open, { type: "outside-click" }));
    }
  };

  constructor(
    private readonly anchor: HTMLElement,
    private readonly panel: HTMLElement,
    private readonly options: PopoverOptions,
  ) {
    document.addEventListener("keydown", this.handleKeydown);
    document.addEventListener("pointerdown", this.handlePointerDown);
  }

  toggle(): void {
    this.apply(nextOpenState(this.open, { type: "toggle" }));
  }

  close(): void {
    if (this.open) this.apply(nextOpenState(this.open, { type: "close" }));
  }

  isOpen(): boolean {
    return this.open;
  }

  private apply(open: boolean): void {
    this.open = open;
    this.options.onChange(open);
  }

  destroy(): void {
    document.removeEventListener("keydown", this.handleKeydown);
    document.removeEventListener("pointerdown", this.handlePointerDown);
  }
}
