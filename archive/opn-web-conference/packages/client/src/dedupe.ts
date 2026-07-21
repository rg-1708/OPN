/**
 * Bounded "have I seen this message_id?" ring (OPN-CORE.md §8: at-least-once to
 * the UI, exactly-once render). Resume replay re-delivers messages the client
 * already has; this drops the repeats. A ring, not an unbounded set, because a
 * long-lived session must not leak one entry per message forever — the window
 * only needs to cover a resume gap, which is capped at 500 (§4.4) well under the
 * default 512.
 */
export class DedupeRing {
  readonly #size: number;
  readonly #seen = new Set<string>();
  readonly #order: string[] = [];

  constructor(size = 512) {
    this.#size = Math.max(1, size);
  }

  /** Records `id`; returns `true` if it was new, `false` if already seen. */
  admit(id: string): boolean {
    if (this.#seen.has(id)) return false;
    this.#seen.add(id);
    this.#order.push(id);
    if (this.#order.length > this.#size) {
      const evicted = this.#order.shift();
      if (evicted !== undefined) this.#seen.delete(evicted);
    }
    return true;
  }
}
