import type { UsbPort, UsbTreeNode, UsbTreeResult } from "../../proto/types";

/**
 * Mock `usb.tree` payloads for tests, shaped like the real bench topology
 * documented in the task brief and SPECIFICATION.md §4.4/§5.1. The real
 * `awic-server` is still being built by another agent, so these stand in
 * for it.
 *
 * Ports are first-class (`UsbTreeNode.ports: UsbPort[]`): a hub reports
 * every physical port 1..=N, and `UsbPort.device` is only present when
 * something is attached. That is what lets "empty ports shown too" and "a
 * powered-off port keeps its toggle" both hold from the wire shape itself,
 * with no sentinel values needed.
 *
 * Ports are *discovered*, never configured (SPEC §4.4): there is no
 * allowlist. `power_switchable` is the hardware fact (`ppps`); `protected` /
 * `protection` say whether switching is refused until unlocked, derived from
 * what's attached or from the user's own `user_choice`; `switchable` is the
 * two combined.
 */

interface PortOptions {
  readonly powerSwitchable?: boolean;
  readonly power?: boolean | null;
  readonly protected?: boolean;
  readonly protection?: string | null;
  /** The rule's verdict, whatever the user chose. */
  readonly derivedProtection?: string | null;
  readonly userChoice?: boolean | null;
  readonly device?: UsbTreeNode | null;
}

function port(number: number, opts: PortOptions = {}): UsbPort {
  const powerSwitchable = opts.powerSwitchable ?? true;
  const protectedFlag = opts.protected ?? false;
  const protection = opts.protection ?? (protectedFlag ? "protected" : null);
  const switchable = powerSwitchable && !protectedFlag;
  const nonSwitchableReason = switchable ? null : !powerSwitchable ? "hub lacks per-port power switching" : protection;
  return {
    number,
    power: opts.power ?? (powerSwitchable ? false : null),
    power_switchable: powerSwitchable,
    protected: protectedFlag,
    protection,
    derived_protection: opts.derivedProtection ?? null,
    user_choice: opts.userChoice ?? null,
    switchable,
    non_switchable_reason: nonSwitchableReason,
    device: opts.device ?? null,
  };
}

function scopeNode(overrides: { vid: number; pid: number; product: string }): UsbTreeNode {
  return {
    bus: 3,
    port_path: [3, 4, 2],
    vid: overrides.vid,
    pid: overrides.pid,
    product: overrides.product,
    speed: "high",
    is_hub: false,
    ppps: null,
    is_instrument: true,
    ports: [],
  };
}

/**
 * The full bench tree from the task brief:
 *
 * ```
 * Bus 003
 * ├── 3-3        2109:2822  USB2.0 Hub, 4 ports, ppps
 * │   ├── port 3 → 0925:3881  fx2lafw logic analyzer (or FX2 oscilloscope in logic mode)  [powered]
 * │   └── port 4 → 3-3.4  0bda:5411  USB2.1 Hub, 4 ports, ppps  [powered]
 * │       └── port 2 → 04b4:602a  FX2 oscilloscope (unprogrammed)  [powered]
 * ```
 *
 * Ports 1 and 2 of `3-3`, and ports 1, 3, 4 of `3-3.4`, are empty but
 * power-switchable and unprotected. Port 4 of `3-3` is populated (by hub
 * `3-3.4`) and power-switchable, but automatically **protected** — a hub is
 * attached, so switching it would cut everything behind it (SPEC §4.4).
 */
export function benchTree(
  scope: { vid: number; pid: number; product: string } = { vid: 0x04b4, pid: 0x602a, product: "FX2 oscilloscope (unprogrammed)" },
): UsbTreeResult {
  const instrumentHub: UsbTreeNode = {
    bus: 3,
    port_path: [3, 4],
    vid: 0x0bda,
    pid: 0x5411,
    product: "USB2.1 Hub",
    speed: "high",
    is_hub: true,
    ppps: true,
    is_instrument: false,
    ports: [
      port(1),
      port(2, { power: true, device: scopeNode(scope) }),
      port(3),
      port(4),
    ],
  };

  const logicNode: UsbTreeNode = {
    bus: 3,
    port_path: [3, 3],
    vid: 0x0925,
    pid: 0x3881,
    product: null,
    speed: "high",
    is_hub: false,
    ppps: null,
    is_instrument: true,
    ports: [],
  };

  const upstreamHub: UsbTreeNode = {
    bus: 3,
    port_path: [3],
    vid: 0x2109,
    pid: 0x2822,
    product: "USB2.0 Hub",
    speed: "high",
    is_hub: true,
    ppps: true,
    is_instrument: false,
    ports: [
      port(1),
      port(2),
      port(3, { power: true, device: logicNode }),
      port(4, {
        power: true,
        protected: true,
        protection: "feeds hub 3-3.4: switching it cuts every device behind it",
        derivedProtection: "feeds hub 3-3.4: switching it cuts every device behind it",
        device: instrumentHub,
      }),
    ],
  };

  return { roots: [upstreamHub] };
}
