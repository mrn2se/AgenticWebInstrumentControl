import "@xterm/xterm/css/xterm.css";
import { Terminal, type ITheme } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";

import { AwicModule } from "../../shell/module";
import { registerModule } from "../../shell/registry";
import type { ChannelRole, DecodeOpenSpec, DecoderInfo, RecListResult, SerialSettings, TerminalSourceInfo } from "../../proto/types";
import { buildDecodeConfig, initialOptionInputs, type RawOptionValue } from "../decoder-form";
import type { DecodedTermFrame } from "../../ws/term-frame";
import { toErrorInfo } from "../../ws/errors";
import { findSource, groupSources } from "./sources-view";
import {
  BAUD_PRESETS,
  DATA_BITS_OPTIONS,
  DEFAULT_SERIAL_SETTINGS,
  PARITY_OPTIONS,
  STOP_BITS_OPTIONS,
  isValidCustomBaud,
} from "./serial-settings";
import {
  applySessionState,
  isWritable,
  removeSession,
  stateBadge,
  upsertSession,
  type SessionMap,
  type SessionViewModel,
} from "./sessions";
import { filterLines, lineMatchesFilter, type LogFilterLevel } from "./log-filter";
import { LineEditor } from "./line-editor";

const TAG = "awic-module-terminal";

/** Kept close to the app's own `--color-awic-*` palette rather than xterm's
 * defaults, so the terminal doesn't look like a foreign widget dropped onto
 * the page (SPEC §11.1: plots — and here, the terminal — follow the theme).
 * Hand-picked hex, not read from `getComputedStyle` (unlike
 * `render/plotly-theme.ts`): xterm wants a full `ITheme` up front rather
 * than a handful of colors, and these only need to roughly track the
 * `--color-awic-*` tokens in `styles/index.css`, not match them exactly. */
const LIGHT_THEME: ITheme = {
  background: "#ffffff",
  foreground: "#1a1f26",
  cursor: "#1a1f26",
  selectionBackground: "#cfe0f5",
  black: "#1a1f26",
  brightBlack: "#5b6472",
};

const DARK_THEME: ITheme = {
  background: "#16171a",
  foreground: "#e7e8ea",
  cursor: "#e7e8ea",
  selectionBackground: "#33455e",
  black: "#e7e8ea",
  brightBlack: "#9a9ca3",
};

const SELECT = "rounded border border-awic-border bg-awic-bg px-1.5 py-0.5 text-sm text-awic-fg";
// Colorless structural base plus exactly one of a neutral/primary (or
// inactive/active) color variant — never both a neutral and a colored
// background utility on the same element. Tailwind utility precedence is
// decided by each utility's position in the *generated stylesheet*, not by
// order in the `class` attribute, so combining `bg-awic-bg-raised` (in a
// "base" class) with `bg-awic-accent` (in an "active" class) on one element
// lets either one win depending on scan order — silently, with no build
// warning. Splitting them so at most one background-color utility ever
// applies to a given element is what avoids that.
const BUTTON_BASE = "rounded-md border px-3 py-1.5 text-sm font-medium disabled:cursor-not-allowed disabled:opacity-60";
const BUTTON_NEUTRAL = "border-awic-border bg-awic-bg-raised hover:bg-awic-bg";
const BUTTON_PRIMARY = "border-awic-accent bg-awic-accent text-awic-accent-fg hover:bg-awic-accent";
const POPOVER_ROW = "flex items-center gap-2 text-sm";
const TAB_BASE = "flex cursor-pointer items-center gap-1.5 whitespace-nowrap rounded-md border px-2 py-1 text-sm";
const TAB_INACTIVE = "border-awic-border bg-awic-bg-raised";
const TAB_ACTIVE = "border-awic-accent bg-awic-accent text-awic-accent-fg";

/** Lines kept per log session for re-deriving the view when the level
 * filter changes — capped well above the server's own 1000-line ring
 * (SPEC §12) so switching filters within one open session never loses
 * anything the server would have replayed. */
const LOG_BUFFER_LINES = 2000;

interface OpenPane {
  readonly container: HTMLDivElement;
  readonly term: Terminal;
  readonly fitAddon: FitAddon;
  /** Whether `term.open(container)` has run yet — deferred until this pane
   * is first shown; see `ensurePane`. */
  opened: boolean;
  /** `log` sessions only: every line seen, newest last, so the level filter
   * can be changed after the fact without losing lines it had filtered out
   * before. `null` for non-log sessions, which have no line structure to
   * filter. */
  logLines: string[] | null;
  logFilter: LogFilterLevel;
  /** `scpi` sessions only: the local line editor, so output can be written
   * around a half-typed line instead of into it. */
  lineEditor: LineEditor | null;
}

