import { type InstallChoices, DEFAULT_CHOICES, afterInstallNote, buildInstallScript } from "./install-guide";
import { traceBannerSvg } from "./trace-banner";

export type WelcomeTab = "demo" | "install";

export interface WelcomeOptions {
  startDemo(): void;
}

const REPO = "https://github.com/mrn2se/AgenticWebInstrumentControl";
const DOCS: ReadonlyArray<readonly [string, string]> = [
  ["Installation", `${REPO}#install`],
  ["Development", `${REPO}/blob/main/docs/DEVELOPMENT.md`],
  ["Example reports", `${REPO}/tree/main/example-report`],
  ["Discussions", `${REPO}/discussions`],
];

const PRIMARY = "rounded-md border border-awic-accent bg-awic-accent px-4 py-2 text-sm font-medium text-awic-accent-fg hover:opacity-90";
const SECONDARY = "rounded-md border border-awic-border bg-awic-bg-raised px-3 py-1.5 text-sm font-medium hover:bg-awic-bg";
const INPUT = "w-full rounded-md border border-awic-border bg-awic-bg px-2.5 py-1.5 text-sm text-awic-fg";
const LINK = "text-awic-accent underline hover:opacity-80";
const FIELD_LABEL = "mb-1 block text-sm font-medium";
const TAB_ON = "border-awic-accent text-awic-fg";
const TAB_OFF = "border-transparent text-awic-fg-muted hover:text-awic-fg";

function esc(text: string): string {
  return text.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c] as string);
}

function radioGroup(name: string, legend: string, options: ReadonlyArray<readonly [string, string]>, current: string, disabled = false): string {
  return `
    <fieldset class="m-0 border-0 p-0 ${disabled ? "opacity-50" : ""}" ${disabled ? "disabled" : ""}>
      <legend class="mb-1 p-0 text-sm font-medium">${legend}</legend>
      <div class="flex flex-wrap gap-x-3 gap-y-1">
        ${options.map(([value, label]) => `
          <label class="flex items-center gap-1.5 text-sm">
            <input type="radio" name="${name}" value="${value}" ${value === current ? "checked" : ""} /> ${label}
          </label>`).join("")}
      </div>
    </fieldset>`;
}

/**
 * The greeting: try the simulated bench, or get the install commands. A native `<dialog>` for focus handling and Escape. Light
 * DOM with the shared Tailwind stylesheet, like everything else here.
 */
export class WelcomeDialog {
  readonly element: HTMLDialogElement;
  private tab: WelcomeTab = "demo";
  private choices: InstallChoices = DEFAULT_CHOICES;
  private readonly bodyEl: HTMLElement;
  private readonly tabsEl: HTMLElement;

