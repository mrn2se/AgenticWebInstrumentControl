import { AwicModule } from "../../shell/module";
import { registerModule } from "../../shell/registry";
import type { McpLogEntry, McpLoggingState, McpSessionInfo, McpStatusResult } from "../../proto/types";
import { toErrorInfo } from "../../ws/errors";
import { connectionSnippets, mcpEndpointUrl, reachabilityNote, type Snippet } from "./connection";
import {
  McpLogModel,
  entryTitle,
  formatAgo,
  formatBytes,
  formatClock,
  formatDuration,
  isAtBottom,
  matchesFilter,
  plural,
  type LogFilter,
} from "./log-model";

const TAG = "awic-module-mcp";

const SECTION = "flex flex-col gap-2";
const HEADING = "text-xs font-semibold uppercase tracking-wide text-awic-fg-muted";
const BUTTON = "rounded-md border border-awic-border bg-awic-bg-raised px-2 py-0.5 text-xs font-medium hover:bg-awic-bg cursor-pointer";
const INPUT = "rounded border border-awic-border bg-awic-bg px-1.5 py-0.5 text-sm text-awic-fg";
const PRE = "m-0 max-h-72 overflow-auto whitespace-pre-wrap break-all rounded border border-awic-border bg-awic-bg p-2 font-mono text-xs";
const SNIPPET_TAB = "rounded-md border px-2 py-0.5 text-xs cursor-pointer";
const SNIPPET_TAB_ACTIVE = "border-awic-accent bg-awic-accent text-awic-accent-fg";
const SNIPPET_TAB_INACTIVE = "border-awic-border bg-awic-bg-raised";
const BADGE = "rounded px-1.5 py-px text-[11px] font-semibold";
const BADGE_OK = "bg-awic-ok-bg text-awic-ok";
const BADGE_ERR = "bg-awic-danger-bg text-awic-danger";
const BADGE_INFO = "bg-awic-bg text-awic-fg-muted";

/** Coalesces bursts of `mcp.activity` (an agent calling tools back to back). */
const FETCH_DEBOUNCE_MS = 200;
/** How many entries the first fetch asks for. */
const INITIAL_FETCH = 2000;

/**
 * Copies `text`, falling back to a hidden textarea where the async
 * clipboard API is missing — it is only offered on secure origins, and a
 * bench server on the LAN is usually plain `http://`.
 */
async function copyText(text: string): Promise<boolean> {
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // fall through to the textarea path
  }
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.appendChild(area);
  area.select();
  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }
  area.remove();
  return ok;
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, className = "", text?: string): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

/**
 * The MCP tab (SPECIFICATION.md §8.5): how to connect an agent, which
 * sessions are connected, and a live log of what they asked and were
 * answered — screenshots included. The server keeps the log (bounded, and
 * switchable from here because it costs memory); this module mirrors it
 * through `mcp.log`, pulling only while the tab is shown.
 */
class McpModule extends AwicModule {
  private readonly model = new McpLogModel();
  private readonly rows = new Map<number, HTMLElement>();
  /** Fetched pictures as `data:` URLs, keyed `seq:index`. */
  private readonly images = new Map<string, string>();

  private endpointEl!: HTMLElement;
  private serverEl!: HTMLElement;
  private authEl!: HTMLElement;
  private reachEl!: HTMLElement;
  private snippetTabsEl!: HTMLElement;
  private snippetWhereEl!: HTMLElement;
  private snippetEl!: HTMLElement;
  private sessionsEl!: HTMLUListElement;
  private sessionsCountEl!: HTMLElement;
  private logEnabledEl!: HTMLInputElement;
  private logImagesEl!: HTMLInputElement;
  private memoryBarEl!: HTMLElement;
  private memoryTextEl!: HTMLElement;
  private filterTextEl!: HTMLInputElement;
  private filterSessionEl!: HTMLSelectElement;
  private filterErrorsEl!: HTMLInputElement;
  private filterImagesEl!: HTMLInputElement;
  private followEl!: HTMLInputElement;
  private countEl!: HTMLElement;
  private logEl!: HTMLElement;
  private emptyEl!: HTMLElement;
  private errorEl!: HTMLElement;
  private viewerEl!: HTMLDialogElement;
  private viewerImgEl!: HTMLImageElement;

