import { AwicModule } from "../../shell/module";
import { registerModule } from "../../shell/registry";
import { Popover } from "../../shell/popover";
import type {
  AcqConfig,
  ChannelCalibration,
  ChannelRole,
  DecodedAnnotation,
  DecoderInfo,
  DeviceInfo,
  RecChannel,
  RecListResult,
  RecordingSummary,
  Slope,
} from "../../proto/types";
import {
  createRenderer,
  DEFAULT_RENDERER_ID,
  listRenderers,
  RendererNotImplementedError,
  RendererUnavailableError,
  type RendererId,
  type WaveformRenderer,
} from "../../render/index";
import type { TriggerMarker } from "../../render/types";
import { readThemeColors } from "../../render/plotly-theme";
import { type ChannelControl, type DeviceControlSchema, buildControlSchema, controlsFingerprint, isAnalogControl, isDigitalControl } from "./capabilities";
import { chooseSelection } from "./device-selection";
import { buildAcqConfig, triggerSampleIndex, type TriggerFormState } from "./acq-config";
import {
  type AcquisitionUiState,
  applyAcqStateEvent,
  applyStopped,
  applyStreamStarted,
  initialAcquisitionState,
} from "./acquisition";
import {
  activityLine,
  deviceActivity,
  stopTarget,
  type DeviceActivity,
  type StartPhase,
} from "./device-activity";
import { collisionNote, deviceDisplayLabel } from "./device-label";
import { buildLayout, controlFromRecChannel, type AnalogChannelSettings } from "./panel-layout";
import { FrameSpanPlacer, frameToTraces, type PlacedFrame, TraceAccumulator, type TraceContext } from "./traces";
import { identityCalibration } from "./scale";
import { cursorReadout } from "./cursors";
import { formatSampleRate, formatSiDuration, formatSiFrequency } from "../../format";
import {
  addMeasurementStatus,
  captureHeader,
  requestMeasurement,
  measurementBlocks,
  measurementRows,
  ANALOG_MEASUREMENT_KINDS,
  DIGITAL_MEASUREMENT_KINDS,
  type CaptureInfo,
  type MeasurementStatus,
} from "./measurements";
import { pickRecordingToAttach, startedItsOwnRecording } from "./live-attach";
import {
  columnsForWidth,
  debounce,
  followWindowSeconds,
  initialLiveViewState,
  pauseAtViewport,
  resumeFollowing,
  type Debouncer,
  type LiveViewState,
} from "./live-state";
import { viewFrameToTraces, tracesTimeBounds } from "./view-traces";
import { formatRecordingStatus, headSeconds, NO_RECORDING_STATUS } from "./recording-status";
import {
  formatMatchTimestamp,
  formatWaitOutcome,
  searchOutcomeFromError,
  searchOutcomeFromResult,
  searchResultSummary,
  waitOutcomeFromError,
  waitOutcomeFromResult,
  type SearchOutcome,
} from "./search-ui";
import { buildDecodeConfig, initialOptionInputs, type RawOptionValue } from "../decoder-form";
import { annotationLaneFor, decodedFrameRows, formatDecodedFrameTimestamp } from "./decoded-frames";
import { imageUnavailableReason, imageViewUrl, isSingleChannel, type ImageKind } from "./image-view";

const TAG = "awic-module-graph";

// A colorless base plus exactly one color/emphasis variant per element (see
// `modules/terminal/index.ts`'s comment on the same pattern) — Tailwind's
// compiled rule order, not class-attribute order, decides when two classes
// on one element set the same property.
const BUTTON_BASE = "rounded-md border px-2.5 py-1 text-sm font-medium disabled:cursor-not-allowed disabled:opacity-60";
const BUTTON_NEUTRAL = "border-awic-border bg-awic-bg-raised hover:bg-awic-bg";
const BUTTON_PRIMARY = "border-awic-accent bg-awic-accent text-awic-accent-fg hover:bg-awic-accent";
const BUTTON_ACTIVE = "border-awic-accent bg-awic-accent text-awic-accent-fg";
const SELECT = "rounded border border-awic-border bg-awic-bg px-1.5 py-0.5 text-sm text-awic-fg";
const NUMBER_INPUT = "w-24 rounded border border-awic-border bg-awic-bg px-1.5 py-0.5 text-sm text-awic-fg";
const LABEL = "flex items-center gap-1.5 text-sm text-awic-fg-muted";
const HINT = "text-xs text-awic-fg-muted";
const POPOVER_PANEL =
  "absolute left-0 top-full z-10 mt-1 flex min-w-[16rem] flex-col gap-2 rounded-lg border border-awic-border bg-awic-bg-raised p-3 shadow-lg";
const POPOVER_ROW = "flex flex-wrap items-center gap-2 text-sm";
const SIDE_SECTION_TITLE = "mb-1.5 text-sm font-semibold";
/** Below this window width the side panel is a drawer over the plot; the `max-[900px]:` classes on it say the same. */
const SIDE_PANEL_DRAWER_PX = 900;

const ACQ_STATUS_CLASS: Record<string, string> = {
  running: "text-awic-ok",
  recording: "text-awic-ok",
  armed: "text-awic-accent",
  error: "text-awic-danger",
};

interface ChannelUiState {
  readonly control: ChannelControl;
  enabled: boolean;
  rangeV: number;
  offsetV: number;
}

type GraphMode = "single" | "live";

/**
 * The graph module (SPECIFICATION.md §5, §11.3, §5.5): a compact one-row
 * toolbar with popovers for settings (M15's layout rework — see
 * `shell/popover.ts`), a collapsible side panel for measurements and
 * pattern search, and two acquisition modes: **Single** (`acq.*`, the
 * capture store — unchanged from before M15) and **Live** (`rec.*`, a
 * follow view of a server-owned recording, §5.5). Device identity and
 * acquisition/recording state are read from the shared store/socket and the
 * server's own responses only — this module keeps no parallel truth about
 * what the server is doing (§11).
 */
class GraphModule extends AwicModule {
  // Assigned in `onInit()`, not the constructor — see `shell/module.ts`.
  private plotEl!: HTMLElement;
  private sidePanelEl!: HTMLElement;
  private panelToggleEl!: HTMLButtonElement;
  private deviceSelectEl!: HTMLSelectElement;
  private deviceNoteEl!: HTMLElement;
  private modeSingleEl!: HTMLButtonElement;
  private modeLiveEl!: HTMLButtonElement;
  private startButtonEl!: HTMLButtonElement;
  private stopButtonEl!: HTMLButtonElement;
  private followButtonEl!: HTMLButtonElement;
  private snapshotButtonEl!: HTMLButtonElement;
  private imageButtonEl!: HTMLButtonElement;
  private imageKindEl!: HTMLSelectElement;
  private imageChannelEl!: HTMLSelectElement;
  private acqStatusEl!: HTMLElement;
  private recStatusEl!: HTMLElement;
  private gapBannerEl!: HTMLElement;
  private plotHintEl!: HTMLElement;

  private channelsPopoverBtn!: HTMLButtonElement;
  private channelsPopoverPanel!: HTMLElement;
  private triggerPopoverBtn!: HTMLButtonElement;
  private triggerPopoverPanel!: HTMLElement;
  private acqPopoverBtn!: HTMLButtonElement;
  private acqPopoverPanel!: HTMLElement;
  private displayPopoverBtn!: HTMLButtonElement;
  private displayPopoverPanel!: HTMLElement;
  private popovers: Popover[] = [];
  private triggerPopover: Popover | null = null;

  private channelsEl!: HTMLElement;
  private rateSelectEl!: HTMLSelectElement;
  private samplesRowEl!: HTMLElement;
  private samplesInputEl!: HTMLInputElement;
  private liveWindowRowEl!: HTMLElement;
  private liveWindowInputEl!: HTMLInputElement;
  private triggerEnabledEl!: HTMLInputElement;
  private triggerChannelEl!: HTMLSelectElement;
  private triggerSlopeEl!: HTMLSelectElement;
  private triggerLevelEl!: HTMLInputElement;
  private triggerHintEl!: HTMLElement;
  private rendererSelectEl!: HTMLSelectElement;
  private rendererNoticeEl!: HTMLElement;
  private cursorAEl!: HTMLInputElement;
  private cursorBEl!: HTMLInputElement;
  private cursorReadoutEl!: HTMLElement;
  private viewStartEl!: HTMLInputElement;
  private viewEndEl!: HTMLInputElement;

  private measurementsEl!: HTMLElement;
  private snapshotSectionEl!: HTMLElement;
  private snapshotStatusEl!: HTMLElement;
  private snapshotChannelsEl!: HTMLElement;
  private searchSectionEl!: HTMLElement;
  private searchPatternEl!: HTMLInputElement;
  private searchButtonEl!: HTMLButtonElement;
  private searchErrorEl!: HTMLElement;
  private searchResultsEl!: HTMLElement;
  private waitTimeoutEl!: HTMLInputElement;
  private waitButtonEl!: HTMLButtonElement;
  private waitStatusEl!: HTMLElement;

  private decodeDecoderEl!: HTMLSelectElement;
  private decodeRolesEl!: HTMLElement;
  private decodeOptionsEl!: HTMLElement;
  private decodeApplyEl!: HTMLButtonElement;
  private decodeErrorEl!: HTMLElement;
  private decodeFramesEl!: HTMLElement;

  private unsubscribers: Array<() => void> = [];
  private plotResizeObserver: ResizeObserver | null = null;

  private devices: readonly DeviceInfo[] = [];
  /**
   * The selected device as last seen on the bus. While it is gone (port
   * power cut, unplugged, re-flashing) it stays selected under a "not
   * connected" entry instead of the picker jumping to another instrument,
   * which silently pointed the next Start at the wrong device and rebuilt the
   * controls, losing the rate and ranges (2026-10-09, scope power-cycled
   * under Live).
   */
  private selectedDeviceInfo: DeviceInfo | null = null;
  private selectedDeviceId: string | null = null;
  private schema: DeviceControlSchema = { channels: [], sampleRatesHz: [] };
  private channelState: ChannelUiState[] = [];
  private mode: GraphMode = "single";

  // --- Single mode (SPEC §3.1, unchanged behaviour from pre-M15) ---------
  private acqState: AcquisitionUiState = initialAcquisitionState();
  /** What the device reported it will actually do — traces are placed/scaled from this, never from the request. */
  private effective: { cfg: AcqConfig; calibration: ChannelCalibration[] } | null = null;
  private triggerForm: TriggerFormState = { enabled: false, channel: 0, slope: "rising", level: 0 };

  // --- Live mode (SPEC §5.5) ----------------------------------------------
  private liveRecId: number | null = null;
  private liveViewId: number | null = null;
  /**
   * View frames applied since Live mode last started, mirrored to
   * `data-live-frames` on this element. A live plot of a fast signal can
   * legitimately look identical frame to frame (every lane a solid toggled
   * band), so "is it still streaming?" is answered by counting, not by
   * comparing pixels — for the e2e test and for anyone inspecting the page.
   */
  private liveFrameCount = 0;
  private liveViewState: LiveViewState = initialLiveViewState();
  private liveChannels: readonly RecChannel[] = [];
  private liveRateHz = 0;
  private liveRecordings: readonly RecordingSummary[] = [];
  private liveList: RecListResult | null = null;
  private liveListTimer: ReturnType<typeof setInterval> | null = null;
  private liveSnapshotCapture: string | null = null;
  private readonly viewportDebouncer: Debouncer<void> = debounce(300, () => this.flushViewUpdate());

  private rendererId: RendererId = DEFAULT_RENDERER_ID;
  private renderer: WaveformRenderer | null = null;
  private viewportUnsubscribe: (() => void) | null = null;
  private measurementStatuses: MeasurementStatus[] = [];
  /** What each capture measured from was, for the header of its block of results. */
  private captureInfos = new Map<string, CaptureInfo>();
  /** The capture `renderMeasurements` last treated as current; a different one now means re-marking the blocks. */
  private measurementsCurrentCapture: string | null = null;

