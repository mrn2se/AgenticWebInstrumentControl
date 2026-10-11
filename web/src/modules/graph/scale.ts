import type { ChannelCalibration } from "../../proto/types";

/**
 * Raw device code -> physical units, using the driver's own calibration from
 * the `acq.configure` result: `value = raw * scale + offset`.
 *
 * The driver supplies this because only it knows it — ADC width, front-end
 * gain for the selected range, and per-range calibration offsets (the oscilloscope
 * stores these on the device). An earlier version derived volts here
 * from the capability's bit depth and the selected range instead, and read
 * the range as peak-to-peak where the protocol defines it as ± full scale:
 * every trace was drawn at half its true amplitude. Deriving volts in the
 * client cannot be made right; taking them from the driver can.
 */
export function codeToUnits(code: number, cal: ChannelCalibration): number {
  return code * cal.scale + cal.offset;
}

/** Shows raw codes unscaled, for a channel with no known calibration. */
export function identityCalibration(channel: number): ChannelCalibration {
  return { channel, scale: 1, offset: 0, unit: "" };
}
