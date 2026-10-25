import { AwicModule } from "../../shell/module";
import { registerModule } from "../../shell/registry";
import { requestModuleActivation } from "../../shell/activation";
import type { TerminalSourceInfo, UsbTreeResult } from "../../proto/types";
import { toErrorInfo } from "../../ws/errors";
import { UsbTreeModel, type PortRow, collisionNote } from "./tree-model";
import { PortPowerTracker, portKey } from "./port-power";
import { toggleViewModel, type ToggleViewModel } from "./toggle-view";
import { PortProtectTracker } from "./port-protect";
import { lockChangeNeedsConfirmation, lockViewModel, protectedValueForChoice, type LockChoice, type LockViewModel } from "./lock-view";
import { serialSourceForDevice } from "./terminal-link";

const TAG = "awic-module-usb";

const REASON_CLASS = "text-xs text-awic-fg-muted";
const ERROR_CLASS = "text-xs text-awic-danger";

/** One power state, one look — never just a bare "On"/"Off" word with no other cue. */
const TOGGLE_BASE = "min-w-[4.5rem] rounded-full border px-2.5 py-0.5 text-xs font-semibold transition-colors";
const TOGGLE_ON = "border-awic-ok bg-awic-ok-bg text-awic-ok";
const TOGGLE_OFF = "border-awic-border bg-awic-bg-raised text-awic-fg-muted";
const TOGGLE_UNKNOWN = "border-awic-warn border-dashed bg-awic-warn-bg text-awic-warn";
const TOGGLE_PENDING = "border-awic-border bg-awic-bg-raised text-awic-fg-muted";

const LOCK_BTN_BASE = "px-2 py-0.5 text-[11px] font-medium transition-colors disabled:cursor-not-allowed disabled:opacity-55";
const LOCK_BTN_ACTIVE = "bg-awic-accent text-awic-accent-fg";
const LOCK_BTN_INACTIVE = "bg-awic-bg-raised text-awic-fg-muted enabled:hover:bg-awic-bg enabled:cursor-pointer";

function formatVidPid(vid: number, pid: number): string {
  return `${vid.toString(16).padStart(4, "0")}:${pid.toString(16).padStart(4, "0")}`;
}

/**
 * What a port was named with `usb.label`, shown before the device itself:
 * bench wiring is the one thing the tree cannot discover, and the point of
 * the label is to be read *before* switching the port (live test F-08).
 */
function describeLabel(row: PortRow): string {
  if (!row.label && !row.dut) return "";
  const name = row.label ?? "device under test";
  return row.dut ? `[DUT] ${name}  ` : `${name}  `;
}

function describeIdentity(row: PortRow): string {
  const named = describeLabel(row);
  if (row.vid === null || row.pid === null) return `${named}unknown device`;
  const vidPid = formatVidPid(row.vid, row.pid);
  if (row.isHub) {
    const ppps = row.ppps ? "ppps" : "no ppps";
    const ports = row.portCount !== null ? `${row.portCount} ports` : "hub";
    return `${named}${vidPid}  ${row.product ?? "USB hub"}, ${ports}, ${ppps}`;
  }
  return `${named}${vidPid}  ${row.product ?? "unknown device"}`;
}

function describePowerLabel(row: PortRow): string {
  if (row.power === true) return "[powered]";
  if (row.power === false) return "[off]";
  return "";
}

/** Text + look for the power toggle button — one unmistakable state, never a bare "On". */
function toggleButtonState(toggle: ToggleViewModel): { text: string; className: string } {
  if (toggle.pending) return { text: "…", className: `${TOGGLE_BASE} ${TOGGLE_PENDING} cursor-wait opacity-70` };
  const disabledLook = toggle.enabled ? "cursor-pointer" : "cursor-not-allowed opacity-60";
  if (toggle.on === true) return { text: "On", className: `${TOGGLE_BASE} ${TOGGLE_ON} ${disabledLook}` };
  if (toggle.on === false) return { text: "Off", className: `${TOGGLE_BASE} ${TOGGLE_OFF} ${disabledLook}` };
  return { text: "Unknown", className: `${TOGGLE_BASE} ${TOGGLE_UNKNOWN} ${disabledLook}` };
}