  private status: McpStatusResult | null = null;
  private snippets: Snippet[] = [];
  private activeSnippet = "vscode";

  private active = false;
  private dirtyLog = true;
  private dirtyStatus = true;
  private fetchTimer: ReturnType<typeof setTimeout> | null = null;
  private fetching = false;
  private agoTimer: ReturnType<typeof setInterval> | null = null;

  private unsubEvents: (() => void) | null = null;
  private unsubConnection: (() => void) | null = null;

  protected override onInit(): void {
    // The panels are laid out on an absolutely positioned layer, leaving
    // this element no intrinsic height: it takes exactly the space the shell
    // gives it, and the columns scroll inside it (SPEC §11.3: no page
    // scroll). On a narrow window the sidebar stacks above the log instead
    // of beside it, at most half the height, and can be folded away.
    this.className = "relative block min-h-0 flex-1 overflow-hidden";
    this.innerHTML = `
      <div class="absolute inset-0 flex flex-col md:flex-row">
      <aside id="mcp-sidebar" class="flex max-h-[50%] w-full flex-none flex-col gap-5 overflow-y-auto border-b border-awic-border bg-awic-bg-raised p-4 md:max-h-none md:w-[25rem] md:border-b-0 md:border-r">
        <section class="${SECTION}">
          <h3 class="${HEADING}">Connection</h3>
          <div class="flex items-center gap-2">
            <code id="mcp-endpoint" class="min-w-0 flex-1 truncate rounded border border-awic-border bg-awic-bg px-2 py-1 font-mono text-sm"></code>
            <button id="copy-endpoint" type="button" class="${BUTTON}">Copy</button>
          </div>
          <p id="mcp-server" class="m-0 text-xs text-awic-fg-muted"></p>
          <p id="mcp-auth" class="m-0 text-xs"></p>
          <p id="mcp-reach" hidden class="m-0 rounded bg-awic-warn-bg px-2 py-1 text-xs text-awic-warn"></p>
          <div id="snippet-tabs" class="mt-1 flex flex-wrap gap-1"></div>
          <p id="snippet-where" class="m-0 text-xs text-awic-fg-muted"></p>
          <div class="relative">
            <pre id="snippet" class="${PRE}"></pre>
            <button id="copy-snippet" type="button" class="${BUTTON} absolute right-1.5 top-1.5">Copy</button>
          </div>
        </section>
        <section class="${SECTION}">
          <h3 class="${HEADING}">Sessions <span id="sessions-count" class="font-normal normal-case"></span></h3>
          <ul id="sessions" class="m-0 flex list-none flex-col gap-1.5 p-0"></ul>
        </section>
        <section class="${SECTION}">
          <h3 class="${HEADING}">Logging</h3>
          <label class="flex items-center gap-2 text-sm"><input id="log-enabled" type="checkbox" /> Log messages</label>
          <label class="flex items-center gap-2 text-sm"><input id="log-images" type="checkbox" /> Keep screenshots</label>
          <div class="h-1.5 overflow-hidden rounded bg-awic-bg"><div id="memory-bar" class="h-full bg-awic-accent" style="width:0%"></div></div>
          <div class="flex items-center justify-between gap-2">
            <span id="memory-text" class="text-xs text-awic-fg-muted"></span>
            <button id="clear-log" type="button" class="${BUTTON}">Clear</button>
          </div>
          <p class="m-0 text-xs text-awic-fg-muted">Sessions and call counts are tracked either way; only the message log uses memory, and it never grows past its limit (<code>[mcp].log_memory_mb</code>) — the oldest entries go first.</p>
        </section>
      </aside>
      <div class="flex min-h-0 min-w-0 flex-1 flex-col">
        <div class="flex flex-none flex-wrap items-center gap-3 border-b border-awic-border bg-awic-bg-raised px-4 py-2">
          <button id="sidebar-toggle" type="button" class="${BUTTON} md:hidden" aria-controls="mcp-sidebar" aria-expanded="true">Hide setup</button>
          <input id="filter-text" type="search" placeholder="Filter tool, client, arguments…" class="${INPUT} w-full min-w-0 sm:w-64" />
          <select id="filter-session" class="${INPUT}"><option value="">All sessions</option></select>
          <label class="flex items-center gap-1.5 text-sm"><input id="filter-errors" type="checkbox" /> Errors</label>
          <label class="flex items-center gap-1.5 text-sm"><input id="filter-images" type="checkbox" /> Screenshots</label>
          <label class="flex items-center gap-1.5 text-sm"><input id="follow" type="checkbox" checked /> Follow</label>
          <span id="log-count" class="ml-auto text-xs text-awic-fg-muted"></span>
        </div>
        <p id="mcp-error" hidden class="m-0 bg-awic-danger-bg px-4 py-2 text-sm text-awic-danger"></p>
        <div id="mcp-log" class="min-h-0 flex-1 overflow-y-auto">
          <p id="log-empty" class="m-0 p-6 text-sm text-awic-fg-muted"></p>
        </div>
      </div>
      </div>
      <dialog id="viewer" class="m-auto max-h-[90vh] max-w-[90vw] rounded-lg border border-awic-border bg-awic-bg-raised p-2 text-awic-fg backdrop:bg-black/50">
        <div class="mb-2 flex justify-end"><button id="viewer-close" type="button" class="${BUTTON}">Close</button></div>
        <img id="viewer-img" alt="MCP screenshot" class="block max-h-[80vh] max-w-full bg-white" />
      </dialog>
    `;

    this.endpointEl = this.requireEl("#mcp-endpoint");
    this.serverEl = this.requireEl("#mcp-server");
    this.authEl = this.requireEl("#mcp-auth");
    this.reachEl = this.requireEl("#mcp-reach");
    this.snippetTabsEl = this.requireEl("#snippet-tabs");
    this.snippetWhereEl = this.requireEl("#snippet-where");
    this.snippetEl = this.requireEl("#snippet");
    this.sessionsEl = this.requireEl<HTMLUListElement>("#sessions");
    this.sessionsCountEl = this.requireEl("#sessions-count");
    this.logEnabledEl = this.requireEl<HTMLInputElement>("#log-enabled");
    this.logImagesEl = this.requireEl<HTMLInputElement>("#log-images");
    this.memoryBarEl = this.requireEl("#memory-bar");
    this.memoryTextEl = this.requireEl("#memory-text");
    this.filterTextEl = this.requireEl<HTMLInputElement>("#filter-text");
    this.filterSessionEl = this.requireEl<HTMLSelectElement>("#filter-session");
    this.filterErrorsEl = this.requireEl<HTMLInputElement>("#filter-errors");
    this.filterImagesEl = this.requireEl<HTMLInputElement>("#filter-images");
    this.followEl = this.requireEl<HTMLInputElement>("#follow");
    this.countEl = this.requireEl("#log-count");
    this.logEl = this.requireEl("#mcp-log");
    this.emptyEl = this.requireEl("#log-empty");
    this.errorEl = this.requireEl("#mcp-error");
    this.viewerEl = this.requireEl<HTMLDialogElement>("#viewer");
    this.viewerImgEl = this.requireEl<HTMLImageElement>("#viewer-img");

    this.requireEl("#copy-endpoint").addEventListener("click", (e) =>
      this.copyWithFeedback(e.currentTarget as HTMLButtonElement, this.endpointEl.textContent ?? ""),
    );
    this.requireEl("#copy-snippet").addEventListener("click", (e) =>
      this.copyWithFeedback(e.currentTarget as HTMLButtonElement, this.snippetEl.textContent ?? ""),
    );
    this.logEnabledEl.addEventListener("change", () => void this.setLogging({ enabled: this.logEnabledEl.checked }));
    this.logImagesEl.addEventListener("change", () => void this.setLogging({ images: this.logImagesEl.checked }));
    this.requireEl("#clear-log").addEventListener("click", () => void this.setLogging({ clear: true }));
    for (const input of [this.filterTextEl, this.filterSessionEl, this.filterErrorsEl, this.filterImagesEl]) {
      input.addEventListener("input", () => this.applyFilter());
      input.addEventListener("change", () => this.applyFilter());
    }
    this.followEl.addEventListener("change", () => {
      if (this.followEl.checked) this.scrollToEnd();
    });
    // Follow tracks where the log is scrolled: up to read unticks it, so the
    // next message does not yank the view back down; the bottom ticks it
    // again. A scroll this module makes itself lands at the bottom, so it
    // never unticks anything.
    this.logEl.addEventListener("scroll", () => {
      this.followEl.checked = isAtBottom(this.logEl.scrollTop, this.logEl.scrollHeight, this.logEl.clientHeight);
    });
    this.requireEl("#viewer-close").addEventListener("click", () => this.viewerEl.close());
    const sidebar = this.requireEl("#mcp-sidebar");
    const sidebarToggle = this.requireEl<HTMLButtonElement>("#sidebar-toggle");
    sidebarToggle.addEventListener("click", () => {
      // `max-md:hidden` only folds it on a narrow window; a wide one always
      // shows it beside the log, whatever this was left at.
      const folded = sidebar.classList.toggle("max-md:hidden");
      sidebarToggle.textContent = folded ? "Show setup" : "Hide setup";
      sidebarToggle.setAttribute("aria-expanded", String(!folded));
    });
    this.viewerEl.addEventListener("click", (e) => {
      if (e.target === this.viewerEl) this.viewerEl.close();
    });

    this.renderConnection();
    this.renderEmpty();

    // Listen from the start, so the tab knows it is behind when it is shown;
    // fetching itself waits until it is (nothing is pulled for a hidden tab).
    this.unsubEvents = this.ctx.ws.events.on((event) => {
      if (event.event !== "mcp.activity") return;
      if (event.seq > this.model.lastSeq) this.dirtyLog = true;
      if (event.sessions_changed) this.dirtyStatus = true;
      this.scheduleFetch();
    });
    this.unsubConnection = this.ctx.ws.connectionState.on((state) => {
      if (state === "connected") {
        this.dirtyLog = true;
        this.dirtyStatus = true;
        this.scheduleFetch();
      }
    });
  }