  // --- Decode (SPEC §6, T21): capture.decode (Single) / rec.decode (Live) ---
  private decoders: readonly DecoderInfo[] = [];
  /** What `onDeviceChanged()` last built the controls from (B-65). */
  private controlsBuiltFrom: string | null = null;
  /** This client's own in-flight start or stop, so the toolbar never says "idle" over one (B-67). */
  private phase: StartPhase = "none";
  /** Set when Stop is pressed while a start is still unanswered: the start is undone as soon as it names what it created (B-67). */
  private cancelPendingStart = false;
  /** Whether the Live recording on screen was started by this tab, and so is this tab's to clean up (B-68). */
  private liveRecStartedHere = false;
  /**
   * Where the capture on screen starts on the plot's time axis, seconds — the
   * `t0Ns` its sample frames carry. Decoded annotations count from the start of
   * what was decoded, so they are placed against this (B-71).
   */
  private captureT0Seconds = 0;
  /**
   * The capture on screen so far, and the stream it came from: a capture
   * larger than one frame arrives as several, and drawing only the latest
   * left most of it off the plot (B-72).
   */
  private streamTraces = new TraceAccumulator();
  private streamTracesFor: number | null = null;
  /** Holds each decimated frame until the time it covers is known (#48). */
  private streamPlacer = new FrameSpanPlacer(0);
  /** Whether anything of the capture on screen has been handed to the renderer yet. */
  private streamDrawn = false;
  /** Whether the plot has shown any data since the page loaded; until then it carries a hint instead of a blank area. */
  private plotHasData = false;
  /** Where the trigger of the Single capture on screen fired, if it was triggered. */
  private captureTrigger: TriggerMarker | null = null;
  /** A redraw of `streamTraces` already scheduled; frames arriving meanwhile join it. */
  private streamDrawTimer: ReturnType<typeof setTimeout> | null = null;
  /**
   * Whether the redraws still widen the viewport to the whole capture: until
   * the user zooms or pans the plot, after which their view is theirs.
   */
  private streamViewFollows = true;
  /** The origin the annotations now on screen were placed against (B-71). */
  private decodeOriginSeconds = 0;
  private decodeRoleBindings: Map<ChannelRole, number | undefined> = new Map();
  /** Analog captures only: the channel each role is decoded against, `channel - minus` (SPEC §6.3, B-56). */
  private decodeRoleMinus: Map<ChannelRole, number | undefined> = new Map();
  private decodeThresholdMv: string | undefined;
  private decodeHysteresisMv: string | undefined;
  private decodeOptionInputs: Map<string, RawOptionValue> = new Map();
  private decodeAnnotations: readonly DecodedAnnotation[] = [];
  /** Set once a decode has been applied in Live mode, so a viewport change re-decodes automatically (debounced) — never before the user has actually asked for a decode once. */
  private decodeActive = false;
  private readonly decodeDebouncer: Debouncer<void> = debounce(500, () => this.autoRedecode());

  protected override onInit(): void {
    // `min-h-0`/`flex-1` on the host and on the plot row below mirror
    // `modules/terminal/index.ts`'s layout: the toolbar is `flex-none`, the
    // row under it is the one flexible, definite-height region the plot
    // fills (SPEC §11.3 — never derived from content).
    this.className = "flex min-h-0 flex-1 flex-col overflow-hidden";
    this.innerHTML = `
      <div class="relative flex flex-none flex-wrap items-center gap-2 border-b border-awic-border bg-awic-bg-raised px-4 py-2">
        <label class="${LABEL}">Device <select id="device" class="${SELECT} w-56 truncate"></select></label>
        <!-- An icon with the note as its tooltip, never a line of text: the
             note comes and goes with the device, and the toolbar must not
             re-flow when it does. -->
        <span id="device-note" class="invisible flex-none cursor-help text-sm text-awic-fg-muted" aria-hidden="true">ⓘ</span>

        <div class="flex gap-1" role="group" aria-label="Mode">
          <button id="mode-single" type="button" class="${BUTTON_BASE} ${BUTTON_ACTIVE}">Single</button>
          <button id="mode-live" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Live</button>
        </div>

        <button id="start" type="button" class="${BUTTON_BASE} ${BUTTON_PRIMARY}">Start</button>
        <button id="stop" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Stop</button>
        <!-- Always present, disabled when they do not apply: a button that
             appears or disappears moves its neighbours under the pointer. -->
        <button id="follow" type="button" disabled class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Follow</button>
        <button id="snapshot" type="button" disabled class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Snapshot</button>

        <!-- SPEC 5.6: a server-drawn view of the current capture, to open,
             save or share. The same renderer an agent reaches through the
             render_view tool, so both see the same picture. -->
        <select id="image-kind" class="${SELECT}" title="Which view to draw">
          <option value="waveform">Waveform</option>
          <option value="eye">Eye</option>
          <option value="histogram">Histogram</option>
          <option value="anomalies">Anomalies</option>
        </select>
        <select id="image-channel" disabled class="${SELECT}" title="Channel to fold"></select>
        <button id="image" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Image</button>

        <div class="relative">
          <button id="popover-channels-btn" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Channels &amp; ranges</button>
          <div id="popover-channels-panel" hidden class="${POPOVER_PANEL}">
            <div id="channels" class="flex flex-col gap-1"></div>
          </div>
        </div>

        <div id="popover-trigger-wrap" class="relative">
          <button id="popover-trigger-btn" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Trigger</button>
          <div id="popover-trigger-panel" hidden class="${POPOVER_PANEL}">
            <div class="${POPOVER_ROW}">
              <label class="${LABEL}"><input id="trigger-enabled" type="checkbox" /> Edge trigger</label>
            </div>
            <div class="${POPOVER_ROW}">
              <label class="${LABEL}">Channel <select id="trigger-channel" class="${SELECT}"></select></label>
              <label class="${LABEL}">Slope
                <select id="trigger-slope" class="${SELECT}">
                  <option value="rising">Rising</option>
                  <option value="falling">Falling</option>
                </select>
              </label>
              <label class="${LABEL}">Level (V) <input id="trigger-level" type="number" step="0.1" value="0" class="${NUMBER_INPUT}" /></label>
            </div>
            <span id="trigger-hint" class="${HINT}"></span>
          </div>
        </div>

        <div class="relative">
          <button id="popover-acq-btn" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Acquisition</button>
          <div id="popover-acq-panel" hidden class="${POPOVER_PANEL}">
            <div class="${POPOVER_ROW}">
              <label class="${LABEL}">Sample rate <select id="rate" class="${SELECT}"></select></label>
            </div>
            <div id="samples-row" class="${POPOVER_ROW}">
              <label class="${LABEL}">Samples <input id="samples" type="number" min="1" step="1" value="1024" class="${NUMBER_INPUT}" /></label>
            </div>
            <div id="live-window-row" hidden class="${POPOVER_ROW}">
              <label class="${LABEL}">Follow window (s) <input id="live-window" type="number" min="0.001" step="0.01" value="0.02" class="${NUMBER_INPUT}" /></label>
            </div>
          </div>
        </div>

        <div class="relative">
          <button id="popover-display-btn" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Display</button>
          <div id="popover-display-panel" hidden class="${POPOVER_PANEL}">
            <div class="${POPOVER_ROW}">
              <label class="${LABEL}">Renderer <select id="renderer" class="${SELECT}"></select></label>
            </div>
            <span id="renderer-notice" class="${HINT}"></span>
            <div class="${POPOVER_ROW}">
              <label class="${LABEL}">Cursor A (s) <input id="cursor-a" type="number" step="any" value="0" class="${NUMBER_INPUT}" /></label>
              <label class="${LABEL}">Cursor B (s) <input id="cursor-b" type="number" step="any" value="0" class="${NUMBER_INPUT}" /></label>
            </div>
            <span id="cursor-readout" class="${HINT}"></span>
            <div class="${POPOVER_ROW}">
              <label class="${LABEL}">View start (s) <input id="view-start" type="number" step="any" class="${NUMBER_INPUT}" /></label>
              <label class="${LABEL}">View end (s) <input id="view-end" type="number" step="any" class="${NUMBER_INPUT}" /></label>
            </div>
            <span class="${HINT}">On the plot: wheel or pinch to zoom, drag, Shift+wheel or a sideways swipe to pan, double-click to fit.</span>
          </div>
        </div>

        <!-- A slot of fixed height and basis for the status: its text changes
             with every state, and wrapping it used to push the plot up and
             down. Long text is cut and kept whole in the tooltip. -->
        <div class="flex h-9 min-w-0 flex-1 basis-64 flex-col justify-center leading-tight">
          <span id="acq-status" class="truncate text-sm font-medium">idle</span>
          <span id="rec-status" class="${HINT} truncate"></span>
        </div>

        <button id="panel-toggle" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Panel</button>
      </div>

      <div class="relative flex min-h-0 flex-1">
        <div class="relative min-h-0 min-w-0 flex-1">
          <div id="plot" class="plot absolute inset-0 overflow-hidden bg-awic-bg [&>*]:h-full [&>*]:w-full"></div>
          <p id="plot-hint" class="pointer-events-none absolute inset-0 m-0 flex items-center justify-center p-6 text-center text-sm text-awic-fg-muted"></p>
          <div id="gap-banner" class="pointer-events-none absolute left-1/2 top-2 -translate-x-1/2 rounded-md bg-awic-danger-bg px-2 py-1 text-xs text-awic-danger empty:hidden"></div>
        </div>
        <!-- Beside the plot on a wide window; below ${SIDE_PANEL_DRAWER_PX}px it
             is a drawer over the plot's right edge, so it never squeezes the
             plot to a sliver. -->
        <aside id="side-panel" class="relative w-80 min-h-0 flex-none border-l border-awic-border bg-awic-bg-raised max-[900px]:absolute max-[900px]:inset-y-0 max-[900px]:right-0 max-[900px]:z-20 max-[900px]:max-w-[85%] max-[900px]:shadow-lg">
          <!--
            The side panel's content is absolutely positioned inside this
            "relative" host rather than laid out in normal flow (mirroring
            render/webgl2.ts's own GL/overlay canvas wrapper and
            modules/terminal/index.ts's #panes/pane pattern): an in-flow
            child's natural content height would otherwise propagate up
            through every ancestor's flex-1/min-h-0 pair (min-height:0 only
            lifts the floor, it does not cap growth) and inflate the page
            past the viewport the moment enough measurements/search results
            accumulate to exceed one screen — exactly the "Plotly growth
            bug" SPEC 11.3 warns about, just reached from a list of rows
            instead of a chart. Being out-of-flow, this div's own
            overflow-y-auto scrollbar is what absorbs that content instead,
            and the page never does.
          -->
          <div class="absolute inset-0 overflow-y-auto p-3">
            <section class="mb-4">
              <h3 class="${SIDE_SECTION_TITLE}">Measurements</h3>
              <p class="${HINT} mb-2">
                Measured by the server on the full-resolution capture, not on what the plot shows.
                A value that cannot be measured says why. Each capture gets its own block; results
                from an earlier capture are marked as such.
              </p>
              <div id="measurements"></div>
            </section>
            <section id="snapshot-section" hidden class="mb-4">
              <h3 class="${SIDE_SECTION_TITLE}">Snapshot</h3>
              <p id="snapshot-status" class="${HINT} mb-2">Click Snapshot to capture the visible range.</p>
              <div id="snapshot-channels" class="flex flex-col gap-1"></div>
            </section>
            <section id="search-section" hidden>
              <h3 class="${SIDE_SECTION_TITLE}">Pattern search</h3>
              <p class="${HINT} mb-2">For example <code>D0 rising</code> or <code>D0 f=1k 100010x101</code>.</p>
              <div class="mb-2 flex flex-wrap items-center gap-2">
                <input id="search-pattern" type="text" placeholder="pattern" class="min-w-0 flex-1 ${SELECT}" />
                <button id="search-btn" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Search</button>
              </div>
              <p id="search-error" class="mb-2 text-xs text-awic-danger empty:hidden"></p>
              <div id="search-results" class="mb-3 flex flex-col gap-1"></div>
              <div class="mb-1 flex flex-wrap items-center gap-2">
                <label class="${LABEL}">Timeout (s) <input id="wait-timeout" type="number" min="1" step="1" value="10" class="${NUMBER_INPUT}" /></label>
                <button id="wait-btn" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Wait for pattern</button>
              </div>
              <p id="wait-status" class="${HINT}"></p>
            </section>
            <section id="decode-section" class="mt-4">
              <h3 class="${SIDE_SECTION_TITLE}">Decode</h3>
              <p class="${HINT} mb-2">
                UART, I&sup2;C, SPI, CAN or 1-Wire over the current capture (Single) or the visible
                recording range (Live). On an analog capture a role can take a second channel to
                subtract (CANH − CANL), and the threshold is found for you unless you set it.
              </p>
              <div class="mb-2 flex flex-wrap items-center gap-2">
                <label class="${LABEL}">Protocol <select id="decode-decoder" class="${SELECT}"></select></label>
              </div>
              <div id="decode-roles" class="mb-2 flex flex-col gap-1"></div>
              <div id="decode-options" class="mb-2 flex flex-col gap-1"></div>
              <button id="decode-apply" type="button" class="${BUTTON_BASE} ${BUTTON_NEUTRAL}">Decode</button>
              <p id="decode-error" class="mt-2 text-xs text-awic-danger empty:hidden"></p>
              <div id="decode-frames" class="mt-2 flex flex-col gap-1"></div>
            </section>
          </div>
        </aside>
      </div>
    `;

    this.plotEl = this.requireEl("#plot");
    this.sidePanelEl = this.requireEl("#side-panel");
    this.panelToggleEl = this.requireEl("#panel-toggle");
    this.deviceSelectEl = this.requireEl<HTMLSelectElement>("#device");
    this.deviceNoteEl = this.requireEl("#device-note");
    this.modeSingleEl = this.requireEl("#mode-single");
    this.modeLiveEl = this.requireEl("#mode-live");
    this.startButtonEl = this.requireEl("#start");
    this.stopButtonEl = this.requireEl("#stop");
    this.followButtonEl = this.requireEl("#follow");
    this.snapshotButtonEl = this.requireEl("#snapshot");
    this.imageButtonEl = this.requireEl("#image");
    this.imageKindEl = this.requireEl("#image-kind");
    this.imageChannelEl = this.requireEl("#image-channel");
    this.acqStatusEl = this.requireEl("#acq-status");
    this.recStatusEl = this.requireEl("#rec-status");
    this.gapBannerEl = this.requireEl("#gap-banner");
    this.plotHintEl = this.requireEl("#plot-hint");

    this.channelsPopoverBtn = this.requireEl("#popover-channels-btn");
    this.channelsPopoverPanel = this.requireEl("#popover-channels-panel");
    this.triggerPopoverBtn = this.requireEl("#popover-trigger-btn");
    this.triggerPopoverPanel = this.requireEl("#popover-trigger-panel");
    this.acqPopoverBtn = this.requireEl("#popover-acq-btn");
    this.acqPopoverPanel = this.requireEl("#popover-acq-panel");
    this.displayPopoverBtn = this.requireEl("#popover-display-btn");
    this.displayPopoverPanel = this.requireEl("#popover-display-panel");

    this.channelsEl = this.requireEl("#channels");
    this.rateSelectEl = this.requireEl("#rate");
    this.samplesRowEl = this.requireEl("#samples-row");
    this.samplesInputEl = this.requireEl("#samples");
    this.liveWindowRowEl = this.requireEl("#live-window-row");
    this.liveWindowInputEl = this.requireEl("#live-window");
    this.triggerEnabledEl = this.requireEl("#trigger-enabled");
    this.triggerChannelEl = this.requireEl("#trigger-channel");
    this.triggerSlopeEl = this.requireEl("#trigger-slope");
    this.triggerLevelEl = this.requireEl("#trigger-level");
    this.triggerHintEl = this.requireEl("#trigger-hint");
    this.rendererSelectEl = this.requireEl("#renderer");
    this.rendererNoticeEl = this.requireEl("#renderer-notice");
    this.cursorAEl = this.requireEl("#cursor-a");
    this.cursorBEl = this.requireEl("#cursor-b");
    this.cursorReadoutEl = this.requireEl("#cursor-readout");
    this.viewStartEl = this.requireEl("#view-start");
    this.viewEndEl = this.requireEl("#view-end");

    this.measurementsEl = this.requireEl("#measurements");
    this.snapshotSectionEl = this.requireEl("#snapshot-section");
    this.snapshotStatusEl = this.requireEl("#snapshot-status");
    this.snapshotChannelsEl = this.requireEl("#snapshot-channels");
    this.searchSectionEl = this.requireEl("#search-section");
    this.searchPatternEl = this.requireEl("#search-pattern");
    this.searchButtonEl = this.requireEl("#search-btn");
    this.searchErrorEl = this.requireEl("#search-error");
    this.searchResultsEl = this.requireEl("#search-results");
    this.waitTimeoutEl = this.requireEl("#wait-timeout");
    this.waitButtonEl = this.requireEl("#wait-btn");
    this.waitStatusEl = this.requireEl("#wait-status");

    this.decodeDecoderEl = this.requireEl("#decode-decoder");
    this.decodeRolesEl = this.requireEl("#decode-roles");
    this.decodeOptionsEl = this.requireEl("#decode-options");
    this.decodeApplyEl = this.requireEl("#decode-apply");
    this.decodeErrorEl = this.requireEl("#decode-error");
    this.decodeFramesEl = this.requireEl("#decode-frames");

    this.renderRendererOptions();
    this.wireStaticControls();
    this.wirePopovers();
    // Closed by default on a narrow screen (SPEC §11.3); a normal desktop
    // width starts with it open, matching the module's pre-M15 behaviour of
    // always showing measurements. Crossing the width later does the same,
    // so a window narrowed with the panel open does not leave the plot a
    // sliver beside it.
    const narrow = window.matchMedia(`(max-width: ${SIDE_PANEL_DRAWER_PX}px)`);
    this.sidePanelEl.hidden = narrow.matches;
    narrow.addEventListener("change", (e) => {
      this.sidePanelEl.hidden = e.matches;
    });
    this.renderPlotHint();
    // Subscriptions/timers start in activate() so an inactive-but-mounted module does no background work.
  }