/**
 * The terminal module (SPEC §11.2/§11.2.1): a source picker over
 * `term.sources`, one xterm instance per open session (each a sub-tab), and
 * decode of incoming `0xA7` output frames dispatched by `WsClient`. Serial
 * sessions accept input (`onData`/`onBinary` -> `term.write`); `log`
 * sessions are read-only. Light DOM throughout (`shell/module.ts`) — xterm's
 * own stylesheet is a normal top-level import for the same reason.
 */
class TerminalModule extends AwicModule {
  // Assigned in `onInit()`, not the constructor — see `shell/module.ts`.
  private sourceSelect!: HTMLSelectElement;
  private settingsButton!: HTMLButtonElement;
  private popoverEl!: HTMLElement;
  private openButton!: HTMLButtonElement;
  private reasonEl!: HTMLElement;
  private statusEl!: HTMLElement;
  private tabsEl!: HTMLElement;
  private panesEl!: HTMLElement;
  private emptyEl!: HTMLElement;

  private serialSettingsEl!: HTMLElement;
  private baudSelect!: HTMLSelectElement;
  private customBaudInput!: HTMLInputElement;
  private dataBitsSelect!: HTMLSelectElement;
  private paritySelect!: HTMLSelectElement;
  private stopBitsSelect!: HTMLSelectElement;
  private levelFilterLabel!: HTMLElement;
  private levelFilterSelect!: HTMLSelectElement;

  private decodeSettingsEl!: HTMLElement;
  private decodeRecSelect!: HTMLSelectElement;
  private decodeDecoderSelect!: HTMLSelectElement;
  private decodeRolesEl!: HTMLElement;
  private decodeOptionsEl!: HTMLElement;
  private decodeFormErrorEl!: HTMLElement;
  private decoders: readonly DecoderInfo[] = [];
  private decodeRecordings: RecListResult | null = null;
  private decodeRoleBindings: Map<ChannelRole, number | undefined> = new Map();
  private decodeOptionInputs: Map<string, RawOptionValue> = new Map();

  private sessions: SessionMap = new Map();
  private readonly panes = new Map<number, OpenPane>();
  private activeSessionId: number | null = null;
  private lastSources: readonly TerminalSourceInfo[] = [];

  private unsubEvents: (() => void) | null = null;
  private unsubTermFrames: (() => void) | null = null;
  private unsubTheme: (() => void) | null = null;
  private unsubConnection: (() => void) | null = null;
  private resizeObserver: ResizeObserver | null = null;

