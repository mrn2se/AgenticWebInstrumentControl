import type { CapabilityInfo } from "../../proto/types";

/**
 * Builds the control panel's schema straight from `capabilities()`
 * (SPECIFICATION.md §5.2): "`Analog` yields channel/scale/offset controls,
 * `Digital` yields channel/threshold controls. A new driver gets a working
 * UI for free." No device model name appears anywhere in this file —
 * everything here is driven only by the `CapabilityInfo` union already
 * generated from Rust, so a driver this project hasn't written yet needs no
 * change here to get a working graph tab.
 *
 * Channel numbering: `CapabilityInfo` reports a channel *count* per
 * capability group, not per-channel indices, and the oscilloscope's hardware mode
 * switch (SPEC §5.1) means a single device exposes either one `Analog`
 * group or one `Digital` group in practice, never both. This numbers each
 * group's channels `0..channels-1` independently — the natural reading
 * absent an explicit global channel index on the wire — which is exactly
 * right for the single-group case and is flagged here as the assumption to
 * revisit if a future device reports more than one group of the same kind.
 */
export interface AnalogChannelControl {
  readonly kind: "analog";
  readonly channel: number;
  readonly label: string;
  readonly bits: number;
  readonly ratesHz: readonly number[];
  readonly rangesV: readonly number[];
}

export interface DigitalChannelControl {
  readonly kind: "digital";
  readonly channel: number;
  readonly label: string;
}

/**
 * The rate options to offer for a `Digital` capability: its exact ladder
 * when the driver reports one, else the single ceiling rate (`max_rate_hz`)
 * as a fallback for a driver built before the ladder existed on the wire
 * (SPEC §2.9.2 — the Digital capability used to carry only a
 * ceiling, so a user could not pick 1 MHz for the logic analyzer).
 */
function digitalRatesHz(cap: Extract<CapabilityInfo, { kind: "digital" }>): readonly number[] {
  return cap.rates_hz.length > 0 ? cap.rates_hz : [cap.max_rate_hz];
}

/**
 * A fingerprint of everything the channel/rate/trigger controls are built
 * from: the device's id and its capabilities, nothing about its *state*.
 *
 * The Graph tab rebuilds those controls from scratch when a device changes,
 * which resets the rate, ranges, offsets and trigger to defaults. It used to
 * do that on every store update, and a Start produces one (the device goes
 * busy, then ready): the first capture ran with the user's settings and every
 * later one at the device's top rate on the default range, triggered on a
 * channel nobody picked (B-65). Comparing this instead means the rebuild
 * happens when there is something new to build from.
 */
export function controlsFingerprint(
  deviceId: string | null,
  capabilities: readonly CapabilityInfo[] | undefined,
): string {
  return JSON.stringify([deviceId, capabilities ?? []]);
}

export type ChannelControl = AnalogChannelControl | DigitalChannelControl;

export interface DeviceControlSchema {
  readonly channels: readonly ChannelControl[];
  /**
   * Sample rates to offer in the timebase selector: the capability's exact
   * rate ladder (`Analog.rates_hz`, `Digital.rates_hz`), falling back to a
   * `Digital` capability's `max_rate_hz` alone for a driver that reports no
   * ladder.
   */
  readonly sampleRatesHz: readonly number[];
}

function analogControls(cap: Extract<CapabilityInfo, { kind: "analog" }>): AnalogChannelControl[] {
  return Array.from({ length: cap.channels }, (_, channel) => ({
    kind: "analog" as const,
    channel,
    label: `CH${channel + 1}`,
    bits: cap.bits,
    ratesHz: cap.rates_hz,
    rangesV: cap.ranges_v,
  }));
}

function digitalControls(cap: Extract<CapabilityInfo, { kind: "digital" }>): DigitalChannelControl[] {
  return Array.from({ length: cap.channels }, (_, channel) => ({
    kind: "digital" as const,
    channel,
    label: `D${channel}`,
  }));
}

/** Builds the device panel's schema from its capability list. Ignores `power_supply` (the PSU module's concern). */
export function buildControlSchema(capabilities: readonly CapabilityInfo[]): DeviceControlSchema {
  const channels: ChannelControl[] = [];
  let sampleRatesHz: readonly number[] = [];

  for (const cap of capabilities) {
    switch (cap.kind) {
      case "analog":
        channels.push(...analogControls(cap));
        sampleRatesHz = cap.rates_hz;
        break;
      case "digital":
        channels.push(...digitalControls(cap));
        if (sampleRatesHz.length === 0) sampleRatesHz = digitalRatesHz(cap);
        break;
      case "power_supply":
        break; // PSU module's concern, not the graph module's.
      default: {
        const exhaustive: never = cap;
        throw new Error(`buildControlSchema: unhandled capability kind ${JSON.stringify(exhaustive)}`);
      }
    }
  }

  return { channels, sampleRatesHz };
}

export function isAnalogControl(control: ChannelControl): control is AnalogChannelControl {
  return control.kind === "analog";
}

export function isDigitalControl(control: ChannelControl): control is DigitalChannelControl {
  return control.kind === "digital";
}