  constructor(private readonly options: WelcomeOptions) {
    const dialog = document.createElement("dialog");
    dialog.id = "welcome";
    dialog.setAttribute("aria-labelledby", "welcome-title");
    dialog.className = "m-auto max-h-[calc(100dvh-2rem)] w-[min(44rem,calc(100vw-2rem))] overflow-y-auto rounded-lg border border-awic-border bg-awic-bg-raised p-0 text-awic-fg backdrop:bg-black/60";
    dialog.innerHTML = `
      <div class="border-b border-awic-border bg-awic-bg">${traceBannerSvg()}</div>
      <div class="p-5">
        <div class="mb-1 flex items-start justify-between gap-4">
          <h2 id="welcome-title" class="m-0 text-xl font-semibold">Agentic Web Instrument Control</h2>
          <button id="welcome-close" type="button" aria-label="Close" class="-mt-1 rounded px-2 py-1 text-lg leading-none text-awic-fg-muted hover:bg-awic-bg">×</button>
        </div>
        <p class="m-0 mb-4 max-w-[60ch] text-sm text-awic-fg-muted">
          Cheap USB oscilloscopes and logic analyzers, driven from your browser and by AI agents through MCP.
          AWIC runs as one small server on the machine they are plugged into.
        </p>
        <div id="welcome-tabs" role="tablist" class="mb-4 flex gap-1 overflow-x-auto border-b border-awic-border"></div>
        <div id="welcome-body" class="min-h-[17rem]"></div>
        <p class="m-0 mt-4 border-t border-awic-border pt-3 text-xs text-awic-fg-muted">
          Documentation:
          ${DOCS.map(([label, href]) => `<a class="${LINK} mr-2" href="${href}" target="_blank" rel="noopener noreferrer">${label}</a>`).join(" ")}
          <br />
          <a class="${LINK} mr-2" href="./legal.html#legal-notice">Legal notice</a>
          <a class="${LINK} mr-2" href="./legal.html#disclaimer">Disclaimer</a>
          <a class="${LINK}" href="./legal.html#privacy">Privacy</a>
        </p>
      </div>
    `;
    this.element = dialog;
    this.bodyEl = dialog.querySelector<HTMLElement>("#welcome-body") as HTMLElement;
    this.tabsEl = dialog.querySelector<HTMLElement>("#welcome-tabs") as HTMLElement;
    dialog.querySelector("#welcome-close")?.addEventListener("click", () => dialog.close());
    dialog.addEventListener("click", (e) => {
      if (e.target === dialog) dialog.close();
    });
  }

  open(tab: WelcomeTab): void {
    this.tab = tab;
    this.render();
    if (!this.element.open) this.element.showModal();
  }

  private render(): void {
    const tabs: ReadonlyArray<readonly [WelcomeTab, string]> = [
      ["demo", "Try the demo"],
      ["install", "Install AWIC"],
    ];
    this.tabsEl.replaceChildren(
      ...tabs.map(([id, label]) => {
        const button = document.createElement("button");
        button.type = "button";
        button.role = "tab";
        button.setAttribute("aria-selected", String(id === this.tab));
        button.dataset.tab = id;
        button.className = `-mb-px flex-none whitespace-nowrap border-b-2 px-3 py-1.5 text-sm font-medium ${id === this.tab ? TAB_ON : TAB_OFF}`;
        button.textContent = label;
        button.addEventListener("click", () => {
          this.tab = id;
          this.render();
        });
        return button;
      }),
    );
    if (this.tab === "demo") this.renderDemo();
    else this.renderInstall();
  }

  private renderDemo(): void {
    this.bodyEl.innerHTML = `
      <p class="m-0 mb-3 max-w-[60ch] text-sm">
        Use the real interface on a simulated bench. You do not need any hardware, and what you do here stays
        in this browser tab.
      </p>
      <ul class="m-0 mb-5 max-w-[60ch] list-disc pl-5 text-sm">
        <li>Capture from a logic analyzer with UART, I2C and SPI traffic on its lines, and decode it.</li>
        <li>Measure a calibration square and a sine on the oscilloscope, or record them live.</li>
        <li>Switch USB hub ports and watch devices leave and return.</li>
        <li>Type into a simulated serial console.</li>
        <li>Follow an AI agent's tool calls as it works through the bench.</li>
      </ul>
      <button id="welcome-demo" type="button" class="${PRIMARY}">Start the demo</button>
    `;
    this.bodyEl.querySelector("#welcome-demo")?.addEventListener("click", () => {
      this.options.startDemo();
      this.element.close();
    });
  }

