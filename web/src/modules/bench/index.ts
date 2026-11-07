import { AwicModule } from "../../shell/module";
import { registerModule } from "../../shell/registry";
import { requestModuleActivation } from "../../shell/activation";
import type { SimStatus } from "../../proto/types";
import type { RequestParams } from "../../ws/client";
import { toErrorInfo } from "../../ws/errors";
import {
  channelName,
  formatHz,
  formatOhm,
  formatUptime,
  instrumentShortName,
  r2ToSlider,
  sliderToR2,
  testPointRows,
} from "./bench-model";
import { createSevenSeg, updateSevenSeg } from "./seven-seg";

const TAG = "awic-module-bench";

/** While the tab is shown: uptime ticks and the display follows the keys. */
const POLL_MS = 400;

const CARD = "rounded-lg border border-awic-border bg-awic-bg-raised p-4";
const H3 = "mb-2 text-base font-semibold";
const MUTED = "text-sm text-awic-fg-muted";
const BTN =
  "rounded-md border border-awic-border bg-awic-bg px-3 py-1.5 text-sm font-medium hover:bg-awic-bg-raised disabled:cursor-not-allowed disabled:opacity-55";
const BTN_ACCENT =
  "rounded-md bg-awic-accent px-3 py-1.5 text-sm font-semibold text-awic-accent-fg hover:opacity-90 disabled:cursor-not-allowed disabled:opacity-55";
const BADGE_ON = "rounded-full bg-awic-ok-bg px-2 py-0.5 text-xs font-semibold text-awic-ok";
const BADGE_OFF = "rounded-full bg-awic-bg px-2 py-0.5 text-xs font-semibold text-awic-fg-muted border border-awic-border";

/**
 * The virtual bench (`awic --sim`): the trainer board with its test points
 * and what is clipped to them, the U3 puzzle with its keys and display, and
 * the faults to hunt. Everything measurable is measured in the other tabs —
 * this page only shows what a person standing at the bench would see, and
 * what the bench is keeping secret until it is revealed.
 */
class BenchModule extends AwicModule {
  private status: SimStatus | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;
  private unsubscribeEvents: (() => void) | null = null;
  private busy = false;
  private seven!: SVGSVGElement;
  private errorEl!: HTMLElement;