  override activate(): void {
    this.active = true;
    this.dirtyStatus = true;
    this.dirtyLog = true;
    this.scheduleFetch(0);
    this.agoTimer = setInterval(() => this.renderSessions(), 5000);
    // Messages that arrived while the tab was hidden were appended without
    // a scroll (a hidden element has no layout to scroll).
    if (this.followEl.checked) requestAnimationFrame(() => this.scrollToEnd());
  }

  override deactivate(): void {
    this.active = false;
    if (this.agoTimer !== null) clearInterval(this.agoTimer);
    this.agoTimer = null;
  }

  disconnectedCallback(): void {
    this.unsubEvents?.();
    this.unsubConnection?.();
    if (this.fetchTimer !== null) clearTimeout(this.fetchTimer);
    if (this.agoTimer !== null) clearInterval(this.agoTimer);
  }

  // ---- fetching ---------------------------------------------------------

  private scheduleFetch(delay = FETCH_DEBOUNCE_MS): void {
    if (!this.active || this.fetchTimer !== null) return;
    this.fetchTimer = setTimeout(() => {
      this.fetchTimer = null;
      void this.fetchNow();
    }, delay);
  }

  private async fetchNow(): Promise<void> {
    if (this.fetching) return;
    this.fetching = true;
    try {
      if (this.dirtyStatus) {
        this.dirtyStatus = false;
        this.status = await this.ctx.ws.send("mcp.status", {});
        this.renderConnection();
        this.renderSessions();
        this.renderLogging(this.status.logging);
      }
      if (this.dirtyLog) {
        this.dirtyLog = false;
        const first = this.model.lastSeq === 0;
        const result = await this.ctx.ws.send("mcp.log", {
          after: this.model.lastSeq,
          limit: first ? INITIAL_FETCH : null,
        });
        if (this.model.merge(result.entries, result.last_seq, result.logging.entries)) this.renderLog();
        this.renderLogging(result.logging);
      }
      this.showError(null);
    } catch (err) {
      this.showError(`Could not read the MCP monitor: ${toErrorInfo(err).msg}`);
    } finally {
      this.fetching = false;
      if (this.dirtyLog || this.dirtyStatus) this.scheduleFetch();
    }
  }

