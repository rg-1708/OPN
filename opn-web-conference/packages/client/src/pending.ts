import { OpnError } from "./errors.ts";
import type { Ack, AckPayload, Scheduler } from "./types.ts";

interface Pending {
  resolve: (payload: AckPayload) => void;
  reject: (err: OpnError) => void;
  timer: unknown;
}

/**
 * Correlates in-flight commands to their acks by frame id → `reply_to`
 * (OPN-CORE.md §7). One promise per outstanding command; a per-command timer
 * rejects if Core never acks (it always should, so this is a safety net, not a
 * flow-control mechanism).
 */
export class PendingMap {
  readonly #map = new Map<number, Pending>();
  readonly #scheduler: Scheduler;
  readonly #ackTimeoutMs: number;

  constructor(scheduler: Scheduler, ackTimeoutMs: number) {
    this.#scheduler = scheduler;
    this.#ackTimeoutMs = ackTimeoutMs;
  }

  /** Register a command; returns the promise the caller awaits. */
  await(id: number): Promise<AckPayload> {
    return new Promise<AckPayload>((resolve, reject) => {
      const timer = this.#scheduler.setTimeout(() => {
        this.#map.delete(id);
        reject(new OpnError("timeout", `command ${id} timed out`));
      }, this.#ackTimeoutMs);
      this.#map.set(id, { resolve, reject, timer });
    });
  }

  /** Resolve or reject the matching command from an ack. Unknown `reply_to` is ignored. */
  settle(ack: Ack): void {
    const pending = this.#map.get(ack.reply_to);
    if (!pending) return;
    this.#map.delete(ack.reply_to);
    this.#scheduler.clearTimeout(pending.timer);
    if (ack.ok) {
      pending.resolve(ack.payload);
    } else {
      pending.reject(new OpnError(ack.err?.code ?? "internal", ack.err?.msg ?? "command failed"));
    }
  }

  /** Fail every outstanding command (a drop invalidates them all). */
  rejectAll(err: OpnError): void {
    for (const pending of this.#map.values()) {
      this.#scheduler.clearTimeout(pending.timer);
      pending.reject(err);
    }
    this.#map.clear();
  }
}
