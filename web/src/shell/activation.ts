import { Emitter } from "../ws/emitter";

/**
 * Cross-module "switch to this tab, and here's a hint about what to do
 * once you're there" channel — the USB tree module uses this to jump to the
 * Terminal tab and open a specific serial source when its terminal icon is
 * clicked (task brief §7), without either module importing the other.
 *
 * `AppShell` is the sole subscriber that actually switches tabs; a target
 * module (e.g. the terminal module) additionally subscribes for its own id
 * to react to `detail`. One shared singleton, like `registerModule`'s
 * module-level registry below it.
 */
export interface ActivationRequest {
  readonly moduleId: string;
  readonly detail?: unknown;
}

export const activationRequests = new Emitter<ActivationRequest>();

/** Requests that `moduleId`'s tab become active, optionally passing `detail`
 * for that module to interpret (its own listener on {@link activationRequests}). */
export function requestModuleActivation(moduleId: string, detail?: unknown): void {
  activationRequests.emit({ moduleId, detail });
}
