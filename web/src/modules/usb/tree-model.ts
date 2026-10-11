import type { DeviceInfo, DeviceState, UsbPort, UsbTreeNode, UsbTreeResult } from "../../proto/types";

/**
 * Pure view-model layer for the USB device tree (SPECIFICATION.md §4.1,
 * §4.4, §4.5, §5.1). No DOM here on purpose — `index.ts` renders this,
 * everything else about "what does the tree look like" is testable in a
 * plain Node environment (there is no jsdom/happy-dom in this project; see
 * `vite.config.ts`'s `test.environment: "node"`).
 *
 * The wire shape makes **ports** first-class, not just devices:
 * `UsbTreeNode.ports: UsbPort[]` lists every downstream port of a hub,
 * 1..=N, whether or not anything is attached (`UsbPort.device` is present
 * only when populated). Power state, switchability and the non-switchable
 * reason live on the `UsbPort`, not on the device — a port that is powered
 * off (or is protected) keeps its row and its toggle even though `device`
 * disappears, which is exactly what makes "power a port back on" possible at
 * all: if the row vanished with the device, there would be nothing left to
 * click. See the `UsbTreeModel` test for the regression this shape exists to
 * prevent (M1's "LA vanishes and reappears" exit test).
 *
 * Ports are *discovered*, not configured (SPECIFICATION.md §4.4 — the old
 * hand-written `[[usb.switchable]]` allowlist and its `port_not_allowlisted`
 * error are gone). `UsbPort.power_switchable` is the hardware fact (the hub's
 * `ppps`); `protected`/`protection` say whether switching is refused until
 * unlocked, derived from what is attached (a hub, HID, mass storage, a
 * wireless controller) or locked/unlocked by the user (`user_choice`);
 * `switchable` is the two combined — power-switchable and not protected —
 * and `non_switchable_reason` explains a false `switchable` either way. The
 * server refuses a protected port with error code `port_protected`.
 *
 * Two things worth restating because they're easy to get wrong:
 *
 * 1. Neither `UsbTreeNode` nor `UsbPort` carries an `id` string — only
 *    `bus` + `port_path` (on the node) and `number` (on the port). The
 *    format that reproduces `DeviceInfo.id` / event `device` fields
 *    (`"3-3.4.2"`, SPECIFICATION.md §2.3) is
 *    `${bus}-${port_path.join(".")}`. That's `formatId` below. A port's own
 *    path — whether or not it's populated — is its parent node's
 *    `port_path` with the port number appended; this is *why* the row's key
 *    survives a device disappearing: the key is derived from port position,
 *    never from the device that happens to occupy it.
 * 2. `UsbTreeNode`/`UsbPort` carry no `state: DeviceState` field —
 *    lifecycle (`detected`/`flashing`/`ready`/`error`) lives only on
 *    `DeviceInfo` and `device.state` events. Rendering the `flashing` ->
 *    `ready` transition therefore requires overlaying the flat device
 *    registry onto the tree by `formatId`, which is what
 *    `overlayDeviceStates` does.
 */

/** One renderable row of the tree: either a top-level device, or one physical port. */
export interface PortRow {
  /** `${bus}-${port_path.join(".")}` — stable across re-enumeration and across power cycles. */
  readonly key: string;
  readonly depth: number;
  readonly bus: number;
  /** This row's own position: the parent's `port_path` plus this port's number, when it's a port row. */
  readonly portPath: readonly number[];
  /** `null` only for a depth-0 root row — a device attached directly to the (unmodeled) bus root. */
  readonly portNumber: number | null;
  /** Formatted id of the parent hub, for `usb.power` requests. `null` when `hasToggle` is false. */
  readonly parentHub: string | null;
  /**
   * True for every row derived from a `UsbPort` (populated or empty) —
   * these can carry a toggle. False only for a depth-0 root row: the real
   * root-hub port that positions it isn't modeled on the wire, so there is
   * nothing to switch and nothing disabled-with-reason to show either.
   */
  readonly hasToggle: boolean;
  /** A port row with nothing attached. Always false for a root row. */
  readonly empty: boolean;
  readonly isHub: boolean;
  readonly isInstrument: boolean;
  readonly vid: number | null;
  readonly pid: number | null;
  readonly product: string | null;
  readonly speed: string | null;
  /** Number of ports this hub reports, i.e. its own `ports.length`. `null` for non-hubs and empty ports. */
  readonly portCount: number | null;
  readonly ppps: boolean | null;
  readonly power: boolean | null;
  /** Hardware fact: the hub can switch this port's power. `false` for the depth-0 root row. */
  readonly powerSwitchable: boolean;
  /** Whether switching is refused until the port is unlocked (SPEC §4.4). `false` for the root row. */
  readonly protected: boolean;
  /** The protection in force and why — derived from what's attached, or locked by the user. */
  readonly protection: string | null;
  /**
   * What the automatic rule says about this port, whatever the user chose —
   * e.g. "feeds hub 3-3.4…". Drives the unlock confirmation, so unlocking a
   * hub port warns even when the user had locked it explicitly first.
   */
  readonly derivedProtection: string | null;
  /** The user's choice on record: `true` locked, `false` unlocked, `null` automatic. */
  readonly userChoice: boolean | null;
  /** What this port feeds, as named with `usb.label` — `null` when unnamed. */
  readonly label: string | null;
  /** Marked as the device under test: switching this port resets what is being measured. */
  readonly dut: boolean;
  /** Power-switchable and not protected — the toggle may be offered right now. */
  readonly switchable: boolean;
  readonly nonSwitchableReason: string | null;
  /** Overlaid from the flat device registry by `key`; `null` when not tracked there. */
  readonly deviceState: DeviceState | null;
}

