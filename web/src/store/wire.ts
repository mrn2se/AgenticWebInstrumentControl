import type { WsClient } from "../ws/client";
import { toErrorInfo } from "../ws/errors";
import type { Store } from "./store";
import {
  type AppState,
  applyServerEvent,
  pushError,
  recordFrameGap,
  setConnectionState,
  setDevices,
} from "./state";

/**
 * The only place the store is fed from the network. Every mutation here
 * traces back to something the server said — a pushed event, or the result
 * of a request the client sent and is now relaying verbatim — never a
 * client-side guess at what the mutation it just requested will do
 * (SPECIFICATION.md §11).
 */
export function wireWsToStore(ws: WsClient, store: Store<AppState>): void {
  ws.connectionState.on((state) => {
    store.update((s) => setConnectionState(s, state));
    if (state === "connected") {
      ws.send("device.list", {}).then(
        (result) => store.update((s) => setDevices(s, result.devices)),
        (err: unknown) => store.update((s) => pushError(s, toErrorInfo(err))),
      );
    }
  });

  ws.events.on((event) => store.update((s) => applyServerEvent(s, event)));
  ws.errors.on((notice) => store.update((s) => pushError(s, notice)));
  ws.frameGaps.on((gap) => store.update((s) => recordFrameGap(s, gap)));
}