  override activate(): void {
    this.unsubscribers.push(
      this.ctx.store.subscribe((state) => {
        this.devices = state.devices;
        this.renderDeviceOptions();
        this.renderGapBanner(state.streams);
      }),
    );
    this.unsubscribers.push(
      this.ctx.ws.events.on((event) => {
        if (event.event === "acq.state") {
          this.acqState = applyAcqStateEvent(this.acqState, event);
          this.renderAcqStatus();
          // The capture's end places a decimated last frame still held (#48).
          if (event.stream === this.streamTracesFor && (event.state === "stopped" || event.state === "error")) {
            this.addStreamFrames(this.streamPlacer.finish(this.captureT0Seconds + this.streamTimebaseSeconds()));
          }
        } else if (event.event === "rec.state") {
          // Any recording, not only this tab's: one an agent or another tab
          // starts or stops holds the selected device just the same (B-31).
          void this.refreshLiveList();
        }
      }),
    );
    this.unsubscribers.push(
      this.ctx.ws.frames.on((frame) => {
        if (this.mode !== "single" || frame.header.streamId !== this.acqState.streamId || !this.renderer) return;
        if (this.streamTracesFor !== frame.header.streamId) {
          // The same origin the traces are placed at, so the frame list and the
          // plot share one axis (B-71) - the capture's first frame, where its
          // decoded annotations count from, not whichever frame came last.
          this.captureT0Seconds = Number(frame.header.t0Ns) / 1e9;
          this.cancelStreamDraw();
          this.streamTraces = new TraceAccumulator();
          this.streamPlacer = new FrameSpanPlacer(this.streamRateHz());
          this.streamTracesFor = frame.header.streamId;
          this.streamViewFollows = true;
          this.streamDrawn = false;
          this.captureTrigger = null;
        }
        this.addStreamFrames(this.streamPlacer.push(frame));
      }),
    );
    this.unsubscribers.push(
      this.ctx.ws.viewFrames.on((frame) => {
        const renderer = this.renderer;
        if (this.mode !== "live" || frame.header.viewId !== this.liveViewId || !renderer) return;
        // Frames still in flight for the view the user just left: a follow
        // frame after a zoom or pan, or a fixed-range one after Follow. Drawn,
        // they replaced the traces with data outside the new view and the
        // plot went blank until the server caught up; skipped, the old traces
        // stay on screen, scaled, until the right data arrives.
        if (frame.header.flags.follow !== this.liveViewState.following) return;
        const traces = viewFrameToTraces(frame, { rateHz: this.liveRateHz, channels: this.liveChannels });
        renderer.setTraces({ traces });
        this.liveFrameCount += 1;
        this.dataset.liveFrames = String(this.liveFrameCount);
        this.markPlotHasData();
        // A follow view's frames slide forward as the recording grows; a
        // renderer only auto-fits its viewport once (its own contract, SPEC
        // §5.3), so Live mode must drive it explicitly on every frame while
        // following, or the plot would freeze at its very first window.
        // While paused, the user's own fixed range (already applied via
        // `renderer.setViewport()` when they zoomed/panned) must not be
        // fought over on every incoming frame. Bounds come from the traces
        // just built, not the header (see `tracesTimeBounds`'s doc comment):
        // a digital-runs frame's `item_count` counts runs, not samples.
        if (this.liveViewState.following) {
          const bounds = tracesTimeBounds(traces);
          if (bounds) renderer.setViewport(bounds.t0, bounds.t1);
        }
      }),
    );
    // The plot follows the app's theme (SPEC §11.1): CSS handles every other
    // element via custom properties, but a renderer draws into its own
    // canvas/SVG with inline colors, so it needs an explicit re-apply on
    // every switch — including the OS preference changing under a "system"
    // choice, which `ThemeController` also reports here.
    this.unsubscribers.push(this.ctx.theme.changes.on(() => this.applyPlotTheme()));
    this.plotResizeObserver = new ResizeObserver(() => {
      if (this.mode === "live" && this.liveViewId !== null) this.viewportDebouncer.call(undefined);
    });
    this.plotResizeObserver.observe(this.plotEl);
    if (this.liveRecId !== null) this.startLiveListPolling();
    this.mountRenderer();
    // `decode.list` is static (no server state to go stale), but the very
    // first `activate()` can race a socket that is still `CONNECTING` — a
    // `send()` right then throws synchronously and this fetch would
    // otherwise fail silently forever (mirrors `modules/terminal/index.ts`'s
    // `refreshSources`-on-reconnect pattern for exactly the same reason).
    this.unsubscribers.push(
      this.ctx.ws.connectionState.on((state) => {
        if (state === "connected") {
          this.refreshDecoders();
          void this.refreshLiveList();
        }
      }),
    );
    this.refreshDecoders();
    // Recordings too, on load and on every reconnect: a reloaded page used to
    // know nothing of a recording it had not started until Start was pressed
    // in Live mode, so an orphaned one held the device with Stop disabled
    // (B-31). Listed, `liveRecordingOnDevice` sees it in either mode.
    void this.refreshLiveList();
  }

  private refreshDecoders(): void {
    this.ctx.ws.send("decode.list", {}).then(
      (result) => {
        this.decoders = result.decoders;
        this.renderDecodeDecoderOptions();
      },
      () => {
        // Surfaced centrally via ws.errors; the next reconnect (or the
        // periodic retry above) tries again — the Decode section just stays
        // empty until then.
      },
    );
  }

  override deactivate(): void {
    for (const unsubscribe of this.unsubscribers.splice(0)) unsubscribe();
    this.viewportUnsubscribe?.();
    this.viewportUnsubscribe = null;
    this.renderer?.destroy();
    this.renderer = null;
    this.plotResizeObserver?.disconnect();
    this.plotResizeObserver = null;
    this.viewportDebouncer.cancel();
    this.decodeDebouncer.cancel();
    this.stopLiveListPolling();
    this.cancelStreamDraw();
  }

  // ---------------------------------------------------------------------
  // Device selection & capability-driven channel controls (SPEC §5.1, §5.2)
  // ---------------------------------------------------------------------

  private renderDeviceOptions(): void {
    const previous = this.selectedDeviceId;
    this.deviceSelectEl.replaceChildren();
    for (const device of this.devices) {
      const option = document.createElement("option");
      option.value = device.id;
      // Labelled by bus-path location (SPEC §5.1), never by model name.
      option.textContent = deviceDisplayLabel(device);
      // Not read by this module itself — a plain DOM signal of what kind of
      // channels the device offers, useful to anything driving the picker
      // from outside (e2e among them) without re-deriving it from
      // `capabilities()` a second time.
      const controls = buildControlSchema(device.capabilities).channels;
      option.dataset.capability = controls.some(isAnalogControl)
        ? controls.some(isDigitalControl)
          ? "mixed"
          : "analog"
        : controls.some(isDigitalControl)
          ? "digital"
          : "none";
      this.deviceSelectEl.appendChild(option);
    }
    const selection = chooseSelection(this.devices, previous, this.selectedDeviceInfo);
    if (selection.departed && selection.info) {
      const option = document.createElement("option");
      option.value = selection.info.id;
      option.textContent = `${deviceDisplayLabel(selection.info)} — not connected`;
      option.dataset.disconnected = "true";
      this.deviceSelectEl.appendChild(option);
    }
    this.selectedDeviceId = selection.id;
    this.selectedDeviceInfo = selection.info;
    if (this.selectedDeviceId) this.deviceSelectEl.value = this.selectedDeviceId;
    // Only rebuild the controls when there is something new to build them
    // from. Every device-state update used to land here — and a Start causes
    // one — so the rate, ranges, offsets and trigger channel were reset to
    // defaults between the first capture and every later one (B-65).
    const fingerprint = controlsFingerprint(this.selectedDeviceId, this.selectedDeviceInfo?.capabilities);
    if (fingerprint !== this.controlsBuiltFrom) {
      this.controlsBuiltFrom = fingerprint;
      this.onDeviceChanged();
    }
    this.renderRunStopButtons();
    this.renderPlotHint();
  }

  /** The selected device is known but not on the bus right now. */
  private selectedDeviceGone(): boolean {
    return this.selectedDeviceId !== null && !this.devices.some((d) => d.id === this.selectedDeviceId);
  }

  private onDeviceChanged(): void {
    const device = this.selectedDeviceInfo;
    const note = device ? collisionNote(device.vid, device.pid) : null;
    this.deviceNoteEl.title = note ?? "";
    this.deviceNoteEl.classList.toggle("invisible", note === null);
    this.deviceNoteEl.setAttribute("aria-hidden", String(note === null));
    if (note !== null) this.deviceNoteEl.setAttribute("aria-label", note);
    this.schema = buildControlSchema(device?.capabilities ?? []);
    this.channelState = this.schema.channels.map(
      (control): ChannelUiState => ({
        control,
        enabled: true,
        rangeV: isAnalogControl(control) ? (control.rangesV[0] ?? 5) : 5,
        offsetV: 0,
      }),
    );
    this.renderChannelControls();
    this.renderRateOptions();
    this.renderTriggerChannelOptions();
    this.mountRenderer();
    this.resetDecodeResults();
    this.renderDecodeRoles();
    this.refreshImageControls();
  }

