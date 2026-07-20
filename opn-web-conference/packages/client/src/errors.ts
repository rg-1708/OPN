import type { ErrCode } from "@opn/contracts";

/**
 * Codes a rejected command can carry: every Core `ErrCode` (from an `ok:false`
 * ack) plus the client-synthetic ones for failures that never reach an ack.
 */
export type OpnErrorCode = ErrCode | "not_connected" | "closed" | "timeout";

/** Thrown by `cmd()` (and friends) when a command does not succeed. */
export class OpnError extends Error {
  readonly code: OpnErrorCode;

  constructor(code: OpnErrorCode, message: string) {
    super(message);
    this.name = "OpnError";
    this.code = code;
  }
}
