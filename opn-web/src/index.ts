export * from "./types";
export * from "./socket";
export * from "./http";

import { OpnSocket, type OpnSocketOptions } from "./socket";
import { OpnHttp } from "./http";

export interface OpnClient {
  socket: OpnSocket;
  http: OpnHttp;
}

/**
 * Wire a socket and an HTTP reader to the same Core and the same (refreshing)
 * token. `httpBaseUrl` defaults to the socket URL with the scheme swapped and
 * `/ws` stripped. Call `socket.connect()` when ready.
 */
export function createOpnClient(
  opts: OpnSocketOptions & { httpBaseUrl?: string },
): OpnClient {
  const socket = new OpnSocket(opts);
  const baseUrl =
    opts.httpBaseUrl ??
    opts.url.replace(/^ws(s?):\/\//, "http$1://").replace(/\/ws\/?$/, "");
  const http = new OpnHttp({ baseUrl, token: () => socket.token });
  return { socket, http };
}