  private async setLogging(change: { enabled?: boolean; images?: boolean; clear?: boolean }): Promise<void> {
    try {
      const state = await this.ctx.ws.send("mcp.logging", {
        enabled: change.enabled ?? null,
        images: change.images ?? null,
        clear: change.clear ?? null,
      });
      if (change.clear) {
        this.model.merge([], this.model.lastSeq, 0);
        this.renderLog();
      }
      this.renderLogging(state);
    } catch (err) {
      this.showError(`Could not change MCP logging: ${toErrorInfo(err).msg}`);
      if (this.status) this.renderLogging(this.status.logging);
    }
  }

  private async loadImage(seq: number, index: number): Promise<string> {
    const key = `${seq}:${index}`;
    const cached = this.images.get(key);
    if (cached) return cached;
    const image = await this.ctx.ws.send("mcp.image", { seq, index });
    const url = `data:${image.mime_type};base64,${image.data_b64}`;
    this.images.set(key, url);
    return url;
  }

  // ---- rendering --------------------------------------------------------

  private showError(msg: string | null): void {
    this.errorEl.hidden = msg === null;
    this.errorEl.textContent = msg ?? "";
  }

  private renderConnection(): void {
    const url = mcpEndpointUrl(window.location, this.status?.endpoint_path ?? "/mcp");
    const auth = this.status?.auth_required ?? false;
    this.endpointEl.textContent = url;
    if (this.status) {
      const s = this.status;
      this.serverEl.textContent = `${s.server_name} ${s.server_version} · MCP ${s.protocol_version} · streamable HTTP · ${s.tool_count} tools`;
      this.authEl.textContent = auth
        ? "A bearer token is required ([auth].token). The snippets prompt for it; it is never shown here."
        : "No token required.";
      this.authEl.className = `m-0 text-xs ${auth ? "text-awic-warn" : "text-awic-fg-muted"}`;
      const note = reachabilityNote(window.location, s.bind);
      this.reachEl.hidden = note === null;
      this.reachEl.textContent = note ?? "";
    }
    this.snippets = connectionSnippets(url, auth);
    this.snippetTabsEl.replaceChildren(
      ...this.snippets.map((snippet) => {
        const button = el(
          "button",
          `${SNIPPET_TAB} ${snippet.id === this.activeSnippet ? SNIPPET_TAB_ACTIVE : SNIPPET_TAB_INACTIVE}`,
          snippet.label,
        );
        button.type = "button";
        button.addEventListener("click", () => {
          this.activeSnippet = snippet.id;
          this.renderConnection();
        });
        return button;
      }),
    );
    const current = this.snippets.find((s) => s.id === this.activeSnippet) ?? this.snippets[0];
    if (!current) return;
    this.snippetWhereEl.textContent = `Paste into ${current.where}:`;
    this.snippetEl.textContent = current.text;
  }