  protected override onInit(): void {
    // `min-h-0` on the host and on `#panes` below is what lets the pane area
    // actually shrink to fill only the leftover height under the toolbar/
    // tabs rows instead of pushing the page taller (SPEC §11.3: no page
    // scroll) — a flex child defaults to a `min-height` of its content's
    // natural size, which for an xterm instance is effectively unbounded.
    this.className = "flex min-h-0 flex-1 flex-col overflow-hidden";
    this.innerHTML = `
      <div class="relative flex flex-none flex-wrap items-center gap-2 border-b border-awic-border bg-awic-bg-raised px-4 py-2">
        <select id="source" class="${SELECT}"></select>
        <button id="settings" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}" title="Serial settings">⚙ Settings</button>
        <button id="open" type="button" class="${BUTTON_BASE} ${BUTTON_PRIMARY}">Open</button>
        <label id="level-filter-label" hidden class="flex items-center gap-1.5 text-sm text-awic-fg-muted">
          Level
          <select id="level-filter" class="${SELECT}">
            <option value="all">All</option>
            <option value="info">Info+</option>
            <option value="warn">Warn+</option>
            <option value="error">Error+</option>
          </select>
        </label>
        <span id="reason" class="text-xs text-awic-fg-muted"></span>
        <span id="status" class="text-xs text-awic-danger"></span>
        <div id="popover" hidden class="absolute left-4 top-full z-10 mt-1 flex flex-col gap-2 rounded-lg border border-awic-border bg-awic-bg-raised p-3 shadow-lg">
          <div id="serial-settings" class="flex flex-col gap-2">
            <div class="${POPOVER_ROW}">
              <label class="min-w-[5rem] text-awic-fg-muted" for="baud">Baud</label>
              <select id="baud" class="${SELECT}"></select>
              <input id="custom-baud" type="number" min="1" placeholder="custom" hidden class="w-24 ${SELECT}" />
            </div>
            <div class="${POPOVER_ROW}">
              <label class="min-w-[5rem] text-awic-fg-muted" for="data-bits">Data bits</label>
              <select id="data-bits" class="${SELECT}"></select>
            </div>
            <div class="${POPOVER_ROW}">
              <label class="min-w-[5rem] text-awic-fg-muted" for="parity">Parity</label>
              <select id="parity" class="${SELECT}"></select>
            </div>
            <div class="${POPOVER_ROW}">
              <label class="min-w-[5rem] text-awic-fg-muted" for="stop-bits">Stop bits</label>
              <select id="stop-bits" class="${SELECT}"></select>
            </div>
          </div>
          <div id="decode-settings" hidden class="flex flex-col gap-2">
            <div class="${POPOVER_ROW}">
              <label class="min-w-[5rem] text-awic-fg-muted" for="decode-rec">Recording</label>
              <select id="decode-rec" class="${SELECT}"></select>
            </div>
            <div class="${POPOVER_ROW}">
              <label class="min-w-[5rem] text-awic-fg-muted" for="decode-decoder">Decoder</label>
              <select id="decode-decoder" class="${SELECT}"></select>
            </div>
            <div id="decode-roles" class="flex flex-col gap-1"></div>
            <div id="decode-options" class="flex flex-col gap-1"></div>
            <p id="decode-form-error" class="text-xs text-awic-danger empty:hidden"></p>
          </div>
        </div>
      </div>
      <div id="tabs" class="flex flex-none gap-1 overflow-x-auto border-b border-awic-border px-3 py-1.5 empty:hidden"></div>
      <div id="panes" class="relative min-h-0 flex-1 overflow-hidden bg-awic-bg">
        <p id="empty" class="absolute inset-0 flex items-center justify-center text-awic-fg-muted">Open a source above to start a terminal session.</p>
      </div>
    `;
    this.sourceSelect = this.requireEl("#source");
    this.settingsButton = this.requireEl("#settings");
    this.popoverEl = this.requireEl("#popover");
    this.openButton = this.requireEl("#open");
    this.reasonEl = this.requireEl("#reason");
    this.statusEl = this.requireEl("#status");
    this.tabsEl = this.requireEl("#tabs");
    this.panesEl = this.requireEl("#panes");
    this.emptyEl = this.requireEl("#empty");

    this.serialSettingsEl = this.requireEl("#serial-settings");
    this.baudSelect = this.requireEl("#baud");
    this.customBaudInput = this.requireEl("#custom-baud");
    this.dataBitsSelect = this.requireEl("#data-bits");
    this.paritySelect = this.requireEl("#parity");
    this.stopBitsSelect = this.requireEl("#stop-bits");
    this.levelFilterLabel = this.requireEl("#level-filter-label");
    this.levelFilterSelect = this.requireEl("#level-filter");
    this.populateSettingsOptions();

    this.decodeSettingsEl = this.requireEl("#decode-settings");
    this.decodeRecSelect = this.requireEl("#decode-rec");
    this.decodeDecoderSelect = this.requireEl("#decode-decoder");
    this.decodeRolesEl = this.requireEl("#decode-roles");
    this.decodeOptionsEl = this.requireEl("#decode-options");
    this.decodeFormErrorEl = this.requireEl("#decode-form-error");
    this.decodeRecSelect.addEventListener("change", () => this.onDecodeSelectionChanged());
    this.decodeDecoderSelect.addEventListener("change", () => this.onDecodeSelectionChanged());

    this.settingsButton.addEventListener("click", () => {
      this.popoverEl.hidden = !this.popoverEl.hidden;
    });
    this.baudSelect.addEventListener("change", () => {
      this.customBaudInput.hidden = this.baudSelect.value !== "custom";
    });
    this.sourceSelect.addEventListener("change", () => this.renderSelectionDetails());
    this.openButton.addEventListener("click", () => this.handleOpenClick());
    this.levelFilterSelect.addEventListener("change", () => this.handleLevelFilterChange());

    this.unsubTheme = this.ctx.theme.changes.on(() => this.applyThemeToAllPanes());
  }