/**
 * Device-tree module (SPECIFICATION.md §4, §11; requirement N-1). Renders
 * the hub topology from `usb.tree`, offers per-port power toggles that wait
 * for server confirmation before changing what they show, and stays live
 * across hot-plug via `usb.changed` (the bus) and `device.*` (the registry).
 *
 * The tree and the toggle logic are deliberately pure (`tree-model.ts`,
 * `port-power.ts`, `toggle-view.ts`) and unit-tested there; this file is the
 * thin DOM layer wiring them to `ModuleContext`, plus the specifics that are
 * genuinely about presentation (indentation, labels, click handling).
 */
class UsbTreeModule extends AwicModule {
  private readonly model = new UsbTreeModel();
  private readonly power = new PortPowerTracker();
  private readonly protect = new PortProtectTracker();
  private readonly rowElements = new Map<string, HTMLLIElement>();
  private readonly busHeaders = new Map<number, HTMLLIElement>();

  // Assigned in `onInit()`, not the constructor: a custom element's
  // constructor must not leave `this` with attributes or children once
  // `document.createElement` returns, which setting `className`/`innerHTML`
  // here would do (see `shell/module.ts`). `onInit()` runs after
  // construction completes and before this module is ever shown, so it is
  // the right place to build this module's light DOM.
  private listEl!: HTMLUListElement;
  private statusEl!: HTMLElement;

  private unsubscribeStore: (() => void) | null = null;
  private unsubscribeEvents: (() => void) | null = null;
  private unsubscribeConnection: (() => void) | null = null;

  private lastResult: UsbTreeResult | null = null;
  private loadError: string | null = null;
  private refreshing = false;
  private refreshQueued = false;

  /** Terminal sources, refetched alongside the tree, so the terminal icon
   * (SPEC §11.2.1) can be shown against whichever port has a serial
   * interface right now. */
  private terminalSources: readonly TerminalSourceInfo[] = [];

  protected override onInit(): void {
    // `min-h-0`: as a flex child its minimum height is otherwise its
    // content's, and a tall tree grew the page instead of scrolling here.
    this.className = "block min-h-0 flex-1 overflow-auto p-6";
    this.innerHTML = `
      <div class="mb-3 flex items-center justify-between gap-4">
        <h2 class="text-xl font-semibold">USB devices</h2>
        <button id="refresh" type="button" class="rounded-md border border-awic-border bg-awic-bg-raised px-3 py-1.5 text-sm hover:bg-awic-bg">Refresh</button>
      </div>
      <p id="usb-status" hidden class="mb-3 rounded-md bg-awic-danger-bg px-3 py-2 text-sm text-awic-danger"></p>
      <ul id="tree" class="m-0 list-none p-0"></ul>
    `;
    this.listEl = this.requireEl<HTMLUListElement>("#tree");
    this.statusEl = this.requireEl("#usb-status");
    this.requireEl("#refresh").addEventListener("click", () => this.requestRefresh());
  }

  override activate(): void {
    this.unsubscribeStore = this.ctx.store.subscribe(() => this.reapplyOverlay());
    this.unsubscribeEvents = this.ctx.ws.events.on((event) => {
      // `usb.changed` is the physical bus (any device, incl. an FX2 instrument
      // re-enumerating after power-on, or a port's protection changing —
      // change: "protection", SPEC §4.4); `device.*` is the instrument
      // registry. Either can change what the tree should show. The lock
      // control never applies a protection change itself (§11): this
      // re-fetch is what actually moves a row after usb.protect settles.
      if (
        event.event === "usb.changed" ||
        event.event === "device.added" ||
        event.event === "device.removed" ||
        event.event === "device.state"
      ) {
        this.requestRefresh();
      }
      if (event.event === "term.sources") {
        this.refreshTerminalSources();
      }
    });
    this.unsubscribeConnection = this.ctx.ws.connectionState.on((state) => {
      if (state === "connected") {
        this.requestRefresh();
        this.refreshTerminalSources();
      }
    });
    this.requestRefresh();
    this.refreshTerminalSources();
  }