  private renderSessions(): void {
    const sessions = this.status?.sessions ?? [];
    const live = sessions.filter((s) => s.active).length;
    this.sessionsCountEl.textContent = this.status ? `· ${live} active of ${this.status.max_sessions}` : "";
    if (sessions.length === 0) {
      this.sessionsEl.replaceChildren(el("li", "text-sm text-awic-fg-muted", "No MCP client has connected since the server started."));
    } else {
      const now = Date.now();
      this.sessionsEl.replaceChildren(...sessions.map((s) => this.sessionRow(s, now)));
    }

    // Keep the session filter's options in step with what is known.
    const selected = this.filterSessionEl.value;
    const all = el("option", "", "All sessions");
    all.value = "";
    const options = [all];
    for (const s of sessions) {
      const option = el("option", "", `${s.id} · ${s.client}`);
      option.value = s.id;
      options.push(option);
    }
    this.filterSessionEl.replaceChildren(...options);
    this.filterSessionEl.value = sessions.some((s) => s.id === selected) ? selected : "";
  }

  private sessionRow(s: McpSessionInfo, now: number): HTMLLIElement {
    const li = el("li", `cursor-pointer rounded-md border border-awic-border p-2 text-sm hover:bg-awic-bg ${s.active ? "" : "opacity-60"}`);
    li.title = "Show only this session's messages";
    const top = el("div", "flex items-center gap-2");
    const dot = el("span", `h-2 w-2 flex-none rounded-full ${s.active ? "bg-awic-ok" : "bg-awic-fg-muted"}`);
    top.append(
      dot,
      el("span", "min-w-0 flex-1 truncate font-medium", s.client_version ? `${s.client} ${s.client_version}` : s.client),
      el("code", "text-xs text-awic-fg-muted", s.id),
    );
    const stats = el(
      "div",
      "mt-0.5 text-xs text-awic-fg-muted",
      `${plural(s.tool_calls, "tool call")} · ${plural(s.requests, "request")}` + (s.errors > 0 ? ` · ${plural(s.errors, "error")}` : ""),
    );
    const last = el(
      "div",
      "text-xs text-awic-fg-muted",
      s.active
        ? `last seen ${formatAgo(s.last_seen_ms, now)}${s.last_tool ? ` · last tool ${s.last_tool}` : ""} · MCP ${s.protocol_version}`
        : `ended: ${s.ended ?? "closed"} · started ${formatAgo(s.started_ms, now)}`,
    );
    if (s.errors > 0) stats.classList.add("text-awic-danger");
    li.append(top, stats, last);
    li.addEventListener("click", () => {
      this.filterSessionEl.value = s.id;
      this.applyFilter();
    });
    return li;
  }