  override activate(): void {
    this.unsubEvents = this.ctx.ws.events.on((event) => {
      if (event.event === "term.sources") {
        this.refreshSources();
      } else if (event.event === "term.state") {
        this.sessions = applySessionState(this.sessions, event.session, event.state);
        this.renderTabs();
        this.panes
          .get(event.session)
          ?.term.write(
            `\r\n\x1b[90m[session ${stateBadge(event.state)}${event.msg ? `: ${event.msg}` : ""}]\x1b[0m\r\n`,
          );
      }
    });
    this.unsubTermFrames = this.ctx.ws.termFrames.on((frame) => this.handleTermFrame(frame));
    // `activate()` can run before the socket has finished (re)connecting —
    // e.g. the very first tab switch, moments after page load. Refresh
    // again once connected, not just once here, so the picker doesn't stay
    // permanently empty from a request that failed only because it was too
    // early (mirrors the USB module's same reconnect-triggered refresh).
    this.unsubConnection = this.ctx.ws.connectionState.on((state) => {
      if (state === "connected") this.refreshSources();
    });
    this.resizeObserver = new ResizeObserver(() => this.fitActivePane());
    this.resizeObserver.observe(this.panesEl);
    this.refreshSources();
  }

  override deactivate(): void {
    this.unsubEvents?.();
    this.unsubEvents = null;
    this.unsubTermFrames?.();
    this.unsubTermFrames = null;
    this.unsubConnection?.();
    this.unsubConnection = null;
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
  }

  /** USB tree module's terminal icon: switch here, then open the given
   * source id immediately with default serial settings. */
  override onActivationDetail(detail: unknown): void {
    if (typeof detail !== "string") return;
    this.openSource(detail, DEFAULT_SERIAL_SETTINGS).catch(() => {
      /* surfaced via this.statusEl inside openSource */
    });
  }

  // --- sources -------------------------------------------------------

  private populateSettingsOptions(): void {
    this.baudSelect.replaceChildren();
    for (const baud of BAUD_PRESETS) {
      const opt = document.createElement("option");
      opt.value = String(baud);
      opt.textContent = String(baud);
      if (baud === DEFAULT_SERIAL_SETTINGS.baud) opt.selected = true;
      this.baudSelect.appendChild(opt);
    }
    const customOpt = document.createElement("option");
    customOpt.value = "custom";
    customOpt.textContent = "Custom…";
    this.baudSelect.appendChild(customOpt);

    this.dataBitsSelect.replaceChildren();
    for (const bits of DATA_BITS_OPTIONS) {
      const opt = document.createElement("option");
      opt.value = String(bits);
      opt.textContent = String(bits);
      if (bits === DEFAULT_SERIAL_SETTINGS.data_bits) opt.selected = true;
      this.dataBitsSelect.appendChild(opt);
    }

    this.paritySelect.replaceChildren();
    for (const parity of PARITY_OPTIONS) {
      const opt = document.createElement("option");
      opt.value = parity;
      opt.textContent = parity;
      if (parity === DEFAULT_SERIAL_SETTINGS.parity) opt.selected = true;
      this.paritySelect.appendChild(opt);
    }

    this.stopBitsSelect.replaceChildren();
    for (const stopBits of STOP_BITS_OPTIONS) {
      const opt = document.createElement("option");
      opt.value = stopBits;
      opt.textContent = stopBits;
      if (stopBits === DEFAULT_SERIAL_SETTINGS.stop_bits) opt.selected = true;
      this.stopBitsSelect.appendChild(opt);
    }
  }

  private refreshSources(): void {
    this.ctx.ws.send("term.sources", {}).then(
      (result) => {
        this.lastSources = result.sources;
        this.renderSourceOptions();
      },
      (err: unknown) => {
        this.statusEl.textContent = `Could not load terminal sources: ${toErrorInfo(err).msg}`;
      },
    );
  }

  private renderSourceOptions(): void {
    const previouslySelected = this.sourceSelect.value;
    this.sourceSelect.replaceChildren();
    for (const group of groupSources(this.lastSources)) {
      const optgroup = document.createElement("optgroup");
      optgroup.label = group.label;
      for (const source of group.sources) {
        const opt = document.createElement("option");
        opt.value = source.source;
        opt.textContent = source.label;
        opt.disabled = !source.available;
        if (!source.available && source.unavailable_reason) {
          opt.title = source.unavailable_reason;
        }
        optgroup.appendChild(opt);
      }
      this.sourceSelect.appendChild(optgroup);
    }
    if (previouslySelected && this.lastSources.some((s) => s.source === previouslySelected)) {
      this.sourceSelect.value = previouslySelected;
    }
    this.renderSelectionDetails();
  }

