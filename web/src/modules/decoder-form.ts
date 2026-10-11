import type { ChannelRole, DecoderId, DecoderInfo, DecoderOptionSpec, OptionSetting, OptionValue } from "../proto/types";

/**
 * Turns `decode.list`'s metadata plus whatever a user has typed into a
 * "Decode" form into the `channels`/`options` payload `capture.decode`/
 * `rec.decode`/the `decode` terminal source all take (SPECIFICATION.md §6,
 * T21) — or a list of validation errors, so the form can be built with no
 * hardcoded protocol knowledge (the same reason `decode.list` itself carries
 * types/defaults/allowed-values rather than a fixed schema per decoder).
 *
 * Deliberately pure: no DOM here, so the role-mapping/option-validation
 * logic is testable without a document, matching `graph/measurements.ts`'s
 * `SendCaptureMeasure` shape. Shared (not nested under `modules/graph`)
 * because both the graph module's "Decode" side-panel section and the
 * terminal module's `decode` source-picker form build the same
 * `channels`/`options` payload from the same `decode.list` metadata; each
 * caller owns turning this into its own `<select>`/`<input>` elements.
 */

/** One channel available to bind a decoder role to, e.g. `{ channel: 5, label: "D5" }` (mirrors `capabilities.ts`'s `DigitalChannelControl`, kept independent so this module has no dependency on capability parsing). */
export interface AvailableChannel {
  readonly channel: number;
  readonly label: string;
}

/** The form's current role -> channel choice; `undefined` (or absent) means "not yet bound". */
export type RoleBindings = ReadonlyMap<ChannelRole, number | undefined>;

/** An option field's raw value as the DOM naturally gives it: a checkbox is a `boolean`, everything else (a text/number/select input) a `string` to be parsed per the option's declared type. */
export type RawOptionValue = string | boolean;
export type OptionInputs = ReadonlyMap<string, RawOptionValue>;

export interface DecodeFormValidationError {
  /** A role name (`"scl"`) or option id (`"baud"`) — whichever field the error is about. */
  readonly field: string;
  readonly message: string;
}

export interface ValidDecodeConfig {
  readonly decoder: DecoderId;
  readonly channels: readonly {
    readonly role: ChannelRole;
    readonly channel: number;
    /** Analog captures only: the role's line is `channel - minus` (SPEC §6.3), e.g. CANH minus CANL. */
    readonly minus?: number;
  }[];
  readonly options: readonly OptionSetting[];
}

/**
 * The parts of the form that only exist for an analog capture (SPEC §6.3):
 * a differential partner per role, and the front end's own threshold and dead
 * band. Those two are not in `decode.list`'s per-decoder options - the server
 * takes them for any decoder on an analog capture and cuts the waveform with
 * them before the decoder sees a bit - so they are carried separately here and
 * folded into `options` on the way out.
 *
 * Absent (or `undefined`) fields are simply not sent, leaving the server to
 * find the threshold itself, which is what it does well.
 */
export interface AnalogDecodeForm {
  /** Role -> the channel to subtract from it. */
  readonly minus?: RoleBindings;
  readonly thresholdMv?: RawOptionValue;
  readonly hysteresisMv?: RawOptionValue;
}

export type DecodeFormResult =
  | { readonly status: "valid"; readonly config: ValidDecodeConfig }
  | { readonly status: "invalid"; readonly errors: readonly DecodeFormValidationError[] };

/**
 * Which roles must actually be bound before a decoder instance can be built.
 * Every role is required except SPI's data lines, where `awic_decode::spi`
 * requires only CLK plus *at least one* of MOSI/MISO (CS always optional) —
 * `DecoderInfo.channels` alone can't express that "at least one of" rule
 * generically, so it is hardcoded here for the one decoder it applies to
 * rather than guessed at from the role list.
 */
function validateRoleBindings(
  decoderId: DecoderId,
  roles: readonly ChannelRole[],
  bindings: RoleBindings,
): DecodeFormValidationError[] {
  const errors: DecodeFormValidationError[] = [];
  const boundChannelOf = new Map<number, ChannelRole>();

  for (const role of roles) {
    const channel = bindings.get(role);
    // SPI: only CLK is unconditionally required — MOSI/MISO are each
    // individually optional (the "at least one of" rule is checked
    // separately, below) and CS is always optional.
    const isOptionalForSpi = decoderId === "spi" && role !== "clk";
    if (channel === undefined) {
      if (!isOptionalForSpi) errors.push({ field: role, message: `${role.toUpperCase()} must be bound to a channel` });
      continue;
    }
    const clash = boundChannelOf.get(channel);
    if (clash) {
      errors.push({ field: role, message: `channel is already bound to ${clash.toUpperCase()}` });
    } else {
      boundChannelOf.set(channel, role);
    }
  }

  if (decoderId === "spi" && bindings.get("mosi") === undefined && bindings.get("miso") === undefined) {
    errors.push({ field: "mosi", message: "at least one of MOSI/MISO must be bound" });
  }
  return errors;
}

