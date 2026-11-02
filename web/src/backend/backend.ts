import type { SocketLike } from "../ws/socket";

/**
 * Where the app's data comes from. What a module needs to know about "the
 * server" beyond the `/ws` protocol lives here, so a module never reads
 * `window.location` and works the same against the server that served the page
 * and against the simulated bench.
 */
export interface Backend {
  readonly kind: "self" | "demo";
  /** The `/ws` URL; a demo backend has none and supplies `socketFactory`. */
  readonly wsUrl: string;
  readonly socketFactory?: (url: string) => SocketLike;
  /** URL of an HTTP resource on this server, or `null` where the backend has no HTTP side. */
  resourceUrl(pathAndQuery: string): string | null;
  /** The address MCP clients should be pointed at. */
  readonly mcpLocation: { readonly protocol: string; readonly host: string };
}

/** The address the demo's MCP settings show: where awic listens once installed. */
const DEMO_MCP_LOCATION = { protocol: "http:", host: "127.0.0.1:8080" };

export function selfBackend(location: { protocol: string; host: string }): Backend {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return {
    kind: "self",
    wsUrl: `${scheme}//${location.host}/ws`,
    resourceUrl: (path) => path,
    mcpLocation: location,
  };
}

export function demoBackend(socketFactory: () => SocketLike): Backend {
  return {
    kind: "demo",
    wsUrl: "demo://simulated-bench/ws",
    socketFactory,
    resourceUrl: () => null,
    mcpLocation: DEMO_MCP_LOCATION,
  };
}