  private renderLogging(state: McpLoggingState): void {
    this.logEnabledEl.checked = state.enabled;
    this.logImagesEl.checked = state.images;
    this.logImagesEl.disabled = !state.enabled;
    const pct = state.memory_limit_bytes > 0 ? Math.min(100, (100 * state.memory_used_bytes) / state.memory_limit_bytes) : 0;
    this.memoryBarEl.style.width = `${pct.toFixed(1)}%`;
    const dropped = state.dropped > 0 ? ` · ${state.dropped} oldest dropped` : "";
    this.memoryTextEl.textContent = `${formatBytes(state.memory_used_bytes)} of ${formatBytes(state.memory_limit_bytes)} · ${plural(state.entries, "entry")}${dropped}`;
    if (this.status) this.status = { ...this.status, logging: state };
    this.renderEmpty();
  }

  private currentFilter(): LogFilter {
    return {
      text: this.filterTextEl.value,
      errorsOnly: this.filterErrorsEl.checked,
      session: this.filterSessionEl.value,
      imagesOnly: this.filterImagesEl.checked,
    };
  }

  private renderEmpty(): void {
    const enabled = this.status?.logging.enabled ?? true;
    const any = this.model.entries.length > 0;
    this.emptyEl.hidden = any;
    this.emptyEl.textContent = enabled
      ? "No MCP messages yet. Connect a client with the settings on the left; its requests appear here as they happen."
      : "Message logging is off. Tick “Log messages” to record what MCP clients ask and are answered.";
  }

  /** Brings the DOM in step with the model: removes dropped rows, appends new ones. */
  private renderLog(): void {
    const wanted = new Set(this.model.entries.map((e) => e.seq));
    for (const [seq, row] of this.rows) {
      if (!wanted.has(seq)) {
        row.remove();
        this.rows.delete(seq);
        for (const key of [...this.images.keys()]) if (key.startsWith(`${seq}:`)) this.images.delete(key);
      }
    }
    const filter = this.currentFilter();
    for (const entry of this.model.entries) {
      if (this.rows.has(entry.seq)) continue;
      const row = this.entryRow(entry);
      row.hidden = !matchesFilter(entry, filter);
      this.rows.set(entry.seq, row);
      this.logEl.appendChild(row);
    }
    this.renderEmpty();
    this.renderCount();
    if (this.followEl.checked) this.scrollToEnd();
  }

  private applyFilter(): void {
    const filter = this.currentFilter();
    for (const entry of this.model.entries) {
      const row = this.rows.get(entry.seq);
      if (row) row.hidden = !matchesFilter(entry, filter);
    }
    this.renderCount();
    if (this.followEl.checked) this.scrollToEnd();
  }

  private renderCount(): void {
    const total = this.model.entries.length;
    let shown = 0;
    for (const row of this.rows.values()) if (!row.hidden) shown++;
    this.countEl.textContent = shown === total ? plural(total, "message") : `${shown} of ${plural(total, "message")}`;
  }

  private scrollToEnd(): void {
    this.logEl.scrollTop = this.logEl.scrollHeight;
  }

