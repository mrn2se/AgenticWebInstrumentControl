import { describe, expect, it } from "vitest";
import { buildDecodeConfig, initialOptionInputs, type OptionInputs, type RoleBindings } from "./decoder-form";
import type { DecoderInfo } from "../proto/types";

const uart: DecoderInfo = {
  id: "uart",
  name: "UART",
  channels: ["rx"],
  options: [
    { type: "int", id: "baud", description: "Baud rate", default: 115200, min: 1, max: null },
    { type: "int", id: "data_bits", description: "Data bits", default: 8, min: 5, max: 9 },
    { type: "enum", id: "parity", description: "Parity", default: "none", values: ["none", "even", "odd"] },
    { type: "bool", id: "invert", description: "Invert idle polarity", default: false },
  ],
};

const i2c: DecoderInfo = { id: "i2c", name: "I2C", channels: ["scl", "sda"], options: [] };

const spi: DecoderInfo = {
  id: "spi",
  name: "SPI",
  channels: ["clk", "mosi", "miso", "cs"],
  options: [{ type: "int", id: "word_size", description: "Bits per word", default: 8, min: 1, max: 32 }],
};

function bindings(entries: readonly (readonly [string, number])[]): RoleBindings {
  return new Map(entries as [DecoderInfo["channels"][number], number][]);
}

describe("buildDecodeConfig: role bindings", () => {
  it("is valid once every required role is bound", () => {
    const result = buildDecodeConfig(uart, bindings([["rx", 5]]), new Map());
    expect(result.status).toBe("valid");
    if (result.status === "valid") {
      expect(result.config.channels).toEqual([{ role: "rx", channel: 5 }]);
      expect(result.config.decoder).toBe("uart");
    }
  });

  it("reports a missing required role by name", () => {
    const result = buildDecodeConfig(uart, new Map(), new Map());
    expect(result.status).toBe("invalid");
    if (result.status === "invalid") {
      expect(result.errors).toEqual([{ field: "rx", message: "RX must be bound to a channel" }]);
    }
  });

  it("requires every role for a decoder with no optional lines (I2C)", () => {
    const result = buildDecodeConfig(i2c, bindings([["scl", 0]]), new Map());
    expect(result.status).toBe("invalid");
    if (result.status === "invalid") {
      expect(result.errors.map((e) => e.field)).toEqual(["sda"]);
    }
  });

  it("rejects two roles bound to the same channel", () => {
    const result = buildDecodeConfig(i2c, bindings([["scl", 0], ["sda", 0]]), new Map());
    expect(result.status).toBe("invalid");
    if (result.status === "invalid") {
      expect(result.errors[0]!.message).toContain("already bound to SCL");
    }
  });

  it("SPI: CLK plus only MOSI is valid (MISO/CS optional)", () => {
    const result = buildDecodeConfig(spi, bindings([["clk", 0], ["mosi", 1]]), new Map());
    expect(result.status).toBe("valid");
  });

  it("SPI: CLK alone with neither MOSI nor MISO is invalid", () => {
    const result = buildDecodeConfig(spi, bindings([["clk", 0]]), new Map());
    expect(result.status).toBe("invalid");
    if (result.status === "invalid") {
      expect(result.errors.some((e) => e.message.includes("at least one of MOSI/MISO"))).toBe(true);
    }
  });

  it("SPI: CS may stay unbound even when everything else is set", () => {
    const result = buildDecodeConfig(spi, bindings([["clk", 0], ["mosi", 1], ["miso", 2]]), new Map());
    expect(result.status).toBe("valid");
    if (result.status === "valid") {
      expect(result.config.channels.map((c) => c.role)).toEqual(["clk", "mosi", "miso"]);
    }
  });
});

