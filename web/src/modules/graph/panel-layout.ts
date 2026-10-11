import type { Layout, PanelSpec } from "../../render/types";
import type { RecChannel } from "../../proto/types";
import type { ChannelControl } from "./capabilities";

/** Per-channel volts/div range and offset the user picked for one analog channel (SPEC §5.2). */
export interface AnalogChannelSettings {
  readonly rangeV: number;
  readonly offsetV: number;
}

/**
 * Builds the renderer's `Layout` from the channels the user enabled, in the
 * order they're enabled — the bridge between `capabilities.ts`'s
 * capability-driven controls and `render/types.ts`'s renderer-facing panel
 * spec.
 */
export function buildLayout(
  enabledControls: readonly ChannelControl[],
  analogSettings: (channel: number) => AnalogChannelSettings,
): Layout {
  const panels: PanelSpec[] = enabledControls.map((control, lane) => {
    if (control.kind === "analog") {
      const settings = analogSettings(control.channel);
      return {
        kind: "analog",
        channel: control.channel,
        label: control.label,
        rangeV: settings.rangeV,
        offsetV: settings.offsetV,
      };
    }
    return { kind: "digital", channel: control.channel, label: control.label, lane };
  });
  return { panels };
}

/**
 * Live mode's channel list comes back as `RecChannel`s (SPEC §3.4), not the
 * `ChannelControl`s Single mode's capability-driven schema builds — but
 * `buildLayout` only needs `kind`/`channel`/`label`, so a Live recording's
 * channels can reuse it via this adapter rather than a parallel
 * "build a live layout" function. Kind is read off the name, using the same
 * `D<n>`/`CH<n>` convention `awic-server`'s `recorder::channel_name` writes
 * and `capabilities.ts` reads (SPEC §2.9.1). The analog-only fields
 * (`bits`/`ratesHz`/`rangesV`) are `buildLayout`-unused placeholders: the
 * caller supplies the actual range/offset per channel via its
 * `analogSettings` callback, exactly as for Single mode.
 */
export function controlFromRecChannel(rc: RecChannel): ChannelControl {
  if (rc.name.startsWith("D")) {
    return { kind: "digital", channel: rc.channel, label: rc.name };
  }
  return { kind: "analog", channel: rc.channel, label: rc.name, bits: 8, ratesHz: [], rangesV: [] };
}