/** The join key between a node/port's bus position and `DeviceInfo.id` / event `device`. */
export function formatId(bus: number, portPath: readonly number[]): string {
  return `${bus}-${portPath.join(".")}`;
}

/** A port with nothing attached. */
export function isEmptyPort(port: Pick<UsbPort, "device">): boolean {
  return !port.device;
}

/** The port-derived fields a row carries; `null` for the depth-0 root row, which has no `UsbPort` behind it. */
interface PortInfo {
  readonly number: number;
  readonly power: boolean | null;
  readonly powerSwitchable: boolean;
  readonly protected: boolean;
  readonly protection: string | null;
  readonly derivedProtection: string | null;
  readonly userChoice: boolean | null;
  readonly label: string | null;
  readonly dut: boolean;
  readonly switchable: boolean;
  readonly reason: string | null;
}

function portInfoOf(port: UsbPort): PortInfo {
  return {
    number: port.number,
    power: port.power ?? null,
    powerSwitchable: port.power_switchable,
    protected: port.protected,
    protection: port.protection ?? null,
    derivedProtection: port.derived_protection ?? null,
    userChoice: port.user_choice ?? null,
    label: port.label ?? null,
    dut: port.dut ?? false,
    switchable: port.switchable,
    reason: port.non_switchable_reason ?? null,
  };
}

function deviceRow(node: UsbTreeNode, depth: number, hasToggle: boolean, portInfo: PortInfo | null, parentHub: string | null): PortRow {
  return {
    key: formatId(node.bus, node.port_path),
    depth,
    bus: node.bus,
    portPath: node.port_path,
    portNumber: portInfo ? portInfo.number : null,
    parentHub,
    hasToggle,
    empty: false,
    isHub: node.is_hub,
    isInstrument: node.is_instrument,
    vid: node.vid,
    pid: node.pid,
    product: node.product ?? null,
    speed: node.speed ?? null,
    portCount: node.is_hub ? node.ports.length : null,
    ppps: node.ppps ?? null,
    power: portInfo ? portInfo.power : null,
    powerSwitchable: portInfo ? portInfo.powerSwitchable : false,
    protected: portInfo ? portInfo.protected : false,
    protection: portInfo ? portInfo.protection : null,
    derivedProtection: portInfo ? portInfo.derivedProtection : null,
    userChoice: portInfo ? portInfo.userChoice : null,
    label: portInfo ? portInfo.label : null,
    dut: portInfo ? portInfo.dut : false,
    switchable: portInfo ? portInfo.switchable : false,
    nonSwitchableReason: portInfo ? portInfo.reason : null,
    deviceState: null,
  };
}

function emptyPortRow(port: UsbPort, parent: UsbTreeNode, depth: number, parentHub: string): PortRow {
  const portPath = [...parent.port_path, port.number];
  const info = portInfoOf(port);
  return {
    key: formatId(parent.bus, portPath),
    depth,
    bus: parent.bus,
    portPath,
    portNumber: port.number,
    parentHub,
    hasToggle: true,
    empty: true,
    isHub: false,
    isInstrument: false,
    vid: null,
    pid: null,
    product: null,
    speed: null,
    portCount: null,
    ppps: null,
    power: info.power,
    powerSwitchable: info.powerSwitchable,
    protected: info.protected,
    protection: info.protection,
    derivedProtection: info.derivedProtection,
    userChoice: info.userChoice,
    label: info.label,
    dut: info.dut,
    switchable: info.switchable,
    nonSwitchableReason: info.reason,
    deviceState: null,
  };
}