function parseOptionValue(
  spec: DecoderOptionSpec,
  raw: RawOptionValue,
): { readonly ok: true; readonly value: OptionValue } | { readonly ok: false; readonly message: string } {
  switch (spec.type) {
    case "int": {
      const n = typeof raw === "boolean" ? NaN : Number(raw);
      if (!Number.isFinite(n) || !Number.isInteger(n)) {
        return { ok: false, message: `${spec.id} must be a whole number` };
      }
      if (spec.min !== null && spec.min !== undefined && n < spec.min) {
        return { ok: false, message: `${spec.id} must be at least ${spec.min}` };
      }
      if (spec.max !== null && spec.max !== undefined && n > spec.max) {
        return { ok: false, message: `${spec.id} must be at most ${spec.max}` };
      }
      return { ok: true, value: n };
    }
    case "bool": {
      const b = typeof raw === "boolean" ? raw : raw === "true";
      return { ok: true, value: b };
    }
    case "enum": {
      const s = typeof raw === "boolean" ? String(raw) : raw;
      if (!spec.values.includes(s)) {
        return { ok: false, message: `${spec.id} must be one of ${spec.values.join(", ")}` };
      }
      return { ok: true, value: s };
    }
    default: {
      const exhaustive: never = spec;
      throw new Error(`parseOptionValue: unhandled option type ${JSON.stringify(exhaustive)}`);
    }
  }
}

/**
 * Builds the payload for `capture.decode`/`rec.decode`/the `decode`
 * terminal source, or the list of everything wrong with the form so far.
 * An option the user never touched (absent from `optionInputs`) is omitted
 * from the result entirely — the server already defaults it (SPEC: "options
 * not supplied take the default `decode.list` reported"), so there is
 * nothing to validate or send for it.
 */
export function buildDecodeConfig(
  decoder: DecoderInfo,
  bindings: RoleBindings,
  optionInputs: OptionInputs,
  analog: AnalogDecodeForm = {},
): DecodeFormResult {
  const errors: DecodeFormValidationError[] = [...validateRoleBindings(decoder.id, decoder.channels, bindings)];
  errors.push(...validateMinusBindings(decoder.channels, bindings, analog.minus));

  const options: OptionSetting[] = [];
  for (const spec of decoder.options) {
    const raw = optionInputs.get(spec.id);
    if (raw === undefined) continue;
    const parsed = parseOptionValue(spec, raw);
    if (!parsed.ok) {
      errors.push({ field: spec.id, message: parsed.message });
    } else {
      options.push({ id: spec.id, value: parsed.value });
    }
  }

  for (const [id, raw] of [
    ["threshold_mv", analog.thresholdMv],
    ["hysteresis_mv", analog.hysteresisMv],
  ] as const) {
    if (raw === undefined || raw === "") continue;
    const mv = typeof raw === "boolean" ? NaN : Number(raw);
    if (!Number.isFinite(mv) || !Number.isInteger(mv)) {
      errors.push({ field: id, message: `${id} must be a whole number of millivolts` });
    } else if (id === "hysteresis_mv" && mv < 0) {
      // The same rule the server enforces, said before the round trip.
      errors.push({ field: id, message: "hysteresis_mv must not be negative" });
    } else {
      options.push({ id, value: mv });
    }
  }

  if (errors.length > 0) return { status: "invalid", errors };

  const channels = decoder.channels
    .map((role) => ({ role, channel: bindings.get(role), minus: analog.minus?.get(role) }))
    .filter((c): c is { role: ChannelRole; channel: number; minus: number | undefined } => c.channel !== undefined)
    .map(({ role, channel, minus }) => (minus === undefined ? { role, channel } : { role, channel, minus }));
  return { status: "valid", config: { decoder: decoder.id, channels, options } };
}

/**
 * A differential partner has to be a channel, and not the role's own line:
 * `channel - channel` is zero everywhere, which decodes as a flat bus rather
 * than as the mistake it is.
 */
function validateMinusBindings(
  roles: readonly ChannelRole[],
  bindings: RoleBindings,
  minus: RoleBindings | undefined,
): DecodeFormValidationError[] {
  if (!minus) return [];
  const errors: DecodeFormValidationError[] = [];
  for (const role of roles) {
    const partner = minus.get(role);
    if (partner === undefined) continue;
    const channel = bindings.get(role);
    if (channel === undefined) {
      errors.push({ field: role, message: `${role.toUpperCase()} has a minus channel but no channel of its own` });
    } else if (partner === channel) {
      errors.push({ field: role, message: `${role.toUpperCase()} cannot be its own minus channel` });
    }
  }
  return errors;
}

/** Pre-fills a form's option inputs with each option's own declared default, so a freshly opened form shows real values rather than blanks. */
export function initialOptionInputs(decoder: DecoderInfo): Map<string, RawOptionValue> {
  const inputs = new Map<string, RawOptionValue>();
  for (const spec of decoder.options) {
    inputs.set(spec.id, spec.type === "bool" ? spec.default : String(spec.default));
  }
  return inputs;
}
