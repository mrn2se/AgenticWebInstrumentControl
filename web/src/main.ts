import "./styles/index.css";
import "./shell/app-shell";
import type { AppShell } from "./shell/app-shell";
import type { ModuleContext } from "./shell/context";
import { WsClient } from "./ws/client";
import { Store } from "./store/store";
import { createInitialState } from "./store/state";
import { wireWsToStore } from "./store/wire";
import { ThemeController } from "./shell/theme-controller";

// Module registration seam (see shell/registry.ts): each module directory
// self-registers as an import side effect. T8 adds `./modules/usb`, T9 adds
// `./modules/graph` — append their imports here, in the order tabs should
// appear.
import "./modules/graph";
import "./modules/usb";
import "./modules/terminal";
import "./modules/mcp";

function wsUrl(): string {
  const scheme = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${window.location.host}/ws`;
}

const store = new Store(createInitialState());
const ws = new WsClient({ url: wsUrl() });
wireWsToStore(ws, store);
const theme = new ThemeController();

const ctx: ModuleContext = { ws, store, theme };

const root = document.getElementById("app");
if (!root) throw new Error("main: #app root missing from index.html");

const shell = document.createElement("awic-app") as AppShell;
shell.setContext(ctx);
root.appendChild(shell);

ws.connect();