  /** Best-effort: a device with a terminal icon simply doesn't get one if
   * this fails (e.g. the WS is briefly reconnecting) — never a hard error
   * on top of the tree's own status line. */
  private refreshTerminalSources(): void {
    this.ctx.ws.send("term.sources", {}).then(
      (result) => {
        this.terminalSources = result.sources;
        this.reapplyOverlay();
      },
      () => {},
    );
  }

  override deactivate(): void {
    this.unsubscribeStore?.();
    this.unsubscribeStore = null;
    this.unsubscribeEvents?.();
    this.unsubscribeEvents = null;
    this.unsubscribeConnection?.();
    this.unsubscribeConnection = null;
  }

  /** Fetches a fresh, authoritative tree. Coalesces overlapping calls into one in-flight request. */
  private requestRefresh(): void {
    if (this.refreshing) {
      this.refreshQueued = true;
      return;
    }
    this.refreshing = true;
    this.ctx.ws
      .send("usb.tree", {})
      .then((result) => {
        this.loadError = null;
        this.lastResult = result;
        this.applyTree(result);
      })
      .catch((err: unknown) => {
        this.loadError = toErrorInfo(err).msg;
        this.renderStatus();
      })
      .finally(() => {
        this.refreshing = false;
        if (this.refreshQueued) {
          this.refreshQueued = false;
          this.requestRefresh();
        }
      });
  }

  private applyTree(result: UsbTreeResult): void {
    const devices = this.ctx.store.get().devices;
    const outcome = this.model.apply(result, devices);
    this.renderRows(outcome.rows, outcome.removedKeys);
    this.renderStatus();
  }

  /** Re-renders from the last-known tree with a fresh device-state overlay — no network round trip. */
  private reapplyOverlay(): void {
    if (!this.lastResult) return;
    this.applyTree(this.lastResult);
  }

  private handleToggle(row: PortRow): void {
    if (!row.switchable || row.parentHub === null || row.portNumber === null) return;
    const hub = row.parentHub;
    const port = row.portNumber;
    const key = portKey(hub, port);
    const nextOn = !(row.power ?? false);

    // Pending only; the server's response (via a fresh usb.tree fetch below)
    // is the sole source of truth for what the port actually does next.
    this.power.begin(key);
    this.reapplyOverlay();

    this.ctx.ws.send("usb.power", { hub, port, on: nextOn }).then(
      () => {
        this.power.resolve(key);
        this.requestRefresh();
      },
      (err: unknown) => {
        this.power.fail(key, toErrorInfo(err));
        this.reapplyOverlay();
      },
    );
  }

  /**
   * Sends `usb.protect` for a lock-control click. Server-authoritative, same
   * discipline as `handleToggle`: only the tracker's pending flag changes
   * here, never `row.protected`/`row.userChoice` — the tree re-fetch after
   * the response (and, in general, the server's `usb.changed` /
   * `change: "protection"` event) is what actually moves the row.
   *
   * Unlocking a port the rule protects automatically must be confirmed
   * first (task brief §2) — the one guardrail against a single click cutting
   * a hub or keyboard from the UI.
   */
  private handleLock(row: PortRow, hub: string, port: number, choice: LockChoice): void {
    if (lockChangeNeedsConfirmation(row, choice)) {
      // The rule's own reason — the actual risk — not the current state,
      // which for a port the user locked first would only say "locked".
      const reason = row.derivedProtection ?? "this port is protected by default";
      const confirmed = window.confirm(
        `Unlock port ${port} on ${hub}?\n\nProtected by default: ${reason}.\n\nUnlocking allows switching this port's power off.`,
      );
      if (!confirmed) return;
    }

    const key = portKey(hub, port);
    this.protect.begin(key);
    this.reapplyOverlay();

    this.ctx.ws.send("usb.protect", { hub, port, protected: protectedValueForChoice(choice) }).then(
      () => {
        this.protect.resolve(key);
        this.requestRefresh();
      },
      (err: unknown) => {
        this.protect.fail(key, toErrorInfo(err));
        this.reapplyOverlay();
      },
    );
  }