  protected override onInit(): void {
    this.className = "block min-h-0 flex-1 overflow-auto p-4 md:p-6";
    this.innerHTML = `
      <div class="mb-4 flex flex-wrap items-baseline justify-between gap-2">
        <h2 class="text-xl font-semibold">Virtual bench</h2>
        <span id="bench-meta" class="${MUTED}"></span>
      </div>
      <p id="bench-error" hidden class="mb-3 rounded-md bg-awic-danger-bg px-3 py-2 text-sm text-awic-danger"></p>
      <div class="grid gap-4 lg:grid-cols-2">
        <section class="${CARD}">
          <div class="mb-2 flex flex-wrap items-center justify-between gap-2">
            <h3 class="text-base font-semibold" id="board-name"></h3>
            <span id="board-power"></span>
          </div>
          <p id="board-desc" class="${MUTED} mb-3"></p>
          <dl id="board-facts" class="mb-3 grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm"></dl>
          <div class="flex flex-wrap gap-2">
            <button id="board-console" type="button" class="${BTN_ACCENT}">Open console</button>
            <button id="board-cycle" type="button" class="${BTN}">Power-cycle</button>
            <button id="board-toggle" type="button" class="${BTN}"></button>
          </div>
        </section>

        <section class="${CARD}">
          <h3 class="${H3}">U3 — what is this chip?</h3>
          <p id="u3-desc" class="${MUTED} mb-3"></p>
          <div class="flex flex-wrap items-center gap-6">
            <div class="flex flex-col items-center gap-1">
              <div id="u3-display" class="rounded-md bg-black/80 p-2"></div>
              <span class="text-xs text-awic-fg-muted">DS1</span>
            </div>
            <div id="u3-inputs" class="grid grid-cols-2 gap-2"></div>
          </div>
          <form id="u3-guess" class="mt-4 flex flex-wrap items-center gap-2">
            <label for="u3-part" class="text-sm">Part number</label>
            <input id="u3-part" type="text" autocomplete="off" placeholder="e.g. 74HC4511"
              class="w-40 rounded-md border border-awic-border bg-awic-bg px-2 py-1 text-sm" />
            <button type="submit" class="${BTN}">Check</button>
            <span id="u3-verdict" class="text-sm"></span>
          </form>
        </section>

        <section class="${CARD} lg:col-span-2">
          <h3 class="${H3}">Test points</h3>
          <p id="tp-note" class="${MUTED} mb-2"></p>
          <div class="overflow-x-auto">
            <table class="w-full text-left text-sm">
              <thead class="text-xs text-awic-fg-muted">
                <tr><th class="py-1 pr-3">Point</th><th class="py-1 pr-3">Name</th><th class="py-1 pr-3">What it is</th><th class="py-1">Probed by</th></tr>
              </thead>
              <tbody id="tp-rows"></tbody>
            </table>
          </div>
          <div id="instruments" class="mt-3 flex flex-wrap gap-2 text-sm"></div>
        </section>

        <section class="${CARD}">
          <h3 class="${H3}">555 timer</h3>
          <p class="${MUTED} mb-2">R2 is a pot on the board. Turn it and measure the change on the scope (CH1 the capacitor, CH2 the output).</p>
          <div class="flex flex-wrap items-center gap-3">
            <input id="pot" type="range" min="0" max="1000" step="1" class="w-56" aria-label="R2" />
            <span id="pot-value" class="text-sm tabular-nums"></span>
          </div>
        </section>

        <section class="${CARD}">
          <h3 class="${H3}">Faults</h3>
          <p id="fault-note" class="${MUTED} mb-2"></p>
          <ul id="faults" class="m-0 mb-3 flex list-none flex-col gap-2 p-0"></ul>
          <div class="flex flex-wrap gap-2">
            <button id="fault-mystery" type="button" class="${BTN_ACCENT}">Start a mystery</button>
            <button id="fault-clear" type="button" class="${BTN}">Clear faults</button>
            <button id="fault-reveal" type="button" class="${BTN}">Reveal everything</button>
          </div>
        </section>
      </div>
    `;
    this.errorEl = this.requireEl("#bench-error");
    this.seven = createSevenSeg();
    this.requireEl("#u3-display").appendChild(this.seven);

    this.requireEl("#board-console").addEventListener("click", () => {
      if (this.status) requestModuleActivation("terminal", this.status.board.serial_source);
    });
    this.requireEl("#board-cycle").addEventListener("click", () => {
      const s = this.status;
      if (!s) return;
      this.run(this.ctx.ws.send("usb.cycle", { hub: s.hub, port: s.board.port, off_ms: 800 }, 15_000));
    });
    this.requireEl("#board-toggle").addEventListener("click", () => {
      const s = this.status;
      if (!s) return;
      this.run(this.ctx.ws.send("usb.power", { hub: s.hub, port: s.board.port, on: !s.board.powered }));
    });
    const pot = this.requireEl<HTMLInputElement>("#pot");
    pot.addEventListener("input", () => {
      this.requireEl("#pot-value").textContent = `R2 ≈ ${formatOhm(sliderToR2(Number(pot.value)))}`;
    });
    pot.addEventListener("change", () => this.set({ osc_r2_ohm: sliderToR2(Number(pot.value)) }));
    this.requireEl("#fault-mystery").addEventListener("click", () => this.set({ mystery: true }));
    this.requireEl("#fault-clear").addEventListener("click", () => this.set({ faults: [] }));
    this.requireEl("#fault-reveal").addEventListener("click", () => {
      if (window.confirm("Reveal the wiring, the faults in force and U3's part number? This ends the puzzles.")) {
        this.set({ reveal: true });
      }
    });
    this.requireEl<HTMLFormElement>("#u3-guess").addEventListener("submit", (e) => {
      e.preventDefault();
      const input = this.requireEl<HTMLInputElement>("#u3-part");
      const guess = input.value.trim();
      if (!guess || !this.status) return;
      const before = this.status.counter.wrong_guesses;
      this.ctx.ws.send("sim.set", { guess }).then(
        (status) => {
          const right = status.counter.wrong_guesses === before;
          const verdict = this.requireEl("#u3-verdict");
          verdict.textContent = right ? `Yes — it is a ${status.counter.part ?? guess}.` : `No, not a ${guess}.`;
          verdict.className = `text-sm ${right ? "text-awic-ok" : "text-awic-danger"}`;
          this.apply(status);
        },
        (err: unknown) => this.showError(toErrorInfo(err).msg),
      );
    });
  }

