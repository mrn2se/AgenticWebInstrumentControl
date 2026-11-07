/**
 * The seven-segment display DS1, drawn as an SVG from the segments the
 * server says are lit (`"abcdef"` is a 0). Segment letters follow the usual
 * convention: a top, b top-right, c bottom-right, d bottom, e bottom-left,
 * f top-left, g middle.
 */

const SVG_NS = "http://www.w3.org/2000/svg";

/** Hexagonal segment outlines in a 60 × 100 box, slanted like a real display. */
const SEGMENT_POINTS: Readonly<Record<string, string>> = {
  a: "14,6 46,6 51,11 46,16 14,16 9,11",
  b: "52,13 57,18 55,44 50,49 45,44 47,18",
  c: "50,53 55,58 53,84 48,89 43,84 45,58",
  d: "8,86 40,86 45,91 40,96 8,96 3,91",
  e: "10,53 15,58 13,84 8,89 3,84 5,58",
  f: "12,13 17,18 15,44 10,49 5,44 7,18",
  g: "12,46 44,46 49,51 44,56 12,56 7,51",
};

export const SEGMENTS = ["a", "b", "c", "d", "e", "f", "g"] as const;

/** Which segments are lit, from the server's letter string. */
export function litSegments(display: string): ReadonlySet<string> {
  return new Set([...display.toLowerCase()].filter((c) => (SEGMENTS as readonly string[]).includes(c)));
}

/** Builds the display; call `updateSevenSeg` to change what it shows. */
export function createSevenSeg(): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "-4 0 70 102");
  svg.setAttribute("role", "img");
  svg.classList.add("h-28", "w-20");
  // A slight italic, as real seven-segment digits have.
  const g = document.createElementNS(SVG_NS, "g");
  g.setAttribute("transform", "skewX(-6) translate(6 0)");
  for (const seg of SEGMENTS) {
    const poly = document.createElementNS(SVG_NS, "polygon");
    poly.setAttribute("points", SEGMENT_POINTS[seg] ?? "");
    poly.dataset.segment = seg;
    g.appendChild(poly);
  }
  const dp = document.createElementNS(SVG_NS, "circle");
  dp.setAttribute("cx", "58");
  dp.setAttribute("cy", "93");
  dp.setAttribute("r", "4");
  dp.dataset.segment = "dp";
  g.appendChild(dp);
  svg.appendChild(g);
  updateSevenSeg(svg, "");
  return svg;
}

export function updateSevenSeg(svg: SVGSVGElement, display: string): void {
  const lit = litSegments(display);
  for (const el of svg.querySelectorAll<SVGElement>("[data-segment]")) {
    const on = lit.has(el.dataset.segment ?? "");
    el.setAttribute("fill", on ? "var(--color-awic-seg-on)" : "var(--color-awic-seg-off)");
  }
  svg.setAttribute("aria-label", lit.size === 0 ? "display blank" : `segments lit: ${[...lit].join(" ")}`);
}
