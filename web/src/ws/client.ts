import type {
  Request as WireRequest,
  Response as WireResponse,
  Event as ServerEvent,
  ErrorInfo,
  DeviceListResult,
  UsbTreeResult,
  UsbPowerResult,
  UsbProtectResult,
  AcqConfigureResult,
  AcqStartResult,
  AcqStopResult,
  CaptureMeasureResult,
  RecStartResult,
  RecAck,
  RecListResult,
  RecViewResult,
  RecSearchResult,
  RecWaitResult,
  RecSnapshotResult,
  CaptureDecodeResult,
  RecDecodeResult,
  DecodeListResult,
  TermSourcesResult,
  TermOpenResult,
  TermWriteResult,
  TermCloseResult,
  McpStatusResult,
  McpLogResult,
  McpImageResult,
  McpLoggingState,
} from "../proto/types";
import { Emitter } from "./emitter";
import { type SocketLike, wrapNativeWebSocket } from "./socket";
import { type BackoffOptions, DEFAULT_BACKOFF, computeBackoffDelay } from "./backoff";
import { FRAME_MAGIC, decodeFrame, type DecodedFrame } from "./frame";
import { VIEW_FRAME_MAGIC, decodeViewFrame, type DecodedViewFrame } from "./view-frame";
import { isTermFrame, decodeTermFrame, type DecodedTermFrame } from "./term-frame";
import { SequenceTracker, type FrameGapNotice } from "./sequence";
import { RpcError, RpcTimeoutError } from "./errors";

export type ConnectionState = "connecting" | "connected" | "reconnecting" | "offline";

/**
 * Maps each request `op` to its `result` type. This is the single place that
 * ties the two halves of the tagged unions in `proto/types.ts` together —
 * `Request` is tagged on `op`, `ResponseResult` is untagged, and the wire
 * format relies on the client already knowing which result shape to expect
 * from the `id` it sent. If the Rust side ever renames or drops an op this
 * map still lists, `RequestParams` below collapses to `never` and every
 * `send()` call site becomes a compile error rather than a runtime surprise.
 */
export interface ResultForOp {
  "device.list": DeviceListResult;
  "usb.tree": UsbTreeResult;
  "usb.power": UsbPowerResult;
  "usb.protect": UsbProtectResult;
  "acq.configure": AcqConfigureResult;
  "acq.start": AcqStartResult;
  "acq.stop": AcqStopResult;
  "capture.measure": CaptureMeasureResult;
  "rec.start": RecStartResult;
  "rec.stop": RecAck;
  "rec.delete": RecAck;
  "rec.list": RecListResult;
  "rec.view": RecViewResult;
  "rec.view.update": RecAck;
  "rec.view.close": RecAck;
  "rec.search": RecSearchResult;
  "rec.wait": RecWaitResult;
  "rec.snapshot": RecSnapshotResult;
  "capture.decode": CaptureDecodeResult;
  "rec.decode": RecDecodeResult;
  "decode.list": DecodeListResult;
  "term.sources": TermSourcesResult;
  "term.open": TermOpenResult;
  "term.write": TermWriteResult;
  "term.close": TermCloseResult;
  "mcp.status": McpStatusResult;
  "mcp.log": McpLogResult;
  "mcp.image": McpImageResult;
  "mcp.logging": McpLoggingState;
}

export type RequestOp = keyof ResultForOp;

type RequestFor<Op extends RequestOp> = Extract<WireRequest, { op: Op }>;

/** The request fields the caller supplies; `v`, `id` and `op` are added by `send()`. */
export type RequestParams<Op extends RequestOp> = Omit<RequestFor<Op>, "v" | "id" | "op">;

/** An `{ ok: false }` response, surfaced centrally so no caller has to remember to `.catch()`. */
export interface RpcErrorNotice extends ErrorInfo {
  readonly id: number | null;
}

interface PendingRequest {
  readonly resolve: (value: unknown) => void;
  readonly reject: (err: Error) => void;
  readonly timer: ReturnType<typeof setTimeout>;
}