  private renderSelectionDetails(): void {
    const selected = findSource(this.lastSources, this.sourceSelect.value);
    this.reasonEl.textContent = selected && !selected.available ? (selected.unavailable_reason ?? "unavailable") : "";
    const kind = selected?.kind;
    this.settingsButton.hidden = kind !== "serial" && kind !== "decode";
    if (kind !== "serial" && kind !== "decode") this.popoverEl.hidden = true;
    this.serialSettingsEl.hidden = kind !== "serial";
    this.decodeSettingsEl.hidden = kind !== "decode";
    if (kind === "decode") void this.prepareDecodeForm();
  }

  // --- decode source (SPEC §6, §11.2, T21) ----------------------------

  /** Loads `decode.list`/`rec.list` once per popover open (cheap, and both change rarely enough within one session) then (re)builds the role/option rows for whatever recording+decoder are currently selected. */
  private async prepareDecodeForm(): Promise<void> {
    this.decodeFormErrorEl.textContent = "";
    try {
      if (this.decoders.length === 0) {
        const list = await this.ctx.ws.send("decode.list", {});
        this.decoders = list.decoders;
      }
      this.decodeRecordings = await this.ctx.ws.send("rec.list", {});
    } catch (err) {
      this.decodeFormErrorEl.textContent = toErrorInfo(err).msg;
      return;
    }
    this.renderDecodeRecOptions();
    this.renderDecodeDecoderOptions();
  }

  private renderDecodeRecOptions(): void {
    const previous = this.decodeRecSelect.value;
    this.decodeRecSelect.replaceChildren();
    for (const summary of this.decodeRecordings?.recordings ?? []) {
      const option = document.createElement("option");
      option.value = String(summary.rec);
      option.textContent = `#${summary.rec} — ${summary.device} (${summary.state})`;
      this.decodeRecSelect.appendChild(option);
    }
    const stillPresent = (this.decodeRecordings?.recordings ?? []).some((r) => String(r.rec) === previous);
    if (stillPresent) this.decodeRecSelect.value = previous;
  }

  private renderDecodeDecoderOptions(): void {
    const previous = this.decodeDecoderSelect.value;
    this.decodeDecoderSelect.replaceChildren();
    for (const decoder of this.decoders) {
      const option = document.createElement("option");
      option.value = decoder.id;
      option.textContent = decoder.name;
      this.decodeDecoderSelect.appendChild(option);
    }
    const stillPresent = this.decoders.some((d) => d.id === previous);
    this.decodeDecoderSelect.value = stillPresent ? previous : (this.decoders[0]?.id ?? "");
    this.onDecodeSelectionChanged();
  }

  private currentDecoderInfo(): DecoderInfo | null {
    return this.decoders.find((d) => d.id === this.decodeDecoderSelect.value) ?? null;
  }

  private decodeAvailableChannels(): readonly { readonly channel: number; readonly label: string }[] {
    const rec = Number(this.decodeRecSelect.value);
    const summary = this.decodeRecordings?.recordings.find((r) => r.rec === rec);
    return (summary?.channels ?? []).map((c) => ({ channel: c.channel, label: c.name }));
  }

  private onDecodeSelectionChanged(): void {
    const decoder = this.currentDecoderInfo();
    this.decodeRoleBindings = new Map();
    this.decodeOptionInputs = decoder ? initialOptionInputs(decoder) : new Map();
    this.renderDecodeRoles();
    this.renderDecodeOptions();
  }

  private renderDecodeRoles(): void {
    this.decodeRolesEl.replaceChildren();
    const decoder = this.currentDecoderInfo();
    if (!decoder) return;
    const channels = this.decodeAvailableChannels();
    for (const role of decoder.channels) {
      const row = document.createElement("label");
      row.className = POPOVER_ROW;
      const roleName = document.createElement("span");
      roleName.className = "min-w-[5rem] text-awic-fg-muted uppercase";
      roleName.textContent = role;
      const select = document.createElement("select");
      select.className = SELECT;
      select.dataset.role = role; // e2e hook: pick a role's channel select without a per-row id
      const none = document.createElement("option");
      none.value = "";
      none.textContent = "—";
      select.appendChild(none);
      for (const ch of channels) {
        const option = document.createElement("option");
        option.value = String(ch.channel);
        option.textContent = ch.label;
        select.appendChild(option);
      }
      const bound = this.decodeRoleBindings.get(role);
      select.value = bound !== undefined ? String(bound) : "";
      select.addEventListener("change", () => {
        const next = new Map(this.decodeRoleBindings);
        if (select.value === "") next.delete(role);
        else next.set(role, Number(select.value));
        this.decodeRoleBindings = next;
      });
      row.append(roleName, select);
      this.decodeRolesEl.appendChild(row);
    }
  }