  private renderChannelControls(): void {
    this.channelsEl.replaceChildren();
    if (this.channelState.length === 0) {
      const empty = document.createElement("p");
      empty.className = HINT;
      empty.textContent = "No analog or digital capability on this device.";
      this.channelsEl.appendChild(empty);
      return;
    }
    for (const state of this.channelState) {
      const row = document.createElement("div");
      row.className = "flex flex-wrap items-center gap-3 py-0.5";

      const enable = document.createElement("input");
      enable.type = "checkbox";
      enable.checked = state.enabled;
      enable.addEventListener("change", () => {
        state.enabled = enable.checked;
        this.applyLayout();
      });

      const label = document.createElement("span");
      label.className = "min-w-12 font-semibold";
      label.textContent = state.control.label;

      row.append(enable, label);

      if (isAnalogControl(state.control)) {
        const rangeSelect = document.createElement("select");
        rangeSelect.className = SELECT;
        for (const rangeV of state.control.rangesV) {
          const option = document.createElement("option");
          option.value = String(rangeV);
          option.textContent = `${rangeV} V`;
          rangeSelect.appendChild(option);
        }
        rangeSelect.value = String(state.rangeV);
        rangeSelect.addEventListener("change", () => {
          state.rangeV = Number(rangeSelect.value);
          this.applyLayout();
        });

        const offsetInput = document.createElement("input");
        offsetInput.type = "number";
        offsetInput.step = "any";
        offsetInput.className = NUMBER_INPUT;
        offsetInput.value = String(state.offsetV);
        // `input`, not `change`: a number field fires `change` only when it
        // loses focus or takes Enter, and a lane that moves as the number is
        // typed is the whole point of the control.
        offsetInput.addEventListener("input", () => {
          state.offsetV = Number(offsetInput.value) || 0;
          this.applyLayout();
        });

        const rangeLabel = document.createElement("label");
        rangeLabel.className = LABEL;
        rangeLabel.append("Range ", rangeSelect);
        const offsetLabel = document.createElement("label");
        offsetLabel.className = LABEL;
        offsetLabel.append("Offset (V) ", offsetInput);
        row.append(rangeLabel, offsetLabel);

        // Single mode only: measuring a live-recorded channel needs a
        // snapshot first (the "Snapshot" side-panel section), since there is
        // no one finite capture handle for a still-growing recording.
        if (this.mode === "single")
          row.append(
            this.buildMeasureControl(
              state.control.channel,
              state.control.label,
              "analog",
              () => this.acqState.captureHandle,
              () => this.effective?.calibration.find((c) => c.channel === state.control.channel)?.scale,
            ),
          );
      } else if (this.mode === "single") {
        row.append(
          this.buildMeasureControl(state.control.channel, state.control.label, "digital", () => this.acqState.captureHandle),
        );
      }

      this.channelsEl.appendChild(row);
    }
  }

  private renderRateOptions(): void {
    this.rateSelectEl.replaceChildren();
    for (const rateHz of this.schema.sampleRatesHz) {
      const option = document.createElement("option");
      option.value = String(rateHz);
      // The digital rate ladder spans 20 kS/s..24 MS/s; always
      // labelling in MS/s would show "0.02 MS/s" for the low end.
      option.textContent = formatSampleRate(rateHz);
      this.rateSelectEl.appendChild(option);
    }
    // Default to the fastest available rate, not whichever the ladder
    // happens to list first (a plain <select> otherwise defaults to its
    // first <option> — Analog's own `rates_hz` already lists fastest-first,
    // but the Digital ladder lists slowest-first, and every
    // existing capability effectively defaulted to its ceiling when it had
    // only one rate to offer).
    if (this.schema.sampleRatesHz.length > 0) {
      this.rateSelectEl.value = String(Math.max(...this.schema.sampleRatesHz));
    }
  }

  private renderTriggerChannelOptions(): void {
    this.triggerChannelEl.replaceChildren();
    for (const control of this.schema.channels) {
      const option = document.createElement("option");
      option.value = String(control.channel);
      option.textContent = control.label;
      this.triggerChannelEl.appendChild(option);
    }
  }

  private currentRateHz(): number {
    return Number(this.rateSelectEl.value) || this.schema.sampleRatesHz[0] || 0;
  }

  private currentSamples(): number {
    return Number(this.samplesInputEl.value) || 1024;
  }

  private currentFollowWindowSeconds(): number {
    return Number(this.liveWindowInputEl.value) || 0.02;
  }

  // ---------------------------------------------------------------------
  // Mode switch (SPEC §5.5) and popovers (SPEC §11.3)
  // ---------------------------------------------------------------------

  private setMode(mode: GraphMode): void {
    if (this.mode === mode) return;
    this.mode = mode;
    this.modeSingleEl.className = `${BUTTON_BASE} ${mode === "single" ? BUTTON_ACTIVE : BUTTON_NEUTRAL}`;
    this.modeLiveEl.className = `${BUTTON_BASE} ${mode === "live" ? BUTTON_ACTIVE : BUTTON_NEUTRAL}`;
    // Trigger only applies to a finite, triggered Single capture (SPEC §5.5's
    // table). Disabled rather than hidden, like Follow and Snapshot, so the
    // toolbar keeps its shape across modes.
    this.triggerPopoverBtn.disabled = mode === "live";
    this.triggerPopoverBtn.title = mode === "live" ? "Trigger applies to Single captures" : "";
    if (mode === "live") this.triggerPopover?.close();
    this.samplesRowEl.hidden = mode === "live";
    this.liveWindowRowEl.hidden = mode !== "live";
    this.snapshotSectionEl.hidden = mode !== "live";
    // "not recording" means nothing in Single mode.
    if (mode === "single") this.recStatusEl.textContent = "";
    this.renderTriggerMarker();
    this.renderTimeLimits();
    this.searchSectionEl.hidden = mode !== "live";
    this.renderChannelControls(); // Measure buttons only show in Single mode
    this.refreshImageControls();
    this.renderRunStopButtons();
    this.renderLiveStatus();
    this.renderLiveButtons();
    this.renderMeasurements(); // which capture is the current one changed
    this.mountRenderer();
    this.resetDecodeResults();
    this.renderDecodeRoles();
  }

  /** Wires one `Popover` per settings button — closes on Escape/outside-click, never affects layout (each panel is `absolute`, SPEC §11.3). */
  private wirePopovers(): void {
    const wire = (anchor: HTMLButtonElement, panel: HTMLElement): Popover => {
      const popover = new Popover(anchor, panel, { onChange: (open) => (panel.hidden = !open) });
      anchor.addEventListener("click", () => popover.toggle());
      return popover;
    };
    this.triggerPopover = wire(this.triggerPopoverBtn, this.triggerPopoverPanel);
    this.popovers = [
      wire(this.channelsPopoverBtn, this.channelsPopoverPanel),
      this.triggerPopover,
      wire(this.acqPopoverBtn, this.acqPopoverPanel),
      wire(this.displayPopoverBtn, this.displayPopoverPanel),
    ];
  }

  // ---------------------------------------------------------------------
  // Renderer abstraction (SPEC §5.3) — switchable at runtime
  // ---------------------------------------------------------------------

  private renderRendererOptions(): void {
    this.rendererSelectEl.replaceChildren();
    for (const id of listRenderers()) {
      const option = document.createElement("option");
      option.value = id;
      option.textContent = id;
      this.rendererSelectEl.appendChild(option);
    }
    this.rendererSelectEl.value = this.rendererId;
  }

  /** The panel layout for whichever mode is active (SPEC §5.5: Live uses the recording's own channel list once one exists). */
  private currentLayout(): { panels: ReturnType<typeof buildLayout>["panels"] } {
    const analogSettings = (channel: number): AnalogChannelSettings => {
      const state = this.channelState.find((c) => c.control.channel === channel && isAnalogControl(c.control));
      return { rangeV: state?.rangeV ?? 5, offsetV: state?.offsetV ?? 0 };
    };
    if (this.mode === "live" && this.liveChannels.length > 0) {
      return buildLayout(this.liveChannels.map(controlFromRecChannel), analogSettings);
    }
    const enabled = this.channelState.filter((c) => c.enabled).map((c) => c.control);
    return buildLayout(enabled, analogSettings);
  }

  private updateTriggerHint(): void {
    if (this.mode === "single") {
      const samples = this.currentSamples();
      this.triggerHintEl.textContent = `Trigger lands at the capture midpoint (sample ${triggerSampleIndex(samples)} of ${samples}).`;
    } else {
      this.triggerHintEl.textContent = "";
    }
  }

  /**
   * A range, offset or channel-enable change: the panel layout moved, but the
   * plot is already mounted and its data has not changed.
   *
   * This used to re-`mount()`, which tears the canvases down and takes a fresh
   * WebGL context for every keystroke in an Offset field — against a browser
   * limit of a handful of live contexts — and is why those fields appeared to
   * do nothing (B-55). `setLayout` redraws the lanes in place.
   */
  private applyLayout(): void {
    this.updateTriggerHint();
    const renderer = this.renderer;
    if (!renderer || renderer.id !== this.rendererId) {
      this.mountRenderer();
      return;
    }
    renderer.setLayout(this.currentLayout());
  }

  private mountRenderer(): void {
    this.updateTriggerHint();

    let renderer: WaveformRenderer | null = this.renderer;
    if (this.rendererId !== renderer?.id) {
      renderer = this.replaceRenderer(this.rendererId);
    }
    if (!renderer) throw new Error("graph module: renderer was not constructed");
    const layout = this.currentLayout();
    try {
      renderer.mount(this.plotEl, layout);
      this.rendererNoticeEl.textContent = "";
      this.applyPlotTheme();
      this.renderTriggerMarker();
      this.renderTimeLimits();
      if (this.mode === "live") this.primeLiveViewport(renderer);
    } catch (err) {
      if (err instanceof RendererNotImplementedError) {
        this.rendererNoticeEl.textContent = `${err.rendererId} is not implemented yet — pick another renderer.`;
      } else if (err instanceof RendererUnavailableError) {
        // SPEC §5.3 task instructions: never leave a blank panel — fall back
        // to Plotly (always available; it's an SVG renderer) and say why.
        this.rendererId = "plotly";
        renderer = this.replaceRenderer(this.rendererId);
        renderer.mount(this.plotEl, layout);
        this.rendererNoticeEl.textContent = `${err.rendererId} is unavailable in this browser — showing Plotly instead.`;
        this.applyPlotTheme();
        if (this.mode === "live") this.primeLiveViewport(renderer);
        this.renderRendererOptions(); // reflect the forced fallback in the picker
      } else {
        throw err;
      }
    }
  }

  /**
   * Gives the renderer a non-null viewport before any Live-mode view frame
   * arrives. Every backend auto-fits its viewport on the very first
   * `setTraces()` call it ever receives after `mount()` (SPEC §5.3's `Trace`
   * doc comment: "the graph module has no other way to learn the range this
   * renderer picked for the very first frame") and reports it through
   * `onViewportChange` — indistinguishable, from this module's side, from
   * genuine user interaction. Live mode treats *any* such notification as
   * "the user paused following" (SPEC §5.5, `onRendererViewportChange`), so
   * without this, following would end itself after exactly one frame. The
   * view-frame handler immediately re-fits this placeholder to the real
   * data on the very next frame — following or not — so it is never visible
   * for more than one frame.
   */
  private primeLiveViewport(renderer: WaveformRenderer): void {
    renderer.setViewport(0, followWindowSeconds(this.currentFollowWindowSeconds()));
  }

  /** Destroys the current renderer (if any) and constructs+subscribes a fresh one for `id`, without mounting it. */
  private replaceRenderer(id: RendererId): WaveformRenderer {
    this.viewportUnsubscribe?.();
    this.renderer?.destroy();
    const renderer = createRenderer(id);
    this.viewportUnsubscribe = renderer.onViewportChange((t0, t1) => this.onRendererViewportChange(t0, t1));
    this.renderer = renderer;
    return renderer;
  }

  /**
   * A renderer reports a viewport change only for interaction the user did
   * directly on the plot (SPEC §5.3's `onViewportChange` doc comment) —
   * wheel-zoom, drag-pan, double-click-fit. In Live mode this is exactly
   * what "pauses following" (SPEC §5.5); in Single mode it just keeps the
   * view-range inputs truthful, as before M15.
   */
  private onRendererViewportChange(t0: number, t1: number): void {
    this.viewStartEl.value = String(t0);
    this.viewEndEl.value = String(t1);
    if (this.mode === "single") this.streamViewFollows = false;
    if (this.mode === "live" && this.liveViewId !== null) {
      this.liveViewState = pauseAtViewport(t0, t1);
      this.renderLiveStatus();
      this.viewportDebouncer.call(undefined);
      // SPEC T21: "Live mode re-decodes the visible range when the view
      // changes (debounced)" — only once a decode has actually been applied
      // once (`decodeActive`), never speculatively on every pan/zoom.
      if (this.decodeActive) this.decodeDebouncer.call(undefined);
    }
  }

  /** Re-reads AWIC's current theme colors off the plot's own element and applies them (SPEC §11.1). */
  private applyPlotTheme(): void {
    this.renderer?.setTheme(readThemeColors(this.plotEl));
  }

  private plotPixelWidth(): number {
    return Math.max(1, Math.round(this.plotEl.getBoundingClientRect().width));
  }

  // ---------------------------------------------------------------------
  // Single-mode acquisition (SPEC §3.1) — server authoritative (§11)
  // ---------------------------------------------------------------------