  private renderInstall(): void {
    const c = this.choices;
    const script = buildInstallScript(c);
    // A demo is a fresh download for a PC, with nothing to set up and nothing to choose but attestation.
    const demo = c.action === "demo";
    const uninstall = c.action === "uninstall";
    const noPrerequisites = c.action === "update" || demo;
    const noDownload = uninstall;
    const noNetwork = demo || uninstall || (c.action === "update" && c.run === "systemd");
    this.bodyEl.innerHTML = `
      <div class="grid gap-4 md:grid-cols-[15rem_minmax(0,1fr)]">
        <form id="welcome-install" class="flex flex-col gap-3">
          ${radioGroup("action", "What do you want to do?", [["install", "New install"], ["update", "Update"], ["demo", "Run demo"], ["uninstall", "Uninstall"]], c.action)}
          ${radioGroup("run", "How does it run?", [["once", "Started by hand"], ["systemd", "systemd service"]], c.run, demo)}
          <div>
            <label class="${FIELD_LABEL}" for="welcome-arch">Machine</label>
            <select id="welcome-arch" class="${INPUT} ${demo || noDownload ? "opacity-50" : ""}" ${demo || noDownload ? "disabled" : ""}>
              ${([["auto", "Detect when it runs"], ["x86_64", "64-bit PC or server"], ["aarch64", "64-bit ARM, Raspberry Pi 3/4/5"], ["armv7", "32-bit ARM, Raspberry Pi 2/3/4"]] as const)
                .map(([value, label]) => `<option value="${value}" ${value === c.arch ? "selected" : ""}>${label}</option>`).join("")}
            </select>
          </div>
          <fieldset class="m-0 flex flex-col gap-1 border-0 p-0">
            <legend class="mb-1 p-0 text-sm font-medium">Options</legend>
            <label class="flex items-start gap-1.5 text-sm ${noPrerequisites ? "opacity-50" : ""}">
              <input type="checkbox" name="prerequisites" class="mt-1" ${c.prerequisites && !noPrerequisites ? "checked" : ""} ${noPrerequisites ? "disabled" : ""} />
              <span>
                ${uninstall ? "Also remove the USB permissions" : "Install firmware and USB permissions first"}
                <span class="ml-0.5 inline-flex items-center gap-0.5 whitespace-nowrap rounded bg-awic-warn-bg px-1 align-middle text-xs font-medium text-awic-warn">
                  <svg aria-hidden="true" viewBox="0 0 16 16" class="h-3 w-3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="7" width="10" height="7" rx="1.5"/><path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2"/></svg>
                  Needs sudo
                </span>
              </span>
            </label>
            <label class="flex items-start gap-1.5 text-sm ${noDownload ? "opacity-50" : ""}">
              <input type="checkbox" name="attestation" class="mt-1" ${c.attestation && !noDownload ? "checked" : ""} ${noDownload ? "disabled" : ""} />
              <span>
                Verify the GitHub build attestation (needs the GitHub CLI)
                <span class="ml-0.5 inline-flex items-center gap-0.5 whitespace-nowrap rounded bg-awic-ok-bg px-1 align-middle text-xs font-medium text-awic-ok">
                  <svg aria-hidden="true" viewBox="0 0 16 16" class="h-3 w-3" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M8 1.5 2.5 3.5v4c0 3.3 2.3 5.9 5.5 7 3.2-1.1 5.5-3.7 5.5-7v-4z"/><path d="m5.5 8 1.8 1.8L10.7 6.3"/></svg>
                  More secure
                </span>
                <a class="${LINK} text-xs" href="https://docs.github.com/en/actions/concepts/security/artifact-attestations" target="_blank" rel="noopener noreferrer">What is this?</a>
              </span>
            </label>
            <label class="flex items-start gap-1.5 text-sm ${noNetwork ? "opacity-50" : ""}">
              <input type="checkbox" name="network" class="mt-1" ${c.network && !demo && !uninstall ? "checked" : ""} ${noNetwork ? "disabled" : ""} />
              <span class="text-awic-danger">
                <svg aria-hidden="true" viewBox="0 0 16 16" class="mr-0.5 inline h-3.5 w-3.5 -translate-y-px" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M8 2 1.5 13.5h13z"/><path d="M8 6.5v3.2M8 11.6v.1"/></svg>
                Reachable from other machines: anyone on your network can drive the bench, with no login by default.
                <a class="font-medium underline hover:opacity-80" href="${REPO}#security" target="_blank" rel="noopener noreferrer">Read Security first</a>
              </span>
            </label>
          </fieldset>
        </form>
        <div class="min-w-0">
          <div class="mb-1 flex items-center justify-between gap-2">
            <span class="text-sm font-medium">Paste this into a terminal on that machine</span>
            <button id="welcome-copy" type="button" class="${SECONDARY}">Copy</button>
          </div>
          <pre id="welcome-script" tabindex="0" class="m-0 max-h-80 overflow-auto rounded-md border border-awic-border bg-awic-bg p-3 font-mono text-xs leading-relaxed"></pre>
          ${demo ? `<p class="m-0 mt-2 text-xs text-awic-fg-muted">The demo runs the virtual bench on a 64-bit Linux PC: a simulated board with two logic analyzers and a scope. No hardware, root, USB permissions or service. <a class="${LINK}" href="${REPO}/blob/main/docs/DEMOS.md#2-the-virtual-bench" target="_blank" rel="noopener noreferrer">What to try</a></p>` : ""}
          <p class="m-0 mt-2 text-xs text-awic-fg-muted">${esc(afterInstallNote(c))}</p>
        </div>
      </div>
    `;
    const pre = this.bodyEl.querySelector<HTMLElement>("#welcome-script") as HTMLElement;
    pre.textContent = script;
    const form = this.bodyEl.querySelector<HTMLFormElement>("#welcome-install") as HTMLFormElement;
    const read = (): void => {
      // Rendering replaces the form; keep keyboard users on the control they just changed.
      const focused = document.activeElement instanceof HTMLInputElement || document.activeElement instanceof HTMLSelectElement
        ? document.activeElement
        : null;
      const focusSelector = focused === null
        ? null
        : focused.id
          ? `#${focused.id}`
          : focused.type === "radio"
            ? `input[name="${focused.name}"][value="${focused.value}"]`
            : `input[name="${focused.name}"]`;
      const data = new FormData(form);
      const picked = data.get("action");
      const action = picked === "update" || picked === "demo" || picked === "uninstall" ? picked : "install";
      // Disabled controls are left out of the form data; keep their choices for when they come back.
      const runDisabled = form.querySelector<HTMLInputElement>('[name="run"]')?.matches(":disabled") ?? false;
      const archSelect = form.querySelector<HTMLSelectElement>("#welcome-arch") as HTMLSelectElement;
      const network = form.querySelector<HTMLInputElement>('[name="network"]') as HTMLInputElement;
      const attestation = form.querySelector<HTMLInputElement>('[name="attestation"]') as HTMLInputElement;
      const prerequisites = form.querySelector<HTMLInputElement>('[name="prerequisites"]') as HTMLInputElement;
      this.choices = {
        action,
        run: runDisabled ? this.choices.run : data.get("run") === "systemd" ? "systemd" : "once",
        arch: archSelect.disabled ? this.choices.arch : archSelect.value as InstallChoices["arch"],
        // A disabled box reads as off; the choice should survive a look at Update.
        prerequisites: prerequisites.disabled ? this.choices.prerequisites : prerequisites.checked,
        attestation: attestation.disabled ? this.choices.attestation : attestation.checked,
        network: network.disabled ? this.choices.network : network.checked,
      };
      this.renderInstall();
      if (focusSelector) this.bodyEl.querySelector<HTMLElement>(focusSelector)?.focus();
    };
    form.addEventListener("change", read);
    const copy = this.bodyEl.querySelector<HTMLButtonElement>("#welcome-copy") as HTMLButtonElement;
    copy.addEventListener("click", () => {
      void navigator.clipboard.writeText(script).then(
        () => { copy.textContent = "Copied"; },
        () => { copy.textContent = "Select and copy by hand"; },
      );
    });
  }
}