  private renderStatus(): void {
    if (this.loadError) {
      this.statusEl.hidden = false;
      this.statusEl.textContent = `Could not load the USB tree: ${this.loadError}`;
    } else {
      this.statusEl.hidden = true;
      this.statusEl.textContent = "";
    }
  }

  private renderRows(rows: readonly PortRow[], removedKeys: readonly string[]): void {
    for (const key of removedKeys) {
      this.rowElements.get(key)?.remove();
      this.rowElements.delete(key);
    }

    const usedBuses = new Set<number>();
    let lastBus: number | null = null;

    for (const row of rows) {
      if (row.depth === 0 && row.bus !== lastBus) {
        lastBus = row.bus;
        usedBuses.add(row.bus);
        let header = this.busHeaders.get(row.bus);
        if (!header) {
          header = document.createElement("li");
          header.className = "pt-3 pb-1 text-xs font-semibold uppercase tracking-wide text-awic-fg-muted";
          this.busHeaders.set(row.bus, header);
        }
        header.textContent = `Bus ${String(row.bus).padStart(3, "0")}`;
        this.listEl.appendChild(header);
      }

      let li = this.rowElements.get(row.key);
      if (!li) {
        li = document.createElement("li");
        this.rowElements.set(row.key, li);
      }
      this.renderRow(li, row);
      this.listEl.appendChild(li); // re-appending an existing node moves it into place in order
    }

    for (const [bus, header] of this.busHeaders) {
      if (!usedBuses.has(bus)) {
        header.remove();
        this.busHeaders.delete(bus);
      }
    }
  }

  private renderRow(li: HTMLLIElement, row: PortRow): void {
    li.className = `flex flex-wrap items-center gap-2 border-b border-awic-border py-1.5 text-sm ${row.empty ? "italic text-awic-fg-muted" : ""}`;
    // Per-level indentation is inherently a dynamic, per-row numeric value —
    // set directly rather than via a shared CSS rule (SPEC §11: readable
    // tree indentation).
    li.style.paddingLeft = `${row.depth * 1.5}rem`;
    li.dataset.key = row.key;
    li.replaceChildren();

    if (row.depth > 0) {
      li.appendChild(this.span("text-awic-fg-muted", "└─"));
    }
    if (row.portNumber !== null) {
      li.appendChild(this.span("min-w-[4.5rem] text-awic-fg-muted", `port ${row.portNumber}`));
    }
    li.appendChild(this.span("font-mono text-[0.92em]", row.empty ? describeLabel(row) + "(empty)" : describeIdentity(row)));

    const note = collisionNote(row.vid, row.pid);
    if (note) li.appendChild(this.span(REASON_CLASS, `(${note})`));

    if (row.deviceState && row.deviceState !== "ready") {
      li.appendChild(this.span(REASON_CLASS, `[${row.deviceState}]`));
    }

    const terminalSource = row.empty ? undefined : serialSourceForDevice(this.terminalSources, row.key);
    if (terminalSource) {
      li.appendChild(this.renderTerminalButton(terminalSource));
    }

    const powerLabel = describePowerLabel(row);
    if (powerLabel) {
      li.appendChild(this.span(`min-w-[4.5rem] text-xs ${row.power ? "text-awic-ok" : "text-awic-fg-muted"}`, powerLabel));
    }

    // The depth-0 root row has no `UsbPort` behind it (the real root-hub
    // port isn't modeled on the wire) — nothing to switch, so no control at
    // all, not even a disabled one.
    if (!row.hasToggle || row.parentHub === null || row.portNumber === null) return;

    const key = portKey(row.parentHub, row.portNumber);
    const toggle = toggleViewModel(row, this.power.get(key));

    li.appendChild(this.renderToggleButton(row, toggle));

    // The protection reason is shown exactly once (task brief): here only
    // when the control is disabled for a reason OTHER than protection (a
    // hub lacking ppps), or when there is no lock control to show it
    // instead. When the port IS protected AND gets a lock control below,
    // that is the one place that names it, right next to the state that
    // controls it.
    const reasonShownByLock = row.protected && row.powerSwitchable;
    if (toggle.reason && !reasonShownByLock) li.appendChild(this.span(REASON_CLASS, toggle.reason));
    if (toggle.errorMessage) li.appendChild(this.span(ERROR_CLASS, toggle.errorMessage));

    // Every power-switchable port gets a lock control (task brief §2),
    // independent of whether it happens to be protected right now.
    if (row.powerSwitchable) {
      li.appendChild(this.renderLockControl(row, row.parentHub, row.portNumber));
    }
  }

