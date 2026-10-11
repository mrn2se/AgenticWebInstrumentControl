/**
 * The URL behind the graph module's **Image** button: a server-rendered view
 * of the current capture (SPECIFICATION.md §5.6).
 *
 * The browser already draws live traces in WebGL, so this is not how the graph
 * is displayed — it is how a view is *taken away*: opened in a tab, saved,
 * pasted into an issue, or handed to someone who was not at the bench. The
 * server draws it so that the picture a person shares and the picture an agent
 * receives from `render_view` are the same picture, measured the same way.
 */

export type ImageKind = "waveform" | "eye" | "histogram" | "anomalies";

/** The views that are about one channel, and so need one chosen. */
const SINGLE_CHANNEL: readonly ImageKind[] = ["eye", "histogram", "anomalies"];

export function isSingleChannel(kind: ImageKind): boolean {
  return SINGLE_CHANNEL.includes(kind);
}

export interface ImageViewParams {
  capture: string;
  kind: ImageKind;
  /** Channel positions to draw; empty draws all. The eye kind takes one. */
  channels?: number[];
  /** Window within the capture, seconds from its start. */
  t0_s?: number;
  t1_s?: number;
  width?: number;
  height?: number;
}

/**
 * Builds the `/api/render` URL. Only parameters that are actually set are
 * sent, so the server's own defaults stay in one place rather than being
 * duplicated here — a second set of defaults in the browser is how the two
 * drift apart.
 */
export function imageViewUrl(params: ImageViewParams): string {
  const query = new URLSearchParams();
  query.set("capture", params.capture);
  query.set("kind", params.kind);
  if (params.channels && params.channels.length > 0) {
    query.set("channels", params.channels.join(","));
  }
  for (const [key, value] of [
    ["t0_s", params.t0_s],
    ["t1_s", params.t1_s],
    ["width", params.width],
    ["height", params.height],
  ] as const) {
    if (value !== undefined && Number.isFinite(value)) query.set(key, String(value));
  }
  return `/api/render?${query.toString()}`;
}

/**
 * Why the button cannot be used right now, or `null` when it can.
 *
 * Returned as a sentence rather than a boolean because it becomes the button's
 * `title`: a control that is disabled without saying why is a control a person
 * clicks repeatedly.
 */
export function imageUnavailableReason(
  capture: string | null,
  kind: ImageKind,
  selectedChannels: number[],
): string | null {
  if (!capture) return "Run a capture first — there is nothing to draw yet.";
  if (isSingleChannel(kind) && selectedChannels.length !== 1) {
    return selectedChannels.length === 0
      ? `The ${kind} view is about one channel; none is selected.`
      : `The ${kind} view is about one channel; ${selectedChannels.length} are selected.`;
  }
  return null;
}