describe("buildDecodeConfig: options", () => {
  it("omits an option the user never touched", () => {
    const result = buildDecodeConfig(uart, bindings([["rx", 0]]), new Map());
    expect(result.status).toBe("valid");
    if (result.status === "valid") expect(result.config.options).toEqual([]);
  });

  it("parses and includes an int option within range", () => {
    const inputs: OptionInputs = new Map([["baud", "9600"]]);
    const result = buildDecodeConfig(uart, bindings([["rx", 0]]), inputs);
    expect(result.status).toBe("valid");
    if (result.status === "valid") expect(result.config.options).toEqual([{ id: "baud", value: 9600 }]);
  });

  it("rejects a non-integer int option", () => {
    const inputs: OptionInputs = new Map([["baud", "fast"]]);
    const result = buildDecodeConfig(uart, bindings([["rx", 0]]), inputs);
    expect(result.status).toBe("invalid");
    if (result.status === "invalid") expect(result.errors[0]!.field).toBe("baud");
  });

  it("rejects an int option out of its declared range", () => {
    const inputs: OptionInputs = new Map([["data_bits", "12"]]);
    const result = buildDecodeConfig(uart, bindings([["rx", 0]]), inputs);
    expect(result.status).toBe("invalid");
    if (result.status === "invalid") expect(result.errors[0]!.message).toContain("at most 9");
  });

  it("accepts a boolean option as an actual boolean", () => {
    const inputs: OptionInputs = new Map([["invert", true]]);
    const result = buildDecodeConfig(uart, bindings([["rx", 0]]), inputs);
    expect(result.status).toBe("valid");
    if (result.status === "valid") expect(result.config.options).toEqual([{ id: "invert", value: true }]);
  });

  it("rejects an enum option outside its allowed values", () => {
    const inputs: OptionInputs = new Map([["parity", "rot13"]]);
    const result = buildDecodeConfig(uart, bindings([["rx", 0]]), inputs);
    expect(result.status).toBe("invalid");
    if (result.status === "invalid") expect(result.errors[0]!.message).toContain("none, even, odd");
  });

  it("accepts a valid enum option", () => {
    const inputs: OptionInputs = new Map([["parity", "even"]]);
    const result = buildDecodeConfig(uart, bindings([["rx", 0]]), inputs);
    expect(result.status).toBe("valid");
    if (result.status === "valid") expect(result.config.options).toEqual([{ id: "parity", value: "even" }]);
  });

  it("reports every invalid field at once, not just the first", () => {
    const inputs: OptionInputs = new Map([
      ["baud", "nope"],
      ["parity", "rot13"],
    ]);
    const result = buildDecodeConfig(uart, new Map(), inputs);
    expect(result.status).toBe("invalid");
    if (result.status === "invalid") {
      expect(result.errors.map((e) => e.field).sort()).toEqual(["baud", "parity", "rx"]);
    }
  });
});

describe("initialOptionInputs", () => {
  it("seeds every option with its declared default, stringified except booleans", () => {
    const inputs = initialOptionInputs(uart);
    expect(inputs.get("baud")).toBe("115200");
    expect(inputs.get("parity")).toBe("none");
    expect(inputs.get("invert")).toBe(false);
  });

  it("round-trips cleanly through buildDecodeConfig (defaults are always valid)", () => {
    const inputs = initialOptionInputs(spi);
    const result = buildDecodeConfig(spi, bindings([["clk", 0], ["mosi", 1]]), inputs);
    expect(result.status).toBe("valid");
    if (result.status === "valid") expect(result.config.options).toEqual([{ id: "word_size", value: 8 }]);
  });
});

const can: DecoderInfo = {
  id: "can",
  name: "CAN",
  channels: ["can"],
  options: [
    { type: "enum", id: "bitrate", description: "Bit rate", default: "auto", values: ["auto", "500000"] },
    { type: "enum", id: "polarity", description: "Polarity", default: "dominant_low", values: ["dominant_low", "dominant_high"] },
  ],
};

