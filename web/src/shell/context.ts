import type { WsClient } from "../ws/client";
import type { Store } from "../store/store";
import type { AppState } from "../store/state";
import type { ThemeController } from "./theme-controller";

/** What every module gets on `init()`: the shared socket, the shared store, and the theme controller (SPEC §11.1). */
export interface ModuleContext {
  readonly ws: WsClient;
  readonly store: Store<AppState>;
  readonly theme: ThemeController;
}
