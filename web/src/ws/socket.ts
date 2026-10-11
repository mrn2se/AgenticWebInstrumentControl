/**
 * The slice of the browser `WebSocket` API `WsClient` depends on, expressed
 * as callback fields rather than `addEventListener` so a test mock can be a
 * plain object literal with no `EventTarget` machinery behind it.
 */
export interface SocketLike {
  onopen: (() => void) | null;
  onclose: ((code: number, reason: string) => void) | null;
  onerror: (() => void) | null;
  onmessage: ((data: string | ArrayBuffer) => void) | null;
  send(data: string | ArrayBuffer | ArrayBufferView<ArrayBuffer>): void;
  close(code?: number, reason?: string): void;
}

/** Adapts a real browser `WebSocket` to {@link SocketLike}. */
export function wrapNativeWebSocket(url: string): SocketLike {
  const socket = new WebSocket(url);
  socket.binaryType = "arraybuffer";

  const wrapper: SocketLike = {
    onopen: null,
    onclose: null,
    onerror: null,
    onmessage: null,
    send: (data) => socket.send(data),
    close: (code, reason) => socket.close(code, reason),
  };

  socket.addEventListener("open", () => wrapper.onopen?.());
  socket.addEventListener("close", (ev) => wrapper.onclose?.(ev.code, ev.reason));
  socket.addEventListener("error", () => wrapper.onerror?.());
  socket.addEventListener("message", (ev: MessageEvent<unknown>) => {
    const data = ev.data;
    if (typeof data === "string" || data instanceof ArrayBuffer) {
      wrapper.onmessage?.(data);
    }
  });

  return wrapper;
}