  private entryRow(entry: McpLogEntry): HTMLElement {
    const row = el("div", "border-b border-awic-border");
    row.dataset.seq = String(entry.seq);
    const details = el("details", "group");
    const summary = el("summary", "flex cursor-pointer list-none items-center gap-3 px-4 py-1.5 text-sm hover:bg-awic-bg-raised");
    const lifecycle = entry.kind === "session_start" || entry.kind === "session_end";
    const status = lifecycle
      ? el("span", `${BADGE} ${BADGE_INFO}`, "session")
      : entry.is_error
        ? el("span", `${BADGE} ${BADGE_ERR}`, "error")
        : entry.kind === "notification"
          ? el("span", `${BADGE} ${BADGE_INFO}`, "notify")
          : el("span", `${BADGE} ${BADGE_OK}`, "ok");
    const badgeCell = el("span", "w-14 flex-none text-center");
    badgeCell.append(status);
    summary.append(
      el("span", "w-24 flex-none font-mono text-xs text-awic-fg-muted", formatClock(entry.t_ms)),
      el("code", "hidden w-[4.5rem] flex-none text-xs text-awic-fg-muted sm:inline", entry.session ?? "—"),
      el("span", "w-28 flex-none truncate font-medium sm:w-44", entryTitle(entry)),
      badgeCell,
      el("span", "hidden w-16 flex-none text-right text-xs text-awic-fg-muted sm:inline", formatDuration(entry.duration_ms)),
      el("span", `min-w-0 flex-1 truncate font-mono text-xs ${entry.is_error ? "text-awic-danger" : "text-awic-fg-muted"}`, entry.summary),
    );
    if (entry.images.length > 0) {
      summary.append(el("span", "flex-none text-xs text-awic-accent", `🖼 ${entry.images.length}`));
    }
    summary.title = `${entry.client} · ${entry.method}`;
    details.append(summary);
    row.append(details);

    // Screenshots are shown right under their row, open or not (so outside
    // the <details>, whose body is hidden while closed): they are what a
    // person looks at the log for.
    if (entry.images.length > 0) row.append(this.thumbnails(entry));

    let filled = false;
    details.addEventListener("toggle", () => {
      if (!details.open || filled) return;
      filled = true;
      const body = el("div", "flex flex-col gap-2 px-4 pb-3 pt-1");
      body.append(el("div", "text-xs text-awic-fg-muted", `${entry.client} · ${entry.method} · seq ${entry.seq}`));
      if (entry.request) body.append(el("div", HEADING, "Request"), el("pre", PRE, entry.request));
      if (entry.response) body.append(el("div", HEADING, "Response"), el("pre", PRE, entry.response));
      details.append(body);
    });
    return row;
  }

  private thumbnails(entry: McpLogEntry): HTMLElement {
    const strip = el("div", "flex flex-wrap gap-2 px-4 pb-2 sm:pl-[13.5rem]");
    entry.images.forEach((info, index) => {
      if (!info.kept) {
        strip.append(
          el("span", "rounded border border-dashed border-awic-border px-2 py-1 text-xs text-awic-fg-muted", `${info.mime_type}, ${formatBytes(info.bytes)} — not kept`),
        );
        return;
      }
      const img = el("img", "h-28 cursor-zoom-in rounded border border-awic-border bg-white object-contain");
      img.alt = `Screenshot ${index + 1} of ${entryTitle(entry)}`;
      img.title = `${info.mime_type}, ${formatBytes(info.bytes)} — click to enlarge`;
      img.loading = "lazy";
      // A picture is taller once it has loaded, which is after the log was
      // scrolled to its end: scroll again, or the log stops short of it.
      img.addEventListener("load", () => {
        if (this.followEl.checked) this.scrollToEnd();
      });
      void this.loadImage(entry.seq, index)
        .then((url) => {
          img.src = url;
        })
        .catch((err: unknown) => {
          img.replaceWith(el("span", "text-xs text-awic-danger", toErrorInfo(err).msg));
        });
      img.addEventListener("click", () => {
        this.viewerImgEl.src = img.src;
        this.viewerImgEl.alt = img.alt;
        this.viewerEl.showModal();
      });
      strip.append(img);
    });
    return strip;
  }

  private copyWithFeedback(button: HTMLButtonElement, text: string): void {
    void copyText(text).then((ok) => {
      const original = "Copy";
      button.textContent = ok ? "Copied" : "Copy failed";
      setTimeout(() => {
        button.textContent = original;
      }, 1500);
    });
  }
}

customElements.define(TAG, McpModule);

registerModule({
  id: "mcp",
  title: "MCP",
  factory: () => document.createElement(TAG) as McpModule,
});
