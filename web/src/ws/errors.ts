import type { ErrorInfo } from "../proto/types";

/** Rejection reason for a request that received `{ ok: false, error }`. */
export class RpcError extends Error {
  readonly info: ErrorInfo;

  constructor(info: ErrorInfo) {
    super(`${info.code}: ${info.msg}`);
    this.name = "RpcError";
    this.info = info;
  }
}

/** Rejection reason for a request that never received a response in time. */
export class RpcTimeoutError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RpcTimeoutError";
  }
}

/** Normalizes anything a `send()` promise can reject with into `ErrorInfo`. */
export function toErrorInfo(err: unknown): ErrorInfo {
  if (err instanceof RpcError) return err.info;
  if (err instanceof Error) return { code: err.name, msg: err.message };
  return { code: "unknown", msg: String(err) };
}
