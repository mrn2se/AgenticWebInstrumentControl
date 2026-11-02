import "./styles/index.css";
import "./shell/app-shell";
import type { AppShell } from "./shell/app-shell";
import type { ModuleContext } from "./shell/context";
import { WsClient } from "./ws/client";
import { Store } from "./store/store";
import { createInitialState } from "./store/state";
import { wireWsToStore } from "./store/wire";
import { ThemeController } from "./shell/theme-controller";
import { type Backend, demoBackend, selfBackend } from "./backend/backend";
import type { WelcomeDialog, WelcomeTab } from "./welcome/welcome-dialog";

// Module registration seam (see shell/registry.ts): each module directory
// self-registers as an import side effect. T8 adds `./modules/usb`, T9 adds
// `./modules/graph` — append their imports here, in the order tabs should
// appear.
import "./modules/graph";
import "./modules/usb";
import "./modules/terminal";
import "./modules/mcp";

const LINK_BUTTON = "text-awic-accent underline hover:opacity-80";

const rootEl = document.getElementById("app");
if (!rootEl) throw new Error("main: #app root missing from index.html");
const root: HTMLElement = rootEl;
const theme = new ThemeController();

/** Brings up the app against one backend. */
function mount(backend: Backend): void {
  const store = new Store(createInitialState());
  const ws = new WsClient({ url: backend.wsUrl, ...(backend.socketFactory ? { socketFactory: backend.socketFactory } : {}) });
  wireWsToStore(ws, store);
  const ctx: ModuleContext = { ws, store, theme, backend };
  const shell = document.createElement("awic-app") as AppShell;
  shell.setContext(ctx);
  root.appendChild(shell);
  ws.connect();
}

/** The public site's legal page, reachable from every state of it. */
function legalLink(): HTMLAnchorElement {
  const link = document.createElement("a");
  link.href = "./legal.html";
  link.className = "underline hover:opacity-80";
  link.textContent = "Legal notice and privacy";
  return link;
}

/** The GitHub Pages build (`vite build --mode pages`) has no server: it greets, and offers the simulated bench. */
function startHosted(): void {
  let demoRunning = false;
  let dialog: WelcomeDialog | null = null;

  const empty = document.createElement("div");
  empty.className = "m-auto max-w-md p-6 text-center text-sm text-awic-fg-muted";
  const reopen = document.createElement("button");
  reopen.type = "button";
  reopen.className = LINK_BUTTON;
  reopen.textContent = "Start the demo or install AWIC";
  reopen.addEventListener("click", () => void openWelcome("demo"));
  const legal = document.createElement("p");
  legal.className = "m-0 mt-3";
  legal.appendChild(legalLink());
  empty.append(reopen, legal);
  root.appendChild(empty);

  const startDemo = async (): Promise<void> => {
    if (demoRunning) return;
    demoRunning = true;
    const { demoSocketFactory } = await import("./demo");
    empty.remove();
    const notice = document.createElement("div");
    notice.id = "demo-notice";
    notice.className = "bg-awic-warn-bg px-4 py-1.5 text-sm text-awic-warn";
    const install = document.createElement("button");
    install.type = "button";
    install.className = "underline hover:opacity-80";
    install.textContent = "Install AWIC";
    install.addEventListener("click", () => void openWelcome("install"));
    notice.append("Simulated bench: every signal, port and log entry here is made up in your browser, and no hardware is touched. ", install, " to use your own instruments. ", legalLink());
    root.appendChild(notice);
    mount(demoBackend(demoSocketFactory()));
  };

  const openWelcome = async (tab: WelcomeTab): Promise<void> => {
    if (!dialog) {
      const { WelcomeDialog } = await import("./welcome/welcome-dialog");
      dialog = new WelcomeDialog({ startDemo: () => void startDemo() });
      root.appendChild(dialog.element);
    }
    dialog.open(tab);
  };

  if (window.location.hash === "#demo") void startDemo();
  else void openWelcome("demo");
}

if (import.meta.env.MODE === "pages") startHosted();
else mount(selfBackend(window.location));