  private async onStart(): Promise<void> {
    const device = this.selectedDeviceId;
    if (!device) return;
    const enabled = this.channelState.filter((c) => c.enabled);
    const analog = enabled.every((c) => isAnalogControl(c.control)) && enabled.length > 0;
    const cfg = buildAcqConfig({
      rateHz: this.currentRateHz(),
      samples: this.currentSamples(),
      enabledChannels: enabled.map((c) => c.control.channel),
      trigger: this.triggerForm,
      rangesV: analog ? enabled.map((c) => c.rangeV) : undefined,
    });
    try {
      const configured = await this.ctx.ws.send("acq.configure", { device, cfg });
      this.effective = { cfg: configured.cfg, calibration: configured.calibration };
      // Stream ids restart at 1 with the server, so a capture after a restart
      // can carry the id of the one on screen: forget it, or the new
      // capture's frames would be joined onto the old one's.
      this.cancelStreamDraw();
      this.streamTracesFor = null;
      const started = await this.ctx.ws.send("acq.start", { device });
      this.captureInfos.set(started.capture, {
        capture: started.capture,
        takenAtMs: Date.now(),
        rateHz: configured.cfg.rate_hz,
        samples: configured.cfg.samples,
        source: "single",
      });
      this.acqState = applyStreamStarted(this.acqState, started);
      this.renderAcqStatus();
      this.resetDecodeResults(); // a fresh capture invalidates any previously decoded annotations
      this.refreshImageControls();
    } catch {
      // Surfaced centrally via ws.errors -> the shell's error banner; nothing more to do here.
    }
  }

  private async onStop(): Promise<void> {
    const stream = this.acqState.streamId;
    if (stream === null) return;
    try {
      await this.ctx.ws.send("acq.stop", { stream });
      this.acqState = applyStopped(this.acqState);
      this.renderAcqStatus();
    } catch {
      // Surfaced centrally via ws.errors.
    }
  }

  /** The driver's effective configuration, never the request: the hardware may run a different rate than was asked for. */
  private streamRateHz(): number {
    return this.effective?.cfg.rate_hz ?? this.currentRateHz();
  }

  /** How long the configured capture lasts, seconds. */
  private streamTimebaseSeconds(): number {
    const rateHz = this.streamRateHz();
    return rateHz > 0 ? (this.effective?.cfg.samples ?? this.currentSamples()) / rateHz : 0;
  }

  private streamTraceContext(frameSpanSeconds: number | undefined): TraceContext {
    const enabled = this.channelState.filter((c) => c.enabled);
    return {
      timebase: {
        rateHz: this.streamRateHz(),
        totalSamples: this.effective?.cfg.samples ?? this.currentSamples(),
        frameSpanSeconds,
      },
      calibrationForChannel: (channel) =>
        this.effective?.calibration.find((c) => c.channel === channel) ?? identityCalibration(channel),
      labelForChannel: (_kind, channel) => enabled.find((c) => c.control.channel === channel)?.control.label ?? `CH${channel}`,
      laneForChannel: (channel) => enabled.findIndex((c) => c.control.channel === channel),
      digitalChannels: enabled.filter((c) => isDigitalControl(c.control)).map((c) => c.control.channel),
    };
  }

  /** Adds placed frames to the capture on screen and draws it - at once the first time, then one redraw per 100 ms. */
  private addStreamFrames(placed: readonly PlacedFrame[]): void {
    const renderer = this.renderer;
    const streamId = this.streamTracesFor;
    if (placed.length === 0 || !renderer || streamId === null || this.mode !== "single") return;
    for (const { frame, spanSeconds } of placed) this.streamTraces.add(frameToTraces(frame, this.streamTraceContext(spanSeconds)));
    if (!this.streamDrawn) {
      this.streamDrawn = true;
      const traces = this.streamTraces.snapshot();
      renderer.setTraces({ traces });
      // A renderer's first-ever `setTraces` fits itself and reports that as a
      // viewport change, read as the user zooming - on a fresh page the view
      // then stayed on the first 8 ms frame of a 1.28 s capture. Nothing the
      // user did yet: keep widening (`primeLiveViewport` is Live's answer).
      this.streamViewFollows = true;
      // And it fits only that once: after Live, or any earlier capture, the
      // view stayed where it was and a capture that arrived in one frame was
      // drawn off screen until a double-click. Fit the new capture here.
      const bounds = tracesTimeBounds(traces);
      if (bounds) renderer.setViewport(bounds.t0, bounds.t1);
      this.captureTrigger = this.triggerMarkerForCapture();
      this.renderTriggerMarker();
      this.markPlotHasData();
      return;
    }
    // Redrawing everything received so far on every frame is quadratic in
    // a slow capture's hundreds of small frames; one redraw per 100 ms
    // keeps the picture current, and the last one shows the whole capture.
    this.streamDrawTimer ??= setTimeout(() => this.drawStreamTraces(streamId), 100);
  }

  private cancelStreamDraw(): void {
    if (this.streamDrawTimer !== null) clearTimeout(this.streamDrawTimer);
    this.streamDrawTimer = null;
  }

  /**
   * Where the capture now arriving was triggered: the effective trigger,
   * placed at the capture's midpoint sample (`triggerSampleIndex`), on the
   * plot's time axis. An analog channel also gets its level; a digital one
   * has none to draw.
   */
  private triggerMarkerForCapture(): TriggerMarker | null {
    const cfg = this.effective?.cfg;
    const trigger = cfg?.trigger;
    const rateHz = this.streamRateHz();
    if (!cfg || !trigger || !(rateHz > 0)) return null;
    const control = this.channelState.find((c) => c.control.channel === trigger.channel)?.control;
    return {
      t: this.captureT0Seconds + triggerSampleIndex(cfg.samples) / rateHz,
      channel: trigger.channel,
      levelV: control && isAnalogControl(control) ? (trigger.level ?? null) : null,
    };
  }

  /** The trigger marker belongs to a Single capture; Live has none. */
  private renderTriggerMarker(): void {
    this.renderer?.setTrigger(this.mode === "single" ? this.captureTrigger : null);
  }

  /**
   * How far the user may zoom out and pan. Single: the capture itself (the
   * renderer's default). Live: the recording's retained span, so panning
   * into history beyond the loaded window still works.
   */
  private renderTimeLimits(): void {
    const renderer = this.renderer;
    if (!renderer) return;
    const summary =
      this.mode === "live" && this.liveRecId !== null
        ? this.liveRecordings.find((r) => r.rec === this.liveRecId)
        : undefined;
    if (!summary || !(summary.rate_hz > 0)) {
      renderer.setTimeLimits(null);
      return;
    }
    renderer.setTimeLimits({ t0: summary.first_sample / summary.rate_hz, t1: headSeconds(summary) });
  }

  private markPlotHasData(): void {
    if (this.plotHasData) return;
    this.plotHasData = true;
    this.renderPlotHint();
  }

  /** A line in the empty plot saying what to do, until the first data arrives. */
  private renderPlotHint(): void {
    this.plotHintEl.hidden = this.plotHasData;
    this.plotHintEl.textContent =
      this.devices.length === 0 && this.selectedDeviceId === null
        ? "No instrument found yet. Connect one; it appears under Device as soon as it is on the bus."
        : "Pick a device and press Start. In Single mode one capture is taken; Live records continuously.";
  }

  /** Draws the capture received so far for `streamId`, if it is still the one on screen. */
  private drawStreamTraces(streamId: number): void {
    this.streamDrawTimer = null;
    const renderer = this.renderer;
    if (this.mode !== "single" || this.streamTracesFor !== streamId || !renderer) return;
    const traces = this.streamTraces.snapshot();
    renderer.setTraces({ traces });
    // A renderer fits its viewport to the first frame only (SPEC §5.3), so
    // the rest of the capture would land outside it: widen it as it grows -
    // unless the user has zoomed or panned meanwhile, which a redraw every
    // 100 ms would otherwise undo.
    if (!this.streamViewFollows) return;
    const bounds = tracesTimeBounds(traces);
    if (bounds) renderer.setViewport(bounds.t0, bounds.t1);
  }

  /**
   * What holds the selected device, in whichever mode is on screen (B-66), and
   * this client's own pending start or stop (B-67).
   */
  /**
   * The recording that matters for the selected device: the one this tab is
   * watching, else any live one on that device — an agent's or another tab's
   * holds it just the same (SPEC §2.8).
   */
  private liveRecordingOnDevice(): RecordingSummary | null {
    const device = this.selectedDeviceId;
    if (device === null) return null;
    return (
      this.liveRecordings.find((r) => r.rec === this.liveRecId) ??
      this.liveRecordings.find((r) => r.device === device && r.state === "recording") ??
      null
    );
  }

  private currentActivity(): DeviceActivity {
    const recording = this.liveRecordingOnDevice();
    return deviceActivity({
      mode: this.mode,
      acq: this.acqState,
      phase: this.phase,
      recording,
      watching: recording !== null && recording.rec === this.liveRecId,
    });
  }

  private renderAcqStatus(): void {
    const activity = this.currentActivity();
    const line = activityLine(activity);
    this.acqStatusEl.textContent = line;
    this.acqStatusEl.title = line;
    this.acqStatusEl.dataset.state = activity.kind;
    this.acqStatusEl.className = `truncate text-sm font-medium ${ACQ_STATUS_CLASS[activity.kind] ?? "text-awic-fg-muted"}`;
    this.renderRunStopButtons();
    // A new capture makes the results on screen history: re-mark them.
    if (this.currentImageCapture() !== this.measurementsCurrentCapture) this.renderMeasurements();
  }

  private renderGapBanner(streams: Readonly<Record<number, { readonly droppedTotal: number }>>): void {
    const streamId = this.acqState.streamId;
    const dropped = streamId !== null ? (streams[streamId]?.droppedTotal ?? 0) : 0;
    this.gapBannerEl.textContent = dropped > 0 ? `${dropped} frame(s) dropped on this stream — view may have gaps.` : "";
  }

  // ---------------------------------------------------------------------
  // Live mode (SPEC §5.5): rec.start/attach, follow view, pause/resume,
  // snapshot, pattern search/wait.
  // ---------------------------------------------------------------------

  private renderRunStopButtons(): void {
    const activity = this.currentActivity();
    this.startButtonEl.disabled = !activity.canStart || this.selectedDeviceGone();
    this.stopButtonEl.disabled = !activity.canStop;
  }

  private async onStartClick(): Promise<void> {
    // Before anything is sent: a second click in the window before the server
    // answers used to send a second start (B-67).
    if (this.phase !== "none") return;
    this.phase = "starting";
    this.cancelPendingStart = false;
    this.renderAcqStatus();
    try {
      if (this.mode === "single") await this.onStart();
      else await this.onStartLive();
    } finally {
      this.phase = "none";
      this.renderAcqStatus();
    }
    // Stop was pressed while the start was in flight; now that the reply has
    // named what it created, undo it.
    if (this.cancelPendingStart) {
      this.cancelPendingStart = false;
      await this.onStopClick();
    }
  }

  private async onStopClick(): Promise<void> {
    // A Stop during an unanswered start used to do nothing at all while the
    // run carried on (B-67): remember it instead.
    if (this.phase === "starting") {
      this.cancelPendingStart = true;
      return;
    }
    if (this.phase !== "none") return;
    // Decided before the phase changes: `deviceActivity` reports a pending
    // phase ahead of everything else, so routing on it after setting
    // "stopping" read "stopping" and sent the Stop to the mode default -
    // nowhere, with a recording still running (see `stopTarget`).
    const target = stopTarget(
      this.liveRecordingOnDevice()?.state === "recording",
      this.acqState.streamId !== null,
    );
    this.phase = "stopping";
    this.renderAcqStatus();
    try {
      // Routed by what actually holds the device, not by which mode is on
      // screen: a Stop that reaches the wrong path is an enabled button that
      // does nothing, which is what B-67 is about.
      switch (target) {
        case "recording":
          await this.onStopLive();
          break;
        case "stream":
          await this.onStop();
          break;
        default:
          if (this.mode === "live") await this.onStopLive();
          else await this.onStop();
      }
    } finally {
      this.phase = "none";
      this.renderAcqStatus();
    }
  }