  override activate(): void {
    this.unsubscribeEvents = this.ctx.ws.events.on((event) => {
      if (event.event === "sim.changed" || event.event === "usb.changed") this.refresh();
    });
    this.timer = setInterval(() => this.refresh(), POLL_MS);
    this.refresh();
  }

  override deactivate(): void {
    this.unsubscribeEvents?.();
    this.unsubscribeEvents = null;
    if (this.timer) clearInterval(this.timer);
    this.timer = null;
  }

  private refresh(): void {
    if (this.busy) return;
    this.busy = true;
    this.ctx.ws
      .send("sim.status", {})
      .then(
        (status) => this.apply(status),
        (err: unknown) => this.showError(toErrorInfo(err).msg),
      )
      .finally(() => {
        this.busy = false;
      });
  }

  private set(params: RequestParams<"sim.set">): void {
    this.ctx.ws.send("sim.set", params).then(
      (status) => this.apply(status),
      (err: unknown) => this.showError(toErrorInfo(err).msg),
    );
  }

  private run(p: Promise<unknown>): void {
    p.then(
      () => this.refresh(),
      (err: unknown) => this.showError(toErrorInfo(err).msg),
    );
  }

  private showError(msg: string | null): void {
    this.errorEl.hidden = msg === null;
    this.errorEl.textContent = msg ?? "";
  }

  private apply(status: SimStatus): void {
    const first = this.status === null;
    this.status = status;
    this.showError(null);
    this.requireEl("#bench-meta").textContent = `seed ${status.seed} · hub ${status.hub}`;
    this.renderBoard(status);
    this.renderCounter(status);
    this.renderTestPoints(status);
    this.renderPot(status, first);
    this.renderFaults(status);
  }

  private renderBoard(s: SimStatus): void {
    const b = s.board;
    this.requireEl("#board-name").textContent = b.name;
    this.requireEl("#board-desc").textContent = b.description;
    const power = this.requireEl("#board-power");
    power.className = b.powered ? BADGE_ON : BADGE_OFF;
    power.textContent = b.powered ? "powered" : "off";
    const facts: Array<[string, string]> = [
      ["Bus path", `${b.device} (hub ${s.hub} port ${b.port})`],
      ["Console", `${b.serial_source}, ${b.console}`],
      ["Uptime", formatUptime(b.uptime_s)],
      ["Boots", `${b.boots}, last reset: ${b.last_reset_cause}`],
    ];
    const dl = this.requireEl("#board-facts");
    dl.replaceChildren(
      ...facts.flatMap(([k, v]) => {
        const dt = document.createElement("dt");
        dt.className = "text-awic-fg-muted";
        dt.textContent = k;
        const dd = document.createElement("dd");
        dd.className = "m-0 break-all font-mono text-xs leading-5";
        dd.textContent = v;
        return [dt, dd];
      }),
    );
    this.requireEl<HTMLButtonElement>("#board-console").disabled = !b.powered;
    this.requireEl("#board-toggle").textContent = b.powered ? "Power off" : "Power on";
  }

  private renderCounter(s: SimStatus): void {
    const c = s.counter;
    this.requireEl("#u3-desc").textContent = c.description;
    updateSevenSeg(this.seven, c.display);
    const box = this.requireEl("#u3-inputs");
    box.replaceChildren(
      ...c.inputs.map((input) => {
        const button = document.createElement("button");
        button.type = "button";
        button.disabled = !s.board.powered;
        const on = input.state;
        button.className = input.momentary
          ? `${BTN} ${on ? "ring-2 ring-awic-accent" : ""}`
          : `${on ? BTN_ACCENT : BTN}`;
        button.title = input.description;
        button.textContent = input.momentary ? `${input.id} (pin ${input.pin})` : `${input.id} (pin ${input.pin}): ${on ? "on" : "off"}`;
        button.addEventListener("click", () =>
          this.set(input.momentary ? { input: input.id } : { input: input.id, state: !input.state }),
        );
        return button;
      }),
    );
    const verdict = this.requireEl("#u3-verdict");
    if (c.part && !verdict.textContent) {
      verdict.textContent = c.solved ? `Solved: ${c.part}` : `Revealed: ${c.part}`;
      verdict.className = "text-sm text-awic-ok";
    }
  }