export interface WsClientOptions {
  readonly url: string;
  /** Per-request timeout; a request must fail rather than hang forever. Default 10s. */
  readonly requestTimeoutMs?: number;
  readonly backoff?: Partial<BackoffOptions>;
  /** Injectable for tests; defaults to wrapping a real browser `WebSocket`. */
  readonly socketFactory?: (url: string) => SocketLike;
  /** Injectable for deterministic backoff-jitter tests; defaults to `Math.random`. */
  readonly random?: () => number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/**
 * The one `/ws` connection (SPECIFICATION.md §3): text frames are JSON
 * control messages, binary frames are sample data. Handles reconnect with
 * backoff, request/response `id` correlation with timeouts, event dispatch,
 * and binary frame decoding with sequence-gap detection.
 */
export class WsClient {
  readonly events = new Emitter<ServerEvent>();
  readonly errors = new Emitter<RpcErrorNotice>();
  /** 0xA5 sample frames (SPEC §3.2). */
  readonly frames = new Emitter<DecodedFrame>();
  /** 0xA6 view frames (SPEC §3.4) — a recorder's `rec.view`/`rec.view.update`
   * answers, dispatched by their own magic byte alongside sample frames on
   * the same socket. */
  readonly viewFrames = new Emitter<DecodedViewFrame>();
  readonly frameGaps = new Emitter<FrameGapNotice>();
  /** Decoded `0xA7` terminal output frames (SPEC §3.5, §11.2) — dispatched
   * by the first byte of every binary message, distinct from `0xA5` sample
   * frames above so the terminal module and the graph module never see
   * each other's traffic. */
  readonly termFrames = new Emitter<DecodedTermFrame>();
  /** Sequence-gap detection for terminal sessions, keyed by `session_id`
   * exactly as {@link frameGaps} is keyed by `stream_id` — reusing the same
   * notice shape (its `streamId` field holds the session id here). A
   * server-side drop already carries an in-band `[... dropped ...]` marker
   * (SPEC §3.5); this is the client's own independent check that the
   * marker itself wasn't also lost. */
  readonly termFrameGaps = new Emitter<FrameGapNotice>();
  readonly connectionState = new Emitter<ConnectionState>();

  private readonly url: string;
  private readonly requestTimeoutMs: number;
  private readonly backoff: BackoffOptions;
  private readonly socketFactory: (url: string) => SocketLike;
  private readonly random: () => number;
  private readonly seqTracker = new SequenceTracker();
  private readonly termSeqTracker = new SequenceTracker();
  private readonly pending = new Map<number, PendingRequest>();

  private socket: SocketLike | null = null;
  private nextId = 1;
  private state: ConnectionState = "connecting";
  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closedByUser = true;

  constructor(options: WsClientOptions) {
    this.url = options.url;
    this.requestTimeoutMs = options.requestTimeoutMs ?? 10_000;
    this.backoff = { ...DEFAULT_BACKOFF, ...options.backoff };
    this.socketFactory = options.socketFactory ?? wrapNativeWebSocket;
    this.random = options.random ?? Math.random;
  }

  getConnectionState(): ConnectionState {
    return this.state;
  }

  /** Opens the socket and enables automatic reconnect on unexpected drops. */
  connect(): void {
    this.closedByUser = false;
    this.reconnectAttempt = 0;
    this.openSocket();
  }

  /** Closes the socket and disables reconnect. */
  close(): void {
    this.closedByUser = true;
    this.clearReconnectTimer();
    this.socket?.close();
    this.socket = null;
    this.setState("offline");
  }