  /** SPEC §11.2.1: a terminal icon on a port whose device has a serial
   * source — clicking it switches to the Terminal tab and opens that source
   * (`shell/activation.ts`'s cross-module channel). */
  private renderTerminalButton(source: TerminalSourceInfo): HTMLButtonElement {
    const button = document.createElement("button");
    button.type = "button";
    button.className =
      "rounded-md border border-awic-border bg-awic-bg-raised px-1.5 py-0.5 text-xs hover:bg-awic-bg disabled:cursor-not-allowed disabled:opacity-55";
    button.textContent = ">_";
    button.title = source.available ? `Open terminal: ${source.label}` : (source.unavailable_reason ?? "unavailable");
    button.disabled = !source.available;
    button.addEventListener("click", (ev) => {
      ev.stopPropagation();
      requestModuleActivation("terminal", source.source);
    });
    return button;
  }

  private renderLockControl(row: PortRow, hub: string, port: number): HTMLElement {
    const view = lockViewModel(row, this.protect.get(portKey(hub, port)));
    const wrapper = document.createElement("span");
    wrapper.className = "inline-flex items-center gap-2";

    const segmented = document.createElement("span");
    segmented.className = "inline-flex divide-x divide-awic-border overflow-hidden rounded-full border border-awic-border";

    const options: Array<{ choice: LockChoice; label: string }> = [
      { choice: "automatic", label: "Auto" },
      { choice: "locked", label: "Locked" },
      { choice: "unlocked", label: "Unlocked" },
    ];
    for (const { choice, label } of options) {
      const button = document.createElement("button");
      button.type = "button";
      const active = choice === view.choice;
      button.className = `${LOCK_BTN_BASE} ${active ? LOCK_BTN_ACTIVE : LOCK_BTN_INACTIVE}`;
      button.textContent = label;
      button.disabled = view.pending || active;
      button.addEventListener("click", () => this.handleLock(row, hub, port, choice));
      segmented.appendChild(button);
    }
    wrapper.appendChild(segmented);

    this.appendLockReason(wrapper, view);
    if (view.errorMessage) wrapper.appendChild(this.span(ERROR_CLASS, view.errorMessage));

    return wrapper;
  }

  /** The single place a protection reason is shown for a protected, power-switchable port (task brief §2). */
  private appendLockReason(wrapper: HTMLElement, view: LockViewModel): void {
    if (view.protected && view.protectionReason) {
      wrapper.appendChild(this.span(REASON_CLASS, view.protectionReason));
    }
  }

  private renderToggleButton(row: PortRow, toggle: ToggleViewModel): HTMLButtonElement {
    const button = document.createElement("button");
    button.type = "button";
    const { text, className } = toggleButtonState(toggle);
    button.className = className;
    button.disabled = !toggle.enabled || toggle.pending;
    if (!toggle.enabled && toggle.reason) button.title = toggle.reason;
    button.textContent = text;
    if (toggle.enabled) {
      button.addEventListener("click", () => this.handleToggle(row));
    }
    return button;
  }

  private span(className: string, text: string): HTMLSpanElement {
    const el = document.createElement("span");
    el.className = className;
    el.textContent = text;
    return el;
  }
}

customElements.define(TAG, UsbTreeModule);

registerModule({
  id: "usb",
  title: "USB",
  factory: () => document.createElement(TAG) as UsbTreeModule,
});