  private renderTestPoints(s: SimStatus): void {
    const { rows, hiddenProbes } = testPointRows(s);
    this.requireEl("#tp-note").textContent =
      hiddenProbes > 0
        ? `Logic analyzer #1 has one probe on each of TP1–TP8, clipped on in a random order: "D?" means it is your job to find out which channel D0–D7 is on that point (the channel survey on the Graph tab, or survey_channels over MCP, helps).`
        : "Every probe's place is known.";
    const body = this.requireEl("#tp-rows");
    body.replaceChildren(
      ...rows.map((row) => {
        const tr = document.createElement("tr");
        tr.className = "border-t border-awic-border align-top";
        const cells = [row.id, row.name, row.description, row.probes.join(", ") || "—"];
        cells.forEach((text, i) => {
          const td = document.createElement("td");
          td.className = i === 0 ? "py-1 pr-3 font-mono text-xs" : "py-1 pr-3";
          td.textContent = text;
          tr.appendChild(td);
        });
        return tr;
      }),
    );
    const inst = this.requireEl("#instruments");
    inst.replaceChildren(
      ...s.instruments.map((i) => {
        const span = document.createElement("span");
        span.className = i.powered ? BADGE_ON : BADGE_OFF;
        const channels = i.probes.map((p) => `${channelName(i.label, p.channel)}→${p.test_point ?? "?"}`).join(" ");
        span.textContent = `${instrumentShortName(i.label)} · ${i.device} · port ${i.port}${i.powered ? "" : " (off)"}`;
        span.title = channels;
        return span;
      }),
    );
  }

  private renderPot(s: SimStatus, first: boolean): void {
    const pot = this.requireEl<HTMLInputElement>("#pot");
    // Don't yank the slider from under a drag.
    if (first || document.activeElement !== pot) pot.value = String(r2ToSlider(s.osc_r2_ohm));
    this.requireEl("#pot-value").textContent = `R2 = ${formatOhm(s.osc_r2_ohm)} → about ${formatHz(s.osc_nominal_hz)}`;
  }

  private renderFaults(s: SimStatus): void {
    const note = this.requireEl("#fault-note");
    if (s.mystery && !s.revealed) {
      note.textContent = "A mystery is running: one of these is in force. Find which from its symptoms, with the instruments.";
    } else {
      const active = s.faults.filter((f) => f.active).length;
      note.textContent = active === 0 ? "None in force. Pick some, or start a mystery." : `${active} in force.`;
    }
    const list = this.requireEl("#faults");
    list.replaceChildren(
      ...s.faults.map((f) => {
        const li = document.createElement("li");
        li.className = "flex items-start gap-2 text-sm";
        const box = document.createElement("input");
        box.type = "checkbox";
        box.className = "mt-1";
        box.id = `fault-${f.id}`;
        box.checked = f.active === true;
        box.indeterminate = f.active === undefined || f.active === null;
        box.addEventListener("change", () => {
          const ids = s.faults
            .filter((x) => (x.id === f.id ? box.checked : x.active === true))
            .map((x) => x.id);
          this.set({ faults: ids });
        });
        const label = document.createElement("label");
        label.htmlFor = box.id;
        const title = document.createElement("span");
        title.className = "font-medium";
        title.textContent = f.title;
        const symptom = document.createElement("span");
        symptom.className = "block text-awic-fg-muted";
        symptom.textContent = f.symptom;
        label.append(title, symptom);
        if (f.cause) {
          const cause = document.createElement("span");
          cause.className = "block text-awic-warn";
          cause.textContent = `Cause: ${f.cause}`;
          label.appendChild(cause);
        }
        li.append(box, label);
        return li;
      }),
    );
    this.requireEl<HTMLButtonElement>("#fault-reveal").disabled = s.revealed;
  }
}

customElements.define(TAG, BenchModule);

registerModule({
  id: "bench",
  title: "Bench",
  factory: () => document.createElement(TAG) as BenchModule,
  available: (ctx) =>
    ctx.ws.send("sim.status", {}).then(
      () => true,
      () => false,
    ),
});
