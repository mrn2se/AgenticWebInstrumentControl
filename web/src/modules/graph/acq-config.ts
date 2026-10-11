import type { AcqConfig, Slope, Trigger } from "../../proto/types";

/**
 * Builds an `acq.configure` request body (SPEC §3.1) from the control
 * panel's form state — sample rate, sample count, enabled channels, and the
 * edge trigger (channel/slope/level). Kept separate from `index.ts`'s DOM
 * wiring so the mapping from "what the user picked" to "what goes on the
 * wire" is testable without a `document`.
 */
export interface TriggerFormState {
  readonly enabled: boolean;
  readonly channel: number;
  readonly slope: Slope;
  readonly level: number;
}

export interface AcqFormState {
  readonly rateHz: number;
  readonly samples: number;
  readonly enabledChannels: readonly number[];
  readonly trigger: TriggerFormState;
  /**
   * Analog only: the selected ± full-scale range for each enabled channel,
   * in `enabledChannels` order. Omitted for digital devices.
   */
  readonly rangesV?: readonly number[];
}

export function buildAcqConfig(form: AcqFormState): AcqConfig {
  const trigger: Trigger | null = form.trigger.enabled
    ? { kind: "edge", channel: form.trigger.channel, slope: form.trigger.slope, level: form.trigger.level }
    : null;
  const cfg: AcqConfig = {
    rate_hz: form.rateHz,
    channels: [...form.enabledChannels],
    samples: form.samples,
    trigger,
  };
  if (form.rangesV && form.rangesV.length > 0) cfg.ranges_v = [...form.rangesV];
  return cfg;
}

/**
 * The project convention (SPEC §5): the trigger event
 * lands at the capture's midpoint, giving equal pre- and post-trigger
 * history. Returns the sample index (0-based) that a trigger marker should
 * be drawn at for a capture of `samples` total.
 */
export function triggerSampleIndex(samples: number): number {
  return Math.floor(samples / 2);
}