  private renderDecodeOptions(): void {
    this.decodeOptionsEl.replaceChildren();
    const decoder = this.currentDecoderInfo();
    if (!decoder) return;
    for (const spec of decoder.options) {
      const row = document.createElement("label");
      row.className = POPOVER_ROW;
      row.title = spec.description;
      const name = document.createElement("span");
      name.className = "min-w-[5rem] text-awic-fg-muted";
      name.textContent = spec.id;
      row.appendChild(name);

      if (spec.type === "bool") {
        const input = document.createElement("input");
        input.type = "checkbox";
        input.checked = Boolean(this.decodeOptionInputs.get(spec.id) ?? spec.default);
        input.addEventListener("change", () => {
          const next = new Map(this.decodeOptionInputs);
          next.set(spec.id, input.checked);
          this.decodeOptionInputs = next;
        });
        row.appendChild(input);
      } else if (spec.type === "enum") {
        const select = document.createElement("select");
        select.className = SELECT;
        for (const value of spec.values) {
          const option = document.createElement("option");
          option.value = value;
          option.textContent = value;
          select.appendChild(option);
        }
        select.value = String(this.decodeOptionInputs.get(spec.id) ?? spec.default);
        select.addEventListener("change", () => {
          const next = new Map(this.decodeOptionInputs);
          next.set(spec.id, select.value);
          this.decodeOptionInputs = next;
        });
        row.appendChild(select);
      } else {
        const input = document.createElement("input");
        input.type = "number";
        input.className = `w-24 ${SELECT}`;
        if (spec.min !== null && spec.min !== undefined) input.min = String(spec.min);
        if (spec.max !== null && spec.max !== undefined) input.max = String(spec.max);
        input.value = String(this.decodeOptionInputs.get(spec.id) ?? spec.default);
        input.addEventListener("change", () => {
          const next = new Map(this.decodeOptionInputs);
          next.set(spec.id, input.value);
          this.decodeOptionInputs = next;
        });
        row.appendChild(input);
      }
      this.decodeOptionsEl.appendChild(row);
    }
  }

  /** Builds the `decode` source's `term.open` payload from the form, or shows the first validation error and returns `null` (SPEC T21: recording + decoder + channels + options). */
  private readDecodeOpenSpec(): DecodeOpenSpec | null {
    const decoder = this.currentDecoderInfo();
    const rec = Number(this.decodeRecSelect.value);
    if (!decoder || !Number.isFinite(rec)) {
      this.decodeFormErrorEl.textContent = "select a recording and a decoder";
      return null;
    }
    const built = buildDecodeConfig(decoder, this.decodeRoleBindings, this.decodeOptionInputs);
    if (built.status === "invalid") {
      this.decodeFormErrorEl.textContent = built.errors.map((e) => e.message).join("; ");
      return null;
    }
    this.decodeFormErrorEl.textContent = "";
    return { rec, decoder: decoder.id, channels: [...built.config.channels], options: [...built.config.options] };
  }

  private readCustomSerialSettings(): SerialSettings | null {
    const baudValue = this.baudSelect.value;
    let baud: number;
    if (baudValue === "custom") {
      baud = this.customBaudInput.valueAsNumber;
      if (!isValidCustomBaud(baud)) {
        this.statusEl.textContent = "Enter a valid custom baud rate.";
        return null;
      }
    } else {
      baud = Number(baudValue);
    }
    return {
      baud,
      data_bits: Number(this.dataBitsSelect.value),
      parity: this.paritySelect.value as SerialSettings["parity"],
      stop_bits: this.stopBitsSelect.value as SerialSettings["stop_bits"],
    };
  }

  private handleOpenClick(): void {
    const sourceId = this.sourceSelect.value;
    if (!sourceId) return;
    const selected = findSource(this.lastSources, sourceId);
    if (selected?.kind === "decode") {
      const decode = this.readDecodeOpenSpec();
      if (!decode) return; // validation message already shown in the popover
      this.openSource(sourceId, undefined, decode).catch(() => {
        /* surfaced via this.statusEl inside openSource */
      });
      return;
    }
    const settings = selected?.kind === "serial" ? this.readCustomSerialSettings() : undefined;
    if (selected?.kind === "serial" && settings === null) return; // validation message already shown
    this.openSource(sourceId, settings ?? undefined).catch(() => {
      /* surfaced via this.statusEl inside openSource */
    });
  }