/** Depth-first flatten of a `usb.tree` result into renderable rows, root and port order preserved. */
export function flattenTree(result: UsbTreeResult): PortRow[] {
  const rows: PortRow[] = [];

  const visitNode = (node: UsbTreeNode, depth: number, hasToggle: boolean, portInfo: PortInfo | null, parentHub: string | null): void => {
    rows.push(deviceRow(node, depth, hasToggle, portInfo, parentHub));
    if (!node.is_hub) return;
    const hubId = formatId(node.bus, node.port_path);
    for (const port of node.ports) {
      if (port.device) {
        visitNode(port.device, depth + 1, true, portInfoOf(port), hubId);
      } else {
        rows.push(emptyPortRow(port, node, depth + 1, hubId));
      }
    }
  };

  for (const root of result.roots) {
    visitNode(root, 0, false, null, null);
  }
  return rows;
}

/** Overlays lifecycle `state` from the flat device registry onto tree rows, by bus-path key. */
export function overlayDeviceStates(rows: readonly PortRow[], devices: readonly DeviceInfo[]): PortRow[] {
  const byId = new Map(devices.map((d) => [d.id, d]));
  return rows.map((row) => {
    const device = byId.get(row.key);
    if (!device) return row;
    return { ...row, deviceState: device.state };
  });
}

/**
 * The `0925:3881` collision (SPECIFICATION.md §5.1): the standalone logic analyzer
 * clone and an FX2 oscilloscope with its physical switch in logic position
 * enumerate with the same VID:PID and no serial. VID:PID is therefore not a
 * unique key on this bench — only bus path is. This returns a clarifying
 * note to render next to the product name so the two stay distinguishable
 * and neither is silently mislabeled.
 */
export function collisionNote(vid: number | null, pid: number | null): string | null {
  return vid === 0x0925 && pid === 0x3881 ? "fx2lafw logic analyzer, or an FX2 oscilloscope with its switch in logic position" : null;
}

function rowsEqual(a: PortRow, b: PortRow): boolean {
  return (
    a.vid === b.vid &&
    a.pid === b.pid &&
    a.product === b.product &&
    a.speed === b.speed &&
    a.portCount === b.portCount &&
    a.ppps === b.ppps &&
    a.power === b.power &&
    a.powerSwitchable === b.powerSwitchable &&
    a.protected === b.protected &&
    a.protection === b.protection &&
    a.derivedProtection === b.derivedProtection &&
    a.userChoice === b.userChoice &&
    a.switchable === b.switchable &&
    a.nonSwitchableReason === b.nonSwitchableReason &&
    a.deviceState === b.deviceState &&
    a.empty === b.empty &&
    a.isHub === b.isHub &&
    a.isInstrument === b.isInstrument &&
    a.depth === b.depth &&
    a.hasToggle === b.hasToggle &&
    a.portNumber === b.portNumber &&
    a.parentHub === b.parentHub
  );
}

export interface ApplyResult {
  /** All current rows, in display order. */
  readonly rows: readonly PortRow[];
  /** Keys present before and after this `apply()` whose rendered content differs. */
  readonly changedKeys: readonly string[];
  /** Keys present before this `apply()` that are gone now — the port position itself was dropped. */
  readonly removedKeys: readonly string[];
  /** Keys that did not exist before this `apply()`. */
  readonly addedKeys: readonly string[];
}

/**
 * Keeps the last-rendered tree keyed by bus-path so the DOM layer can update
 * a row in place — rather than remove-then-recreate — whenever its content
 * changes at a path that already existed. Because a port's key is derived
 * from its *position* (parent path + port number), not from whatever device
 * happens to occupy it, this holds for both of the cases the task cares
 * about:
 *
 * - re-enumeration (SPEC §4.5): `flashing` -> `ready` is one transition, a
 *   device's VID:PID changing at a stable path is an update, not a
 *   remove+add;
 * - a port being powered off: the device disappears (`device` goes from
 *   present to absent) but the `UsbPort` — and therefore the row and its
 *   toggle — stays, so the very control that turned it off is still there
 *   to turn it back on.
 *
 * A key is only ever reported in `removedKeys` when the server's tree no
 * longer has *any* node or port at that position at all (e.g. an upstream
 * hub itself went away).
 */
export class UsbTreeModel {
  private rows = new Map<string, PortRow>();

  apply(result: UsbTreeResult, devices: readonly DeviceInfo[] = []): ApplyResult {
    const flat = overlayDeviceStates(flattenTree(result), devices);
    const nextRows = new Map(flat.map((r) => [r.key, r] as const));

    const removedKeys: string[] = [];
    for (const key of this.rows.keys()) {
      if (!nextRows.has(key)) removedKeys.push(key);
    }

    const changedKeys: string[] = [];
    const addedKeys: string[] = [];
    for (const [key, row] of nextRows) {
      const prev = this.rows.get(key);
      if (!prev) {
        addedKeys.push(key);
      } else if (!rowsEqual(prev, row)) {
        changedKeys.push(key);
      }
    }

    this.rows = nextRows;
    return { rows: flat, changedKeys, removedKeys, addedKeys };
  }

  get(key: string): PortRow | undefined {
    return this.rows.get(key);
  }

  get size(): number {
    return this.rows.size;
  }
}