  /**
   * Starts (or attaches to) a recording for the selected device (SPEC
   * §5.5, §2.8): "if `rec.list` already shows a recording for that device,
   * attach to it instead of starting another — recordings are server-owned
   * and shared with agents."
   */
  private async onStartLive(): Promise<void> {
    const device = this.selectedDeviceId;
    if (!device) return;
    try {
      await this.dropEndedLiveRecording();
      const list = await this.ctx.ws.send("rec.list", {});
      this.applyRecList(list);
      const existing = pickRecordingToAttach(list.recordings, device);

      let recId: number;
      let rateHz: number;
      let channels: readonly RecChannel[];
      if (existing) {
        recId = existing.rec;
        rateHz = existing.rate_hz;
        channels = existing.channels;
      } else {
        const enabled = this.channelState.filter((c) => c.enabled);
        const analog = enabled.every((c) => isAnalogControl(c.control)) && enabled.length > 0;
        const cfg = buildAcqConfig({
          rateHz: this.currentRateHz(),
          samples: 0,
          enabledChannels: enabled.map((c) => c.control.channel),
          trigger: { enabled: false, channel: 0, slope: "rising", level: 0 },
          rangesV: analog ? enabled.map((c) => c.rangeV) : undefined,
        });
        const started = await this.ctx.ws.send("rec.start", { device, cfg });
        recId = started.rec;
        rateHz = started.cfg.rate_hz;
        const freshList = await this.ctx.ws.send("rec.list", {});
        this.applyRecList(freshList);
        channels = freshList.recordings.find((r) => r.rec === recId)?.channels ?? [];
      }

      this.liveRecId = recId;
      // `pickRecordingToAttach` is an `Array.find`, so "nothing to attach to" is
      // `undefined`, not `null`: `existing === null` was false either way and
      // the tab never treated a recording as its own, so B-68's cleanup never
      // ran. Found by tracing the client's own ws traffic during the start/stop
      // run - `rec.stop` went out and `rec.delete` never did.
      this.liveRecStartedHere = startedItsOwnRecording(existing);
      this.liveRateHz = rateHz;
      this.liveChannels = channels;
      this.liveViewState = initialLiveViewState();
      this.liveSnapshotCapture = null;
      this.renderSnapshotSection();
      this.resetDecodeResults();
      this.renderDecodeRoles();
      this.mountRenderer();

      const view = await this.ctx.ws.send("rec.view", {
        rec: recId,
        follow_s: followWindowSeconds(this.currentFollowWindowSeconds()),
        columns: columnsForWidth(this.plotPixelWidth()),
        fps: 30,
      });
      this.liveViewId = view.view;
      this.startLiveListPolling();
      this.renderRunStopButtons();
      this.renderLiveStatus();
    } catch {
      // Surfaced centrally via ws.errors.
    }
  }

  /**
   * The previous Live run's recording, when it ended on its own (its device
   * left the bus, or stalled): no Stop is offered then, so `onStopLive`'s
   * B-68 cleanup never ran, and every power cycle under Live left one
   * recording behind in `error`. Starting again is the user moving on, so the
   * same rule applies here — started by this tab, nothing taken from it.
   */
  private async dropEndedLiveRecording(): Promise<void> {
    const rec = this.liveRecId;
    if (rec === null || !this.liveRecStartedHere || this.liveSnapshotCapture !== null) return;
    const fresh = await this.ctx.ws.send("rec.list", {});
    if (fresh.recordings.some((r) => r.rec === rec && r.state === "recording")) return;
    this.stopLiveListPolling();
    if (this.liveViewId !== null) {
      const view = this.liveViewId;
      this.liveViewId = null;
      try {
        await this.ctx.ws.send("rec.view.close", { view });
      } catch {
        /* the view went with its recording */
      }
    }
    try {
      await this.ctx.ws.send("rec.delete", { rec });
    } catch {
      // Already gone, or held by something else: leave it listed.
    }
    this.liveRecStartedHere = false;
    this.liveRecId = null;
  }

  private async onStopLive(): Promise<void> {
    const rec = this.liveRecId ?? this.liveRecordingOnDevice()?.rec ?? null;
    const view = this.liveViewId;
    this.stopLiveListPolling();
    this.liveViewId = null;
    if (view !== null) {
      try {
        await this.ctx.ws.send("rec.view.close", { view });
      } catch {
        /* the connection may already be gone; nothing more to clean up client-side */
      }
    }
    if (rec !== null) {
      try {
        await this.ctx.ws.send("rec.stop", { rec });
        // A Live run used to leave its recording behind for ever: 49 of them,
        // 32 MB of the 256 MB budget, after ~70 start/stops from this tab
        // (B-68). The tab deletes the one it started itself, and only when
        // nothing was taken from it - a snapshot is a separate capture, but
        // having taken one means the recording was worth keeping. A recording
        // this tab merely attached to belongs to whoever started it.
        if (this.liveRecStartedHere && this.liveSnapshotCapture === null) {
          try {
            await this.ctx.ws.send("rec.delete", { rec });
          } catch {
            // Already gone, or held by something else: leave it listed.
          }
        }
      } catch {
        // Surfaced via ws.errors.
      }
    }
    this.liveRecStartedHere = false;
    this.liveRecId = null;
    this.renderRunStopButtons();
    this.renderLiveStatus();
  }

  private startLiveListPolling(): void {
    this.stopLiveListPolling();
    this.liveListTimer = setInterval(() => void this.refreshLiveList(), 1000);
  }

  private stopLiveListPolling(): void {
    if (this.liveListTimer !== null) {
      clearInterval(this.liveListTimer);
      this.liveListTimer = null;
    }
  }

  private async refreshLiveList(): Promise<void> {
    try {
      const list = await this.ctx.ws.send("rec.list", {});
      this.applyRecList(list);
      // Single mode too: `renderLiveStatus` stops at Live mode, and the
      // device's activity line and Stop button read the same list (B-66).
      this.renderAcqStatus();
      this.renderLiveStatus();
      this.renderTimeLimits();
    } catch {
      // A transient failure just skips this refresh tick; the next one retries.
    }
  }

  private applyRecList(list: RecListResult): void {
    this.liveList = list;
    this.liveRecordings = list.recordings;
  }

  /** Toolbar status text (SPEC §5.5: "the toolbar shows the retained span and the memory in use") plus the Follow button's visibility. */
  private renderLiveStatus(): void {
    if (this.mode !== "live") return;
    const summary = this.liveRecId !== null ? this.liveRecordings.find((r) => r.rec === this.liveRecId) : undefined;
    this.recStatusEl.textContent =
      summary && this.liveList ? formatRecordingStatus(summary, this.liveList) : NO_RECORDING_STATUS;
    this.recStatusEl.title = this.recStatusEl.textContent;
    this.renderAcqStatus();
    this.renderLiveButtons();
  }

  /** Follow and Snapshot are always on the toolbar; whether they can be used is their disabled state, and why not is their tooltip. */
  private renderLiveButtons(): void {
    const live = this.mode === "live";
    const watching = live && this.liveViewId !== null;
    this.followButtonEl.disabled = !watching || this.liveViewState.following;
    this.followButtonEl.title = !live
      ? "Live mode only"
      : !watching
        ? "Start a Live recording first"
        : this.liveViewState.following
          ? "Already following the newest data"
          : "Go back to following the newest data";
    this.snapshotButtonEl.disabled = !watching;
    this.snapshotButtonEl.title = !live
      ? "Live mode only"
      : !watching
        ? "Start a Live recording first"
        : "Keep the visible range as a capture to measure or export";
  }

  private onFollowClick(): void {
    if (this.liveViewId === null) return;
    this.viewportDebouncer.cancel();
    this.liveViewState = resumeFollowing();
    this.renderLiveStatus();
    this.flushViewUpdate();
  }

  /** Sends `rec.view.update` for the current (following or fixed) state — the debounced target for pan/zoom/resize, and also called immediately for a deliberate action (Follow, a search-result click). */
  private flushViewUpdate(): void {
    const view = this.liveViewId;
    if (view === null) return;
    const columns = columnsForWidth(this.plotPixelWidth());
    if (this.liveViewState.following) {
      this.ctx.ws
        .send("rec.view.update", { view, follow_s: followWindowSeconds(this.currentFollowWindowSeconds()), columns })
        .catch(() => {});
    } else if (this.liveViewState.t0 !== null && this.liveViewState.t1 !== null) {
      this.ctx.ws
        .send("rec.view.update", { view, t0_s: this.liveViewState.t0, t1_s: this.liveViewState.t1, columns })
        .catch(() => {});
    }
  }

  /** The visible range Live mode's Snapshot should capture — exact while paused, approximated from the recording's current head while following. */
  private computeVisibleRange(): { t0: number; t1: number } | null {
    if (!this.liveViewState.following) {
      return this.liveViewState.t0 !== null && this.liveViewState.t1 !== null
        ? { t0: this.liveViewState.t0, t1: this.liveViewState.t1 }
        : null;
    }
    const summary = this.liveRecId !== null ? this.liveRecordings.find((r) => r.rec === this.liveRecId) : undefined;
    if (!summary) return null;
    const t1 = headSeconds(summary);
    const window = followWindowSeconds(this.currentFollowWindowSeconds());
    const t0 = Math.max(summary.first_sample / Math.max(summary.rate_hz, 1), t1 - window);
    return { t0, t1 };
  }

  /**
   * Opens the server-rendered view of the current capture in a new tab
   * (SPEC §5.6). A tab rather than an inline panel: the point of this button
   * is to get a picture *out* — to save, to paste into an issue, to show
   * someone — and the live traces are already on screen behind it.
   */
  private onImageClick(): void {
    const capture = this.currentImageCapture();
    const kind = this.imageKind();
    const channels = this.imageChannelsFor(kind);
    if (imageUnavailableReason(capture, kind, channels) !== null) return;
    window.open(imageViewUrl({ capture: capture as string, kind, channels }), "_blank", "noopener");
  }

  /**
   * The capture to draw: Single mode's own handle, or in Live mode the
   * snapshot the user already took — a still-growing recording has no one
   * finite handle to draw.
   */
  private currentImageCapture(): string | null {
    return this.mode === "single" ? this.acqState.captureHandle : this.liveSnapshotCapture;
  }

  /** The chosen view, validated against the options the select actually offers. */
  private imageKind(): ImageKind {
    const value = this.imageKindEl.value;
    return value === "eye" || value === "histogram" || value === "anomalies" ? value : "waveform";
  }

  /** Hardware channel numbers (D4 is 4, CH1 is 0), as the server's render takes them. */
  private imageChannelsFor(kind: ImageKind): number[] {
    if (!isSingleChannel(kind)) return [];
    const channel = Number.parseInt(this.imageChannelEl.value, 10);
    return Number.isInteger(channel) ? [channel] : [];
  }

  /**
   * Keeps the channel picker and the button's own state in step with the
   * chosen view. The reason a disabled button gives is its `title`, because a
   * control that refuses without saying why gets clicked again.
   */
  private refreshImageControls(): void {
    const kind = this.imageKind();
    this.imageChannelEl.disabled = !isSingleChannel(kind);
    const enabled = this.channelState.filter((c) => c.enabled);
    const wanted = enabled.map((c) => ({ channel: c.control.channel, label: c.control.label }));
    const current = this.imageChannelEl.value;
    const offered = Array.from(this.imageChannelEl.options, (o) => o.value).join(",");
    if (offered !== wanted.map((w) => String(w.channel)).join(",")) {
      this.imageChannelEl.replaceChildren(
        ...wanted.map(({ channel, label }) => {
          const option = document.createElement("option");
          option.value = String(channel);
          option.textContent = label;
          return option;
        }),
      );
      if (wanted.some((w) => String(w.channel) === current)) this.imageChannelEl.value = current;
    }
    const reason = imageUnavailableReason(
      this.currentImageCapture(),
      kind,
      this.imageChannelsFor(kind),
    );
    this.imageButtonEl.disabled = reason !== null;
    this.imageButtonEl.title = reason ?? `Open a ${kind} view of this capture in a new tab`;
  }

  private async onSnapshotClick(): Promise<void> {
    const rec = this.liveRecId;
    const range = this.computeVisibleRange();
    if (rec === null || !range) return;
    try {
      const result = await this.ctx.ws.send("rec.snapshot", { rec, t0_s: range.t0, t1_s: range.t1 });
      this.liveSnapshotCapture = result.capture;
      this.captureInfos.set(result.capture, {
        capture: result.capture,
        takenAtMs: Date.now(),
        rateHz: this.liveRateHz,
        samples: Math.max(0, Math.round((range.t1 - range.t0) * this.liveRateHz)),
        source: "snapshot",
      });
      this.refreshImageControls();
      this.renderMeasurements();
      this.snapshotStatusEl.textContent = `Captured ${formatSiDuration(range.t0)} – ${formatSiDuration(range.t1)}.`;
      this.renderSnapshotSection();
    } catch {
      // Surfaced via ws.errors.
    }
  }

  /** Populates the side panel's Snapshot section with a "Measure" control per live channel, reusing the same `capture.measure` UI Single mode uses (SPEC §5.5). */
  private renderSnapshotSection(): void {
    this.snapshotChannelsEl.replaceChildren();
    if (this.liveSnapshotCapture === null) {
      this.snapshotStatusEl.textContent = "Click Snapshot to capture the visible range.";
    }
    for (const channel of this.liveChannels) {
      const row = document.createElement("div");
      row.className = "flex items-center gap-2 py-0.5";
      const label = document.createElement("span");
      label.className = "min-w-12 font-semibold";
      label.textContent = channel.name;
      row.appendChild(label);
      const kind = channel.name.startsWith("D") ? "digital" : "analog";
      const measure = this.buildMeasureControl(
        channel.channel,
        channel.name,
        kind,
        () => this.liveSnapshotCapture,
        () => channel.calibration.scale,
      );
      row.appendChild(measure);
      this.snapshotChannelsEl.appendChild(row);
    }
  }

  // ---------------------------------------------------------------------
  // Pattern search & wait (SPEC §2.9, §5.5)
  // ---------------------------------------------------------------------

