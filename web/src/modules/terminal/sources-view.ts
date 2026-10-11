import type { TerminalKind, TerminalSourceInfo } from "../../proto/types";

/** `term.sources`, grouped for the toolbar's source picker (task brief §6). */
export interface SourceGroup {
  readonly kind: TerminalKind;
  readonly label: string;
  readonly sources: readonly TerminalSourceInfo[];
}

const KIND_LABEL: Record<TerminalKind, string> = {
  serial: "Serial",
  log: "Log",
  scpi: "SCPI",
  decode: "Decode",
};

/** Fixed display order for the kinds this build knows about; anything else
 * still appears, grouped by its own wire name, after these — additive,
 * never hidden. */
const KNOWN_KIND_ORDER: readonly TerminalKind[] = ["serial", "scpi", "decode", "log"];

/** Groups `term.sources`' flat list by kind, in a stable display order. */
export function groupSources(sources: readonly TerminalSourceInfo[]): SourceGroup[] {
  const groups: SourceGroup[] = [];
  const seen = new Set<TerminalKind>();

  for (const kind of KNOWN_KIND_ORDER) {
    const inKind = sources.filter((s) => s.kind === kind);
    seen.add(kind);
    if (inKind.length > 0) {
      groups.push({ kind, label: KIND_LABEL[kind] ?? kind, sources: inKind });
    }
  }

  const extraKinds = [...new Set(sources.map((s) => s.kind))].filter((k) => !seen.has(k));
  for (const kind of extraKinds) {
    groups.push({ kind, label: kind, sources: sources.filter((s) => s.kind === kind) });
  }

  return groups;
}

/** Finds one source by its `source` id across the flat list — used to look
 * up the currently-selected option's availability/reason for display. */
export function findSource(
  sources: readonly TerminalSourceInfo[],
  source: string,
): TerminalSourceInfo | undefined {
  return sources.find((s) => s.source === source);
}
