import type { SocketLike } from "../ws/socket";
import { DemoServer, DemoSocket } from "./server";

/** A socket factory for `WsClient` whose other end is a simulated bench. Reconnects find the same bench. */
export function demoSocketFactory(): () => SocketLike {
  const server = new DemoServer();
  return () => new DemoSocket(server);
}
