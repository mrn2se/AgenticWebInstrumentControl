import { LogicScene, WIRING } from "../demo/scene";

/**
 * The welcome dialog's one bit of decoration: real traces. The lanes are the
 * demo bench's own signals at a window where each shows its character, so what
 * greets a visitor is what the Graph tab then shows them.
 */
const LANES: ReadonlyArray<{ channel: number; decoder: "uart" | "i2c" | "spi"; span: number }> = [
  { channel: WIRING.uart.rx, decoder: "uart", span: 0.0012 },
  { channel: WIRING.i2c.sda, decoder: "i2c", span: 0.00012 },
  { channel: WIRING.i2c.scl, decoder: "i2c", span: 0.00012 },
  { channel: WIRING.spi.mosi, decoder: "spi", span: 0.000036 },
];

const WIDTH = 640;
const LANE_HEIGHT = 18;
const LANE_GAP = 8;

function lanePath(scene: LogicScene, channel: number, t0: number, span: number, top: number): string {
  const y = (level: number): number => top + (level ? 0 : LANE_HEIGHT);
  const x = (t: number): string => (((t - t0) / span) * WIDTH).toFixed(1);
  let level = scene.levelAt(channel, t0);
  let path = `M0 ${y(level)}`;
  for (const edge of scene.edgesBetween(channel, t0, t0 + span)) {
    path += `H${x(edge.t)}V${y(edge.rising ? 1 : 0)}`;
    level = edge.rising ? 1 : 0;
  }
  return `${path}H${WIDTH}`;
}

export function traceBannerSvg(): string {
  const scene = new LogicScene();
  const height = LANES.length * (LANE_HEIGHT + LANE_GAP) + LANE_GAP;
  const paths = LANES.map((lane, i) => {
    const top = LANE_GAP + i * (LANE_HEIGHT + LANE_GAP);
    const color = i % 2 === 0 ? "var(--color-awic-accent)" : "var(--color-awic-ok)";
    return `<path d="${lanePath(scene, lane.channel, scene.firstAt(lane.decoder), lane.span, top)}" fill="none" stroke="${color}" stroke-width="1.5" vector-effect="non-scaling-stroke"/>`;
  }).join("");
  return `<svg aria-hidden="true" class="awic-trace block w-full" viewBox="0 0 ${WIDTH} ${height}" preserveAspectRatio="none" style="height:${height}px">${paths}</svg>`;
}