  private async onSearchClick(): Promise<void> {
    const rec = this.liveRecId;
    const pattern = this.searchPatternEl.value.trim();
    this.searchErrorEl.textContent = "";
    if (rec === null || !pattern) return;
    try {
      const result = await this.ctx.ws.send("rec.search", { rec, pattern });
      this.renderSearchOutcome(searchOutcomeFromResult(result));
    } catch (err) {
      const outcome = searchOutcomeFromError(err);
      // Shown inline, exactly as the server worded it (SPEC §2.9.1: "an
      // agent should be able to fix its own pattern from the message
      // alone" — true for a person reading it too).
      this.searchErrorEl.textContent = outcome.message;
      this.searchResultsEl.replaceChildren();
    }
  }

  private renderSearchOutcome(outcome: SearchOutcome): void {
    this.searchResultsEl.replaceChildren();
    if (outcome.status === "error") {
      this.searchErrorEl.textContent = outcome.message;
      return;
    }
    const summary = document.createElement("p");
    summary.className = HINT;
    summary.textContent = searchResultSummary(outcome);
    this.searchResultsEl.appendChild(summary);
    for (const match of outcome.matches) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = `${BUTTON_BASE} ${BUTTON_NEUTRAL} block w-full text-left`;
      button.textContent = formatMatchTimestamp(match);
      button.addEventListener("click", () => this.centerViewOn(match.t_s));
      this.searchResultsEl.appendChild(button);
    }
  }

  /**
   * A clicked match centers the (paused) view on it, spanning the current
   * follow window (SPEC §5.5: "clickable timestamps that jump the view").
   * `renderer.setViewport()` runs immediately, before the new range's real
   * data has round-tripped through `rec.view.update` — the same "pan first,
   * data catches up next frame" order the renderer's own drag/zoom already
   * uses — so a click visibly jumps the plot at once rather than appearing
   * to do nothing until the next view frame happens to arrive.
   */
  private centerViewOn(tSeconds: number): void {
    const window = followWindowSeconds(this.currentFollowWindowSeconds());
    const t0 = tSeconds - window / 2;
    const t1 = tSeconds + window / 2;
    this.liveViewState = pauseAtViewport(t0, t1);
    this.renderer?.setViewport(t0, t1);
    this.renderLiveStatus();
    this.flushViewUpdate();
  }

  private async onWaitClick(): Promise<void> {
    const rec = this.liveRecId;
    const pattern = this.searchPatternEl.value.trim();
    if (rec === null || !pattern) return;
    const timeoutSeconds = Number(this.waitTimeoutEl.value) || 10;
    const timeoutMs = Math.min(600_000, Math.max(1, Math.round(timeoutSeconds * 1000)));
    this.waitStatusEl.textContent = formatWaitOutcome({ status: "waiting" });
    try {
      // A margin over the server's own timeout so the client never gives up
      // first and misreports a legitimate resolution as a client timeout.
      const result = await this.ctx.ws.send("rec.wait", { rec, pattern, timeout_ms: timeoutMs }, timeoutMs + 2000);
      this.waitStatusEl.textContent = formatWaitOutcome(waitOutcomeFromResult(result));
    } catch (err) {
      this.waitStatusEl.textContent = formatWaitOutcome(waitOutcomeFromError(err));
    }
  }

  // ---------------------------------------------------------------------
  // Cursors & viewport (SPEC §5: two time cursors showing Δt and 1/Δt)
  // ---------------------------------------------------------------------

  private renderCursorReadout(): void {
    const a = Number(this.cursorAEl.value) || 0;
    const b = Number(this.cursorBEl.value) || 0;
    const readout = cursorReadout({ a, b });
    this.cursorReadoutEl.textContent =
      readout.frequencyHz === null
        ? `Δt = ${formatSiDuration(readout.deltaSeconds)}`
        : `Δt = ${formatSiDuration(readout.deltaSeconds)}, 1/Δt = ${formatSiFrequency(readout.frequencyHz)}`;
  }

  // ---------------------------------------------------------------------
  // Measurements (SPEC §5.4) — server-side only, on a full-resolution
  // capture; the browser never computes a measurement itself.
  // ---------------------------------------------------------------------

  private buildMeasureControl(
    channel: number,
    label: string,
    kind: "analog" | "digital",
    getCapture: () => string | null,
    getResolutionV: () => number | undefined = () => undefined,
  ): HTMLElement {
    const wrapper = document.createElement("span");
    const button = document.createElement("button");
    button.type = "button";
    button.className = "rounded border border-awic-border bg-awic-bg-raised px-2 py-0.5 text-xs hover:bg-awic-bg";
    button.textContent = "Measure";
    button.addEventListener("click", () => {
      const capture = getCapture();
      if (!capture) return;
      const kinds = kind === "analog" ? ANALOG_MEASUREMENT_KINDS : DIGITAL_MEASUREMENT_KINDS;
      const resolution = kind === "analog" ? getResolutionV() : undefined;
      const request = {
        capture,
        channel,
        label,
        kinds,
        ...(resolution !== undefined ? { resolutionV: Math.abs(resolution) } : {}),
      };

      // Show a pending row immediately, then replace it once the server
      // responds — the request is genuinely asynchronous, and a stale
      // "measuring…" row must never be mistaken for a real answer.
      const pending: MeasurementStatus = { status: "pending", request };
      this.measurementStatuses = addMeasurementStatus(this.measurementStatuses, pending);
      this.renderMeasurements();
      if (this.sidePanelEl.hidden) this.sidePanelEl.hidden = false;

      void requestMeasurement((params) => this.ctx.ws.send("capture.measure", params), request).then(
        (resolved) => {
          this.measurementStatuses = this.measurementStatuses.map((s) => (s === pending ? resolved : s));
          this.renderMeasurements();
        },
      );
    });
    wrapper.appendChild(button);
    return wrapper;
  }

  /**
   * One block per capture, newest first, each headed with when it was taken,
   * its rate and its length. Results from a capture that is no longer the one
   * on screen stay readable but are dimmed and marked, so an old value is
   * never taken for a current one.
   */
  private renderMeasurements(): void {
    const current = this.currentImageCapture();
    this.measurementsCurrentCapture = current;
    this.measurementsEl.replaceChildren();
    if (this.measurementStatuses.length === 0) {
      const hint = document.createElement("p");
      hint.className = HINT;
      hint.textContent = "No measurement yet. Use Measure next to a channel under Channels & ranges (or under Snapshot in Live mode).";
      this.measurementsEl.appendChild(hint);
      return;
    }
    const clear = document.createElement("button");
    clear.type = "button";
    clear.className = "mb-2 rounded border border-awic-border bg-awic-bg-raised px-2 py-0.5 text-xs hover:bg-awic-bg";
    clear.textContent = "Clear";
    clear.addEventListener("click", () => {
      this.measurementStatuses = [];
      this.renderMeasurements();
    });
    this.measurementsEl.appendChild(clear);

    for (const block of measurementBlocks(this.measurementStatuses)) {
      const stale = block.capture !== current;
      const section = document.createElement("div");
      section.className = `mb-3 ${stale ? "opacity-60" : ""}`;
      section.dataset.capture = block.capture;
      const heading = document.createElement("div");
      heading.className = "flex items-baseline justify-between gap-2 border-b border-awic-border pb-0.5 text-xs font-semibold";
      const title = document.createElement("span");
      title.className = "truncate";
      title.textContent = captureHeader(this.captureInfos.get(block.capture));
      title.title = title.textContent;
      heading.appendChild(title);
      if (stale) {
        const badge = document.createElement("span");
        badge.className = "flex-none font-normal text-awic-fg-muted";
        badge.textContent = "previous capture";
        heading.appendChild(badge);
      }
      section.appendChild(heading);

      const table = document.createElement("table");
      table.className = "w-full border-collapse text-sm";
      const header = document.createElement("tr");
      header.innerHTML = '<th class="p-1 text-left">Channel</th><th class="p-1 text-left">Kind</th><th class="p-1 text-left">Value</th>';
      table.appendChild(header);
      for (const status of block.statuses) {
        for (const measurementRow of measurementRows(status)) {
          const row = document.createElement("tr");
          const channelCell = document.createElement("td");
          channelCell.className = "p-1";
          channelCell.textContent = measurementRow.label;
          const kindCell = document.createElement("td");
          kindCell.className = "p-1";
          kindCell.textContent = measurementRow.kind;
          const valueCell = document.createElement("td");
          valueCell.className = "p-1 tabular-nums";
          valueCell.textContent = measurementRow.text;
          row.append(channelCell, kindCell, valueCell);
          table.appendChild(row);
        }
      }
      section.appendChild(table);
      this.measurementsEl.appendChild(section);
    }
  }

  // ---------------------------------------------------------------------
  // Decode (SPEC §6, §11.3, T21): capture.decode (Single) / rec.decode
  // (Live), annotation lanes on the renderer, and the frame list.
  // ---------------------------------------------------------------------

  private renderDecodeDecoderOptions(): void {
    const previous = this.decodeDecoderEl.value;
    this.decodeDecoderEl.replaceChildren();
    for (const decoder of this.decoders) {
      const option = document.createElement("option");
      option.value = decoder.id;
      option.textContent = decoder.name;
      this.decodeDecoderEl.appendChild(option);
    }
    const stillPresent = this.decoders.some((d) => d.id === previous);
    this.decodeDecoderEl.value = stillPresent ? previous : (this.decoders[0]?.id ?? "");
    this.onDecodeDecoderChanged();
  }

  private currentDecoderInfo(): DecoderInfo | null {
    return this.decoders.find((d) => d.id === this.decodeDecoderEl.value) ?? null;
  }

  /** Digital channels available to bind a decoder role to, for the mode currently active — the recording's own channel list in Live mode (SPEC §5.5: it may not match the device's full capability once attached to an existing recording), the device's digital capability otherwise. */
  /**
   * The channels a decoder role can be bound to.
   *
   * Analog channels belong here as much as digital ones: the server cuts a
   * waveform at a threshold and decodes it (SPEC §6.3), which is how CAN on
   * the scope is decoded over MCP. Listing only `D*` left the role selector on
   * an analog device with nothing in it but "—", so the Graph tab could not
   * reach a decode it was perfectly able to run (B-56).
   */
  private decodeAvailableChannels(): readonly { readonly channel: number; readonly label: string }[] {
    if (this.mode === "live") {
      return this.liveChannels.map((c) => ({ channel: c.channel, label: c.name }));
    }
    return this.channelState.map((c) => ({ channel: c.control.channel, label: c.control.label }));
  }

  /** The analog channels among them — the only ones a `minus` line can name (SPEC §6.3). */
  private decodeAnalogChannels(): readonly { readonly channel: number; readonly label: string }[] {
    if (this.mode === "live") {
      return this.liveChannels
        .filter((c) => !c.name.startsWith("D"))
        .map((c) => ({ channel: c.channel, label: c.name }));
    }
    return this.channelState
      .filter((c) => isAnalogControl(c.control))
      .map((c) => ({ channel: c.control.channel, label: c.control.label }));
  }

  private onDecodeDecoderChanged(): void {
    const decoder = this.currentDecoderInfo();
    this.decodeRoleBindings = new Map();
    this.decodeRoleMinus = new Map();
    this.decodeOptionInputs = decoder ? initialOptionInputs(decoder) : new Map();
    this.renderDecodeRoles();
    this.renderDecodeOptions();
  }

  private renderDecodeRoles(): void {
    this.decodeRolesEl.replaceChildren();
    const decoder = this.currentDecoderInfo();
    if (!decoder) return;
    const channels = this.decodeAvailableChannels();
    const analogChannels = this.decodeAnalogChannels();
    const channelSelect = (
      options: readonly { readonly channel: number; readonly label: string }[],
      selected: number | undefined,
      onPick: (channel: number | undefined) => void,
    ): HTMLSelectElement => {
      const select = document.createElement("select");
      select.className = SELECT;
      const none = document.createElement("option");
      none.value = "";
      none.textContent = "—";
      select.appendChild(none);
      for (const ch of options) {
        const option = document.createElement("option");
        option.value = String(ch.channel);
        option.textContent = ch.label;
        select.appendChild(option);
      }
      select.value = selected !== undefined ? String(selected) : "";
      select.addEventListener("change", () => onPick(select.value === "" ? undefined : Number(select.value)));
      return select;
    };

    for (const role of decoder.channels) {
      const row = document.createElement("label");
      row.className = LABEL;
      const roleName = document.createElement("span");
      roleName.className = "w-10 shrink-0 font-semibold uppercase";
      roleName.textContent = role;
      const select = channelSelect(channels, this.decodeRoleBindings.get(role), (channel) => {
        const next = new Map(this.decodeRoleBindings);
        if (channel === undefined) next.delete(role);
        else next.set(role, channel);
        this.decodeRoleBindings = next;
      });
      select.dataset.role = role; // e2e hook: pick a role's channel select without a per-row id
      row.append(roleName, select);

      // A differential pair is only a thing on a waveform (SPEC §6.3).
      if (analogChannels.length > 0) {
        const minusName = document.createElement("span");
        minusName.className = "shrink-0";
        minusName.textContent = "−";
        minusName.title = "Optional: decode this role as channel − this channel (CANH − CANL)";
        const minusSelect = channelSelect(analogChannels, this.decodeRoleMinus.get(role), (channel) => {
          const next = new Map(this.decodeRoleMinus);
          if (channel === undefined) next.delete(role);
          else next.set(role, channel);
          this.decodeRoleMinus = next;
        });
        minusSelect.dataset.minusRole = role; // e2e hook
        row.append(minusName, minusSelect);
      }
      this.decodeRolesEl.appendChild(row);
    }

    if (analogChannels.length > 0) this.decodeRolesEl.appendChild(this.analogThresholdRow());
  }

  /**
   * The analog front end's own two settings (SPEC §6.3). They are not in
   * `decode.list`'s options — the server takes them for any decoder on an
   * analog capture and cuts the waveform with them before a decoder sees a bit
   * — so the form carries them itself. Left blank, the server finds the
   * threshold from the signal, which is what it does well.
   */
  private analogThresholdRow(): HTMLElement {
    const row = document.createElement("div");
    row.className = "flex flex-wrap items-center gap-2";
    const field = (
      label: string,
      hint: string,
      current: RawOptionValue | undefined,
      onInput: (value: string) => void,
    ): HTMLLabelElement => {
      const wrapper = document.createElement("label");
      wrapper.className = LABEL;
      wrapper.title = hint;
      const input = document.createElement("input");
      input.type = "number";
      input.step = "1";
      input.placeholder = "auto";
      input.className = NUMBER_INPUT;
      input.value = current === undefined || typeof current === "boolean" ? "" : current;
      input.addEventListener("input", () => onInput(input.value));
      wrapper.append(label, input);
      return wrapper;
    };
    row.append(
      field(
        "threshold (mV) ",
        "Level the waveform is cut at; blank lets the server find it from the signal",
        this.decodeThresholdMv,
        (value) => (this.decodeThresholdMv = value === "" ? undefined : value),
      ),
      field(
        "hysteresis (mV) ",
        "Dead band around the threshold, to keep ringing from reading as edges",
        this.decodeHysteresisMv,
        (value) => (this.decodeHysteresisMv = value === "" ? undefined : value),
      ),
    );
    return row;
  }

  private renderDecodeOptions(): void {
    this.decodeOptionsEl.replaceChildren();
    const decoder = this.currentDecoderInfo();
    if (!decoder) return;
    for (const spec of decoder.options) {
      const row = document.createElement("label");
      row.className = LABEL;
      row.title = spec.description;
      const name = document.createElement("span");
      name.className = "w-16 shrink-0";
      name.textContent = spec.id;
      row.appendChild(name);

      if (spec.type === "bool") {
        const input = document.createElement("input");
        input.type = "checkbox";
        input.checked = Boolean(this.decodeOptionInputs.get(spec.id) ?? spec.default);
        input.addEventListener("change", () => {
          const next = new Map(this.decodeOptionInputs);
          next.set(spec.id, input.checked);
          this.decodeOptionInputs = next;
        });
        row.appendChild(input);
      } else if (spec.type === "enum") {
        const select = document.createElement("select");
        select.className = SELECT;
        for (const value of spec.values) {
          const option = document.createElement("option");
          option.value = value;
          option.textContent = value;
          select.appendChild(option);
        }
        select.value = String(this.decodeOptionInputs.get(spec.id) ?? spec.default);
        select.addEventListener("change", () => {
          const next = new Map(this.decodeOptionInputs);
          next.set(spec.id, select.value);
          this.decodeOptionInputs = next;
        });
        row.appendChild(select);
      } else {
        const input = document.createElement("input");
        input.type = "number";
        input.step = "1";
        if (spec.min !== null && spec.min !== undefined) input.min = String(spec.min);
        if (spec.max !== null && spec.max !== undefined) input.max = String(spec.max);
        input.className = NUMBER_INPUT;
        input.value = String(this.decodeOptionInputs.get(spec.id) ?? spec.default);
        input.addEventListener("change", () => {
          const next = new Map(this.decodeOptionInputs);
          next.set(spec.id, input.value);
          this.decodeOptionInputs = next;
        });
        row.appendChild(input);
      }
      this.decodeOptionsEl.appendChild(row);
    }
  }

  private async onDecodeApplyClick(): Promise<void> {
    const decoder = this.currentDecoderInfo();
    this.decodeErrorEl.textContent = "";
    if (!decoder) return;
    const built = buildDecodeConfig(decoder, this.decodeRoleBindings, this.decodeOptionInputs, {
      minus: this.decodeRoleMinus,
      thresholdMv: this.decodeThresholdMv,
      hysteresisMv: this.decodeHysteresisMv,
    });
    if (built.status === "invalid") {
      this.decodeErrorEl.textContent = built.errors.map((e) => e.message).join("; ");
      return;
    }
    const { channels, options } = built.config;

    try {
      let annotations: readonly DecodedAnnotation[];
      let rateHz: number;
      // Live's view frames and `rec.decode` both count from the recording's
      // own start, so there is nothing to shift there.
      let origin = 0;
      if (this.mode === "single") {
        const capture = this.acqState.captureHandle;
        if (!capture) {
          this.decodeErrorEl.textContent = "no capture yet — run Start first";
          return;
        }
        const response = await this.ctx.ws.send("capture.decode", {
          capture,
          decoder: decoder.id,
          channels: [...channels],
          options: [...options],
        });
        annotations = response.annotations ?? [];
        rateHz = this.effective?.cfg.rate_hz ?? this.currentRateHz();
        origin = this.captureT0Seconds;
        if (response.truncated) this.decodeErrorEl.textContent = `truncated: ${response.omitted} annotation(s) omitted`;
      } else {
        const rec = this.liveRecId;
        if (rec === null) {
          this.decodeErrorEl.textContent = "no recording yet — run Start first";
          return;
        }
        const range = this.computeVisibleRange();
        const response = await this.ctx.ws.send("rec.decode", {
          rec,
          decoder: decoder.id,
          channels: [...channels],
          options: [...options],
          t0_s: range?.t0,
          t1_s: range?.t1,
        });
        annotations = response.annotations ?? [];
        rateHz = this.liveRateHz;
        if (response.truncated) this.decodeErrorEl.textContent = `truncated: ${response.omitted} annotation(s) omitted`;
        this.decodeActive = true;
      }
      this.decodeAnnotations = annotations;
      this.decodeOriginSeconds = origin;
      this.applyDecodeAnnotationsToRenderer(channels.map((c) => c.channel), rateHz);
      this.renderDecodeFrames(rateHz);
    } catch (err) {
      this.decodeErrorEl.textContent = err instanceof Error ? err.message : String(err);
    }
  }

  /** Redraws whatever the renderer holds for annotations under one lane per distinct bound channel — several roles on the same wire (SPI's CLK also carrying, say, a role reused elsewhere) would otherwise draw the same boxes twice. */
  private applyDecodeAnnotationsToRenderer(channels: readonly number[], rateHz: number): void {
    const distinct = [...new Set(channels)];
    const lanes = distinct.map((channel) =>
      annotationLaneFor(channel, this.decodeAnnotations, rateHz, this.decodeOriginSeconds),
    );
    this.renderer?.setAnnotations(lanes);
  }

  private renderDecodeFrames(rateHz: number): void {
    this.decodeFramesEl.replaceChildren();
    const rows = decodedFrameRows(this.decodeAnnotations, rateHz, this.decodeOriginSeconds);
    if (rows.length === 0) {
      const hint = document.createElement("p");
      hint.className = HINT;
      hint.textContent = "No frames decoded.";
      this.decodeFramesEl.appendChild(hint);
      return;
    }
    for (const row of rows) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = `${BUTTON_BASE} ${BUTTON_NEUTRAL} block w-full truncate text-left text-xs`;
      button.textContent = `${formatDecodedFrameTimestamp(row)} — ${row.label}`;
      button.addEventListener("click", () => this.centerViewOnDecoded(row.tSeconds));
      this.decodeFramesEl.appendChild(button);
    }
  }

  /**
   * Clicking a decoded frame centers the view on it (SPEC T21: "clicking one
   * centers the view on it"). Live mode reuses `centerViewOn` (pause +
   * `rec.view.update`, same as a pattern-search match); Single mode has no
   * server-side view to update — the whole capture is already client-side —
   * so this just re-centers the renderer's own viewport, keeping whatever
   * span is currently shown.
   */
  private centerViewOnDecoded(tSeconds: number): void {
    if (this.mode === "live") {
      this.centerViewOn(tSeconds);
      return;
    }
    const t0 = Number(this.viewStartEl.value);
    const t1 = Number(this.viewEndEl.value);
    const span = Number.isFinite(t0) && Number.isFinite(t1) && t1 > t0 ? t1 - t0 : 0.01;
    const newT0 = tSeconds - span / 2;
    const newT1 = tSeconds + span / 2;
    this.renderer?.setViewport(newT0, newT1);
    this.viewStartEl.value = String(newT0);
    this.viewEndEl.value = String(newT1);
  }

  /** Live mode only (SPEC T21: "Live mode re-decodes the visible range when the view changes"), debounced via `decodeDebouncer`. */
  private autoRedecode(): void {
    if (this.mode === "live" && this.decodeActive) void this.onDecodeApplyClick();
  }

  private resetDecodeResults(): void {
    this.decodeAnnotations = [];
    this.decodeActive = false;
    this.decodeErrorEl.textContent = "";
    this.renderer?.setAnnotations([]);
    this.renderDecodeFrames(1);
  }

  // ---------------------------------------------------------------------
  // Static control wiring
  // ---------------------------------------------------------------------

  private wireStaticControls(): void {
    this.deviceSelectEl.addEventListener("change", () => {
      const id = this.deviceSelectEl.value || null;
      this.selectedDeviceInfo =
        this.devices.find((d) => d.id === id) ?? (this.selectedDeviceInfo?.id === id ? this.selectedDeviceInfo : null);
      this.selectedDeviceId = id;
      this.controlsBuiltFrom = controlsFingerprint(this.selectedDeviceId, this.selectedDeviceInfo?.capabilities);
      this.onDeviceChanged();
      // Drops a "not connected" entry the user has just moved away from.
      this.renderDeviceOptions();
    });
    this.modeSingleEl.addEventListener("click", () => this.setMode("single"));
    this.modeLiveEl.addEventListener("click", () => this.setMode("live"));
    this.panelToggleEl.addEventListener("click", () => {
      this.sidePanelEl.hidden = !this.sidePanelEl.hidden;
    });
    this.rendererSelectEl.addEventListener("change", () => {
      this.rendererId = this.rendererSelectEl.value as RendererId;
      this.mountRenderer();
    });
    this.rateSelectEl.addEventListener("change", () => this.applyLayout());
    this.samplesInputEl.addEventListener("change", () => this.applyLayout());
    this.triggerEnabledEl.addEventListener("change", () => {
      this.triggerForm = { ...this.triggerForm, enabled: this.triggerEnabledEl.checked };
    });
    this.triggerChannelEl.addEventListener("change", () => {
      this.triggerForm = { ...this.triggerForm, channel: Number(this.triggerChannelEl.value) || 0 };
    });
    this.triggerSlopeEl.addEventListener("change", () => {
      this.triggerForm = { ...this.triggerForm, slope: this.triggerSlopeEl.value as Slope };
    });
    this.triggerLevelEl.addEventListener("change", () => {
      this.triggerForm = { ...this.triggerForm, level: Number(this.triggerLevelEl.value) || 0 };
    });
    this.startButtonEl.addEventListener("click", () => void this.onStartClick());
    this.stopButtonEl.addEventListener("click", () => void this.onStopClick());
    this.followButtonEl.addEventListener("click", () => this.onFollowClick());
    this.snapshotButtonEl.addEventListener("click", () => void this.onSnapshotClick());
    this.imageButtonEl.addEventListener("click", () => this.onImageClick());
    this.imageKindEl.addEventListener("change", () => this.refreshImageControls());
    this.imageChannelEl.addEventListener("change", () => this.refreshImageControls());
    this.searchButtonEl.addEventListener("click", () => void this.onSearchClick());
    this.waitButtonEl.addEventListener("click", () => void this.onWaitClick());
    this.decodeDecoderEl.addEventListener("change", () => this.onDecodeDecoderChanged());
    this.decodeApplyEl.addEventListener("click", () => void this.onDecodeApplyClick());
    this.cursorAEl.addEventListener("input", () => this.renderCursorReadout());
    this.cursorBEl.addEventListener("input", () => this.renderCursorReadout());
    this.viewStartEl.addEventListener("change", () => this.applyViewport());
    this.viewEndEl.addEventListener("change", () => this.applyViewport());

    this.renderAcqStatus();
    this.renderRunStopButtons();
    this.renderLiveButtons();
    this.renderCursorReadout();
    this.renderMeasurements();
    this.renderSnapshotSection();
  }

  private applyViewport(): void {
    const t0 = Number(this.viewStartEl.value);
    const t1 = Number(this.viewEndEl.value);
    if (!Number.isFinite(t0) || !Number.isFinite(t1) || !this.renderer) return;
    this.renderer.setViewport(t0, t1);
    if (this.mode === "live" && this.liveViewId !== null) {
      this.liveViewState = pauseAtViewport(t0, t1);
      this.renderLiveStatus();
      this.flushViewUpdate();
      if (this.decodeActive) this.decodeDebouncer.call(undefined);
    }
  }
}

customElements.define(TAG, GraphModule);

registerModule({
  id: "graph",
  title: "Graph",
  factory: () => document.createElement(TAG) as GraphModule,
});