  private async openSource(sourceId: string, serial?: SerialSettings, decode?: DecodeOpenSpec): Promise<void> {
    this.statusEl.textContent = "";
    try {
      const result = await this.ctx.ws.send("term.open", { source: sourceId, serial, decode });
      const session: SessionViewModel = {
        sessionId: result.session,
        source: result.source,
        kind: result.kind,
        label: result.label,
        state: result.state,
      };
      this.sessions = upsertSession(this.sessions, session);
      this.ensurePane(session);
      this.renderTabs();
      this.selectSession(session.sessionId);
    } catch (err) {
      this.statusEl.textContent = `Could not open ${sourceId}: ${toErrorInfo(err).msg}`;
      throw err;
    }
  }

  // --- sessions / panes ------------------------------------------------

  private ensurePane(session: SessionViewModel): void {
    if (this.panes.has(session.sessionId)) return;

    const container = document.createElement("div");
    container.className = "absolute inset-0 box-border p-2";
    container.hidden = true;
    this.panesEl.appendChild(container);

    const term = new Terminal({
      convertEol: true,
      theme: this.ctx.theme.resolved === "dark" ? DARK_THEME : LIGHT_THEME,
      scrollback: 5000,
      // Device-reported strings (a UART's own output, the server's log
      // lines) land here; xterm renders them itself rather than through
      // innerHTML either way, but screen-reader support is worth having on
      // its own merits for a text console like this one.
      screenReaderMode: true,
    });
    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    // `term.open()` is deferred to `selectSession` rather than called here:
    // xterm measures character/box metrics from the container at open time,
    // and this container is still `hidden` (zero size) until it becomes the
    // active pane — opening into a zero-size box leaves it permanently
    // mis-measured, `fit()` afterwards notwithstanding.

    if (session.kind === "serial") {
      term.onData((data) => {
        const current = this.sessions.get(session.sessionId);
        if (current && isWritable(current)) {
          this.ctx.ws.send("term.write", { session: session.sessionId, data }).catch(() => {});
        }
      });
      term.onBinary((data) => {
        const current = this.sessions.get(session.sessionId);
        if (current && isWritable(current)) {
          this.ctx.ws.send("term.write", { session: session.sessionId, data_b64: btoa(data) }).catch(() => {});
        }
      });
    }
    let scpiEditor: LineEditor | null = null;
    if (session.kind === "scpi") {
      // The SCPI engine only ever answers a complete line (SPEC §9) — unlike
      // a serial device, it never echoes what was typed, so the line
      // editing (local echo, backspace, cursor movement, history) has to
      // happen here rather than on the wire.
      const lineEditor = new LineEditor();
      scpiEditor = lineEditor;
      term.onData((data) => {
        const current = this.sessions.get(session.sessionId);
        if (!current || !isWritable(current)) return;
        const { echo, lines } = lineEditor.feed(data);
        if (echo) term.write(echo);
        for (const line of lines) {
          this.ctx.ws.send("term.write", { session: session.sessionId, data: `${line}\n` }).catch(() => {});
        }
      });
    }

    this.panes.set(session.sessionId, {
      container,
      term,
      fitAddon,
      opened: false,
      logLines: session.kind === "log" ? [] : null,
      logFilter: "all",
      lineEditor: scpiEditor,
    });
  }

  private selectSession(sessionId: number): void {
    const pane = this.panes.get(sessionId);
    if (!pane) return;
    for (const [id, p] of this.panes) {
      p.container.hidden = id !== sessionId;
    }
    this.activeSessionId = sessionId;
    this.emptyEl.hidden = true;
    this.renderTabs();
    this.levelFilterLabel.hidden = pane.logLines === null;
    this.levelFilterSelect.value = pane.logFilter;
    // First time this pane is shown: now — not at creation, while it was
    // still `hidden` (zero size) — is when xterm can correctly measure its
    // container (see `ensurePane`).
    if (!pane.opened) {
      pane.term.open(pane.container);
      pane.opened = true;
    }
    // A synchronous `fit()` right after unhiding can still land between two
    // layout states and settle on the wrong row/col count. One `fit()` now
    // covers most cases; a second after the next paint corrects the rest
    // without a visible flash.
    this.fitActivePane();
    requestAnimationFrame(() => this.fitActivePane());
  }

