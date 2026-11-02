import type { UsbPort, UsbTreeNode } from "../proto/types";

/** The demo's USB topology: which device hangs on which port, and which ports can be switched. */

export const LOGIC_ID = "3-3.4.1";
export const SCOPE_ID = "3-3.4.2";
export const ESP_ID = "3-3.1";
export const UART_BRIDGE_ID = "3-3.2";

interface Leaf {
  readonly vid: number;
  readonly pid: number;
  readonly product: string;
  readonly instrument?: boolean;
}

interface HubSpec {
  readonly bus: number;
  readonly path: readonly number[];
  readonly vid: number;
  readonly pid: number;
  readonly product: string;
  readonly switchable: boolean;
  readonly ports: Readonly<Record<number, Leaf | HubSpec>>;
  readonly portCount: number;
}

const isHub = (spec: Leaf | HubSpec): spec is HubSpec => "ports" in spec;

const ROOTS: readonly HubSpec[] = [
  {
    bus: 1,
    path: [1],
    vid: 0x05e3,
    pid: 0x0608,
    product: "USB2.0 Hub",
    switchable: false,
    portCount: 4,
    ports: { 2: { vid: 0x046d, pid: 0xc31c, product: "USB Keyboard" } },
  },
  {
    bus: 3,
    path: [3],
    vid: 0x2109,
    pid: 0x2822,
    product: "USB2.0 Hub",
    switchable: true,
    portCount: 4,
    ports: {
      1: { vid: 0x303a, pid: 0x1001, product: "ESP32-C6 dev board (USB JTAG/serial)" },
      2: { vid: 0x10c4, pid: 0xea60, product: "CP2102 USB to UART bridge" },
      4: {
        bus: 3,
        path: [3, 4],
        vid: 0x0bda,
        pid: 0x5411,
        product: "USB2.1 Hub",
        switchable: true,
        portCount: 4,
        ports: {
          1: { vid: 0x0925, pid: 0x3881, product: "fx2lafw logic analyzer", instrument: true },
          2: { vid: 0x1d50, pid: 0x608e, product: "FX2 oscilloscope", instrument: true },
        },
      },
    },
  },
];

export function busPath(bus: number, path: readonly number[]): string {
  return `${bus}-${path.join(".")}`;
}

export class UsbBench {
  /** Ports switched off, as `hub:port`. Everything else is on. */
  private readonly off = new Set<string>();
  /** The user's lock (`true`) or unlock (`false`); absent means the automatic rule decides. */
  private readonly choices = new Map<string, boolean>();

  private findHub(hub: string, roots: readonly HubSpec[] = ROOTS): HubSpec | null {
    for (const root of roots) {
      if (busPath(root.bus, root.path) === hub) return root;
      const nested = Object.values(root.ports).filter(isHub);
      const found = this.findHub(hub, nested);
      if (found) return found;
    }
    return null;
  }

  /** The port as the wire shows it, or `null` for a hub or port that does not exist. */
  port(hubId: string, number: number): UsbPort | null {
    const hub = this.findHub(hubId);
    if (!hub || number < 1 || number > hub.portCount) return null;
    return this.buildPort(hub, number);
  }

  /** Device id on this port, even while it is off. */
  deviceIdAt(hubId: string, number: number): string | null {
    const hub = this.findHub(hubId);
    if (!hub?.ports[number]) return null;
    return busPath(hub.bus, [...hub.path, number]);
  }

  isOn(deviceId: string): boolean {
    const [bus, rest] = deviceId.split("-");
    const parts = (rest ?? "").split(".").map(Number);
    for (let i = 1; i < parts.length; i++) {
      const hub = `${bus}-${parts.slice(0, i).join(".")}`;
      if (this.off.has(`${hub}:${parts[i]}`)) return false;
    }
    return true;
  }

  setPower(hub: string, number: number, on: boolean): void {
    if (on) this.off.delete(`${hub}:${number}`);
    else this.off.add(`${hub}:${number}`);
  }

  setChoice(hub: string, number: number, choice: boolean | null): void {
    if (choice === null) this.choices.delete(`${hub}:${number}`);
    else this.choices.set(`${hub}:${number}`, choice);
  }

  tree(): UsbTreeNode[] {
    return ROOTS.map((root) => this.buildHub(root));
  }

  private buildHub(hub: HubSpec): UsbTreeNode {
    const ports: UsbPort[] = [];
    for (let n = 1; n <= hub.portCount; n++) ports.push(this.buildPort(hub, n));
    return {
      bus: hub.bus,
      port_path: [...hub.path],
      vid: hub.vid,
      pid: hub.pid,
      product: hub.product,
      speed: "high",
      is_hub: true,
      ppps: hub.switchable,
      is_instrument: false,
      ports,
    };
  }

  private buildPort(hub: HubSpec, number: number): UsbPort {
    const hubId = busPath(hub.bus, hub.path);
    const key = `${hubId}:${number}`;
    const attached = hub.ports[number];
    const on = !this.off.has(key);
    const derived = attached && isHub(attached)
      ? `feeds hub ${busPath(attached.bus, attached.path)}: switching it cuts every device behind it`
      : null;
    const choice = this.choices.get(key) ?? null;
    const isProtected = choice ?? derived !== null;
    const protection = isProtected ? (derived && choice !== true ? derived : "locked by you") : null;
    const switchable = hub.switchable && !isProtected;
    let device: UsbTreeNode | null = null;
    if (attached && on) {
      device = isHub(attached)
        ? this.buildHub(attached)
        : {
            bus: hub.bus,
            port_path: [...hub.path, number],
            vid: attached.vid,
            pid: attached.pid,
            product: attached.product,
            speed: "high",
            is_hub: false,
            ppps: null,
            is_instrument: attached.instrument ?? false,
            ports: [],
          };
    }
    return {
      number,
      power: hub.switchable ? on : null,
      power_switchable: hub.switchable,
      protected: isProtected,
      protection,
      derived_protection: derived,
      user_choice: choice,
      switchable,
      non_switchable_reason: switchable ? null : hub.switchable ? protection : "hub lacks per-port power switching",
      device,
    };
  }
}
