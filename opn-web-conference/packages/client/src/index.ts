import { OpnConnection } from "./connection.ts";
import type { ClientOptions } from "./types.ts";

/**
 * Open an authenticated, self-healing OPN session (roadmap W0).
 *
 * ```ts
 * const conn = connect({ url: "ws://localhost:8080/ws", token, remint });
 * conn.onState((s) => render(s));
 * conn.on("ch:" + roomId, (push) => { if (push.evt === "channels.message") … });
 * await conn.sub("ch:" + roomId);
 * await conn.cmd({ cmd: "channels.send", payload: { channel_id, client_uuid, body } });
 * ```
 */
export function connect(opts: ClientOptions): OpnConnection {
  return new OpnConnection(opts);
}

export { OpnConnection } from "./connection.ts";
export { OpnError, type OpnErrorCode } from "./errors.ts";
export { DedupeRing } from "./dedupe.ts";
export { backoffMs } from "./backoff.ts";
export type {
  Ack,
  AckPayload,
  ClientOptions,
  ConnectionState,
  Push,
  PushHandler,
  Scheduler,
  WebSocketLike,
} from "./types.ts";

// Re-export the wire types so apps can `import { Cmd, Evt } from "@opn/client"`
// without also depending on `@opn/contracts` directly.
export type * from "@opn/contracts";