  private fitActivePane(): void {
    if (this.activeSessionId === null) return;
    const pane = this.panes.get(this.activeSessionId);
    try {
      pane?.fitAddon.fit();
    } catch {
      // A pane can be measured before its container has real layout (e.g.
      // immediately on tab activation); a missed fit is corrected by the
      // next resize or tab switch, never worth surfacing to the user.
    }
  }

  private closeSession(sessionId: number): void {
    this.ctx.ws.send("term.close", { session: sessionId }).catch(() => {});
    const pane = this.panes.get(sessionId);
    pane?.term.dispose();
    pane?.container.remove();
    this.panes.delete(sessionId);
    this.sessions = removeSession(this.sessions, sessionId);
    if (this.activeSessionId === sessionId) {
      this.activeSessionId = null;
      const next = [...this.sessions.keys()][0];
      if (next !== undefined) this.selectSession(next);
    }
    this.renderTabs();
    this.emptyEl.hidden = this.sessions.size > 0;
  }

  private renderTabs(): void {
    this.tabsEl.replaceChildren();
    for (const session of this.sessions.values()) {
      const tab = document.createElement("div");
      tab.className = `${TAB_BASE} ${session.sessionId === this.activeSessionId ? TAB_ACTIVE : TAB_INACTIVE}`;
      tab.addEventListener("click", () => this.selectSession(session.sessionId));

      const label = document.createElement("span");
      label.textContent = session.label;
      tab.appendChild(label);

      const badge = document.createElement("span");
      badge.className = "text-xs opacity-80";
      badge.textContent = stateBadge(session.state);
      tab.appendChild(badge);

      const close = document.createElement("button");
      close.type = "button";
      close.className = "text-base leading-none hover:opacity-70";
      close.textContent = "×";
      close.setAttribute("aria-label", `Close ${session.label}`);
      close.addEventListener("click", (ev) => {
        ev.stopPropagation();
        this.closeSession(session.sessionId);
      });
      tab.appendChild(close);

      this.tabsEl.appendChild(tab);
    }
  }

  private handleTermFrame(frame: DecodedTermFrame): void {
    const pane = this.panes.get(frame.header.sessionId);
    if (!pane) return;

    if (pane.logLines === null) {
      // Serial (and anything else non-log): raw bytes straight through, no
      // line structure to filter on. SCPI output goes around whatever the
      // user has half-typed.
      if (pane.lineEditor) {
        pane.term.write(pane.lineEditor.wrapOutput(new TextDecoder().decode(frame.payload)));
      } else {
        pane.term.write(frame.payload);
      }
      return;
    }

    // `log`: split into complete lines so the level filter can apply per
    // line. A `0xA7` chunk is, in practice, either the initial scrollback
    // replay (many lines at once) or one line from a single tracing event
    // — either way, splitting on the line ending it was written with
    // (log_source.rs uses `\r\n`) recovers exactly the lines.
    const text = new TextDecoder().decode(frame.payload);
    const lines = text.split("\r\n").filter((line) => line.length > 0);
    for (const line of lines) {
      pane.logLines.push(line);
    }
    const overflow = pane.logLines.length - LOG_BUFFER_LINES;
    if (overflow > 0) pane.logLines.splice(0, overflow);

    for (const line of lines) {
      if (lineMatchesFilter(line, pane.logFilter)) {
        pane.term.write(`${line}\r\n`);
      }
    }
  }

  /** Re-derives the active log pane's visible content from its full line
   * buffer under the newly selected filter — lines hidden by a stricter
   * filter are not lost, just not currently written to the terminal. */
  private handleLevelFilterChange(): void {
    if (this.activeSessionId === null) return;
    const pane = this.panes.get(this.activeSessionId);
    if (!pane || pane.logLines === null) return;
    pane.logFilter = this.levelFilterSelect.value as LogFilterLevel;
    pane.term.reset();
    const visible = filterLines(pane.logLines, pane.logFilter);
    if (visible.length > 0) {
      pane.term.write(`${visible.join("\r\n")}\r\n`);
    }
  }

  private applyThemeToAllPanes(): void {
    const theme = this.ctx.theme.resolved === "dark" ? DARK_THEME : LIGHT_THEME;
    for (const pane of this.panes.values()) {
      pane.term.options.theme = theme;
    }
  }
}

customElements.define(TAG, TerminalModule);

registerModule({
  id: "terminal",
  title: "Terminal",
  factory: () => document.createElement(TAG) as TerminalModule,
});