// B-56: CAN on the scope was reachable over MCP but not from the Graph tab —
// no analog channel in the role selector, no way to name a differential
// partner, and no threshold fields. The payload those controls build:
describe("buildDecodeConfig: an analog capture", () => {
  it("sends a differential pair as channel + minus", () => {
    const result = buildDecodeConfig(can, bindings([["can", 1]]), new Map(), {
      minus: bindings([["can", 0]]),
    });
    expect(result.status).toBe("valid");
    if (result.status !== "valid") return;
    expect(result.config.channels).toEqual([{ role: "can", channel: 1, minus: 0 }]);
  });

  it("leaves minus out entirely when no partner was picked", () => {
    const result = buildDecodeConfig(can, bindings([["can", 0]]), new Map(), { minus: new Map() });
    expect(result.status).toBe("valid");
    if (result.status !== "valid") return;
    expect(result.config.channels).toEqual([{ role: "can", channel: 0 }]);
    expect("minus" in result.config.channels[0]!).toBe(false);
  });

  it("refuses a role that is its own minus channel", () => {
    const result = buildDecodeConfig(can, bindings([["can", 1]]), new Map(), {
      minus: bindings([["can", 1]]),
    });
    expect(result.status).toBe("invalid");
    if (result.status !== "invalid") return;
    expect(result.errors[0]?.message).toContain("cannot be its own minus channel");
  });

  it("carries the front end's threshold and hysteresis as options", () => {
    const result = buildDecodeConfig(can, bindings([["can", 1]]), new Map(), {
      thresholdMv: "2700",
      hysteresisMv: "250",
    });
    expect(result.status).toBe("valid");
    if (result.status !== "valid") return;
    expect(result.config.options).toEqual([
      { id: "threshold_mv", value: 2700 },
      { id: "hysteresis_mv", value: 250 },
    ]);
  });

  it("omits a threshold left blank, so the server finds one itself", () => {
    const result = buildDecodeConfig(can, bindings([["can", 1]]), new Map(), {
      thresholdMv: "",
      hysteresisMv: undefined,
    });
    expect(result.status).toBe("valid");
    if (result.status !== "valid") return;
    expect(result.config.options).toEqual([]);
  });

  it("refuses a negative hysteresis and a fractional threshold, as the server would", () => {
    const negative = buildDecodeConfig(can, bindings([["can", 1]]), new Map(), { hysteresisMv: "-10" });
    expect(negative.status).toBe("invalid");
    if (negative.status === "invalid") {
      expect(negative.errors[0]?.message).toContain("must not be negative");
    }
    const fractional = buildDecodeConfig(can, bindings([["can", 1]]), new Map(), { thresholdMv: "2.5" });
    expect(fractional.status).toBe("invalid");
    if (fractional.status === "invalid") {
      expect(fractional.errors[0]?.message).toContain("whole number of millivolts");
    }
  });

  it("still refuses a minus with no channel of its own", () => {
    const result = buildDecodeConfig(can, new Map(), new Map(), { minus: bindings([["can", 0]]) });
    expect(result.status).toBe("invalid");
    if (result.status !== "invalid") return;
    expect(result.errors.some((e) => e.message.includes("must be bound to a channel"))).toBe(true);
  });

  it("a digital decode is unchanged when no analog form is given", () => {
    const result = buildDecodeConfig(i2c, bindings([["scl", 6], ["sda", 4]]), new Map());
    expect(result.status).toBe("valid");
    if (result.status !== "valid") return;
    expect(result.config.channels).toEqual([
      { role: "scl", channel: 6 },
      { role: "sda", channel: 4 },
    ]);
    expect(result.config.options).toEqual([]);
  });
});

const oneWire: DecoderInfo = {
  id: "one_wire",
  name: "1-Wire",
  channels: ["owr"],
  options: [
    { type: "enum", id: "speed", description: "Bus speed", default: "standard", values: ["standard", "overdrive"] },
  ],
};

describe("buildDecodeConfig: 1-Wire", () => {
  it("needs its one line bound", () => {
    const result = buildDecodeConfig(oneWire, bindings([]), initialOptionInputs(oneWire));
    expect(result.status).toBe("invalid");
    if (result.status !== "invalid") return;
    expect(result.errors.map((e) => e.field)).toEqual(["owr"]);
  });

  it("builds a config with the speed it was given", () => {
    const inputs = initialOptionInputs(oneWire);
    inputs.set("speed", "overdrive");
    const result = buildDecodeConfig(oneWire, bindings([["owr", 2]]), inputs);
    expect(result.status).toBe("valid");
    if (result.status !== "valid") return;
    expect(result.config.channels).toEqual([{ role: "owr", channel: 2 }]);
    expect(result.config.options).toEqual([{ id: "speed", value: "overdrive" }]);
  });
});