  /**
   * Sends a request and resolves with its `result` once the response with a
   * matching `id` arrives, or rejects on an `{ ok: false }` response, a
   * timeout, or the connection dropping first.
   *
   * `timeoutMs` overrides the client's default per-request timeout for this
   * one call — needed for `rec.wait` (SPEC §2.9.3), whose own `timeout_ms`
   * can be up to 600 000 ms server-side; without this, the client would give
   * up and reject long before a legitimately slow-to-occur pattern resolves.
   */
  send<Op extends RequestOp>(op: Op, params: RequestParams<Op>, timeoutMs?: number): Promise<ResultForOp[Op]> {
    const socket = this.socket;
    if (!socket) {
      return Promise.reject(new Error(`cannot send "${op}": socket is not connected`));
    }
    const id = this.nextId++;
    const request = { v: 1, id, op, ...params } as unknown as WireRequest;
    const effectiveTimeoutMs = timeoutMs ?? this.requestTimeoutMs;

    return new Promise<ResultForOp[Op]>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new RpcTimeoutError(`request "${op}" (id ${id}) timed out after ${effectiveTimeoutMs}ms`));
      }, effectiveTimeoutMs);

      this.pending.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
        timer,
      });

      socket.send(JSON.stringify(request));
    });
  }

  private openSocket(): void {
    this.setState(this.reconnectAttempt === 0 ? "connecting" : "reconnecting");
    const socket = this.socketFactory(this.url);

    socket.onopen = () => {
      this.reconnectAttempt = 0;
      this.setState("connected");
    };
    socket.onclose = () => {
      this.socket = null;
      this.failAllPending(new RpcTimeoutError("connection closed before a response arrived"));
      if (this.closedByUser) {
        this.setState("offline");
      } else {
        this.scheduleReconnect();
      }
    };
    socket.onerror = () => {
      // Browsers always follow an 'error' with a 'close'; reconnect scheduling
      // happens there so it isn't duplicated here.
    };
    socket.onmessage = (data) => this.handleMessage(data);

    this.socket = socket;
  }

  private scheduleReconnect(): void {
    this.setState("reconnecting");
    const delay = computeBackoffDelay(this.reconnectAttempt, this.backoff, this.random);
    this.reconnectAttempt += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.closedByUser) this.openSocket();
    }, delay);
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  private setState(state: ConnectionState): void {
    if (this.state === state) return;
    this.state = state;
    this.connectionState.emit(state);
  }

  private handleMessage(data: string | ArrayBuffer): void {
    if (typeof data === "string") {
      this.handleControlMessage(data);
    } else {
      this.handleBinaryMessage(data);
    }
  }

  private handleControlMessage(raw: string): void {
    let msg: unknown;
    try {
      msg = JSON.parse(raw);
    } catch {
      this.errors.emit({ id: null, code: "invalid_json", msg: "received malformed control frame" });
      return;
    }
    if (!isRecord(msg)) return;

    if (typeof msg.id === "number") {
      this.handleResponse(msg as unknown as WireResponse);
    } else if (typeof msg.event === "string") {
      this.events.emit(msg as unknown as ServerEvent);
    }
  }

  private handleResponse(response: WireResponse): void {
    const pending = this.pending.get(response.id);
    if (!pending) return; // stale id (already timed out) or unknown — ignore.
    this.pending.delete(response.id);
    clearTimeout(pending.timer);

    if (response.ok) {
      pending.resolve(response.result);
    } else {
      const info: ErrorInfo = response.error ?? {
        code: "unknown",
        msg: "response marked ok=false with no error info",
      };
      pending.reject(new RpcError(info));
      this.errors.emit({ id: response.id, ...info });
    }
  }

  /**
   * Binary frames share one socket but carry distinct magic bytes (SPEC
   * §3.2 sample frames are 0xA5, §3.4 view frames are 0xA6) so a client can
   * tell them apart from the first byte alone, with no separate channel.
   */
  private handleBinaryMessage(data: ArrayBuffer): void {
    if (data.byteLength === 0) {
      this.errors.emit({ id: null, code: "frame_decode_error", msg: "received empty binary frame" });
      return;
    }
    const magic = new DataView(data).getUint8(0);
    if (magic === VIEW_FRAME_MAGIC) {
      this.handleViewFrame(data);
    } else if (isTermFrame(data)) {
      this.handleTermFrame(data);
    } else if (magic === FRAME_MAGIC) {
      this.handleSampleFrame(data);
    } else {
      this.errors.emit({
        id: null,
        code: "frame_decode_error",
        msg: `unrecognized binary frame magic byte: 0x${magic.toString(16)}`,
      });
    }
  }

  private handleSampleFrame(data: ArrayBuffer): void {
    let decoded: DecodedFrame;
    try {
      decoded = decodeFrame(data);
    } catch (err) {
      this.errors.emit({
        id: null,
        code: "frame_decode_error",
        msg: err instanceof Error ? err.message : String(err),
      });
      return;
    }
    const gap = this.seqTracker.observe(decoded.header.streamId, decoded.header.sequence);
    if (gap) this.frameGaps.emit(gap);
    this.frames.emit(decoded);
  }

  private handleViewFrame(data: ArrayBuffer): void {
    let decoded: DecodedViewFrame;
    try {
      decoded = decodeViewFrame(data);
    } catch (err) {
      this.errors.emit({
        id: null,
        code: "frame_decode_error",
        msg: err instanceof Error ? err.message : String(err),
      });
      return;
    }
    this.viewFrames.emit(decoded);
  }

  private handleTermFrame(data: ArrayBuffer): void {
    let decoded: DecodedTermFrame;
    try {
      decoded = decodeTermFrame(data);
    } catch (err) {
      this.errors.emit({
        id: null,
        code: "term_frame_decode_error",
        msg: err instanceof Error ? err.message : String(err),
      });
      return;
    }
    const gap = this.termSeqTracker.observe(decoded.header.sessionId, decoded.header.sequence);
    if (gap) this.termFrameGaps.emit(gap);
    this.termFrames.emit(decoded);
  }

  private failAllPending(err: Error): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(err);
    }
    this.pending.clear();
  }
}
