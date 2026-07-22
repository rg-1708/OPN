import type { Cmd } from "@opn/contracts/bindings/Cmd";
import type { Evt } from "@opn/contracts/bindings/Evt";
import type { ErrCode } from "@opn/contracts/bindings/ErrCode";
import type { MePayload } from "@opn/contracts/bindings/MePayload";
import type { ChannelSummary } from "@opn/contracts/bindings/ChannelSummary";
import type { ChannelMember } from "@opn/contracts/bindings/ChannelMember";
import type { ServerSummary } from "@opn/contracts/bindings/ServerSummary";
import type { ContactItem } from "@opn/contracts/bindings/ContactItem";
import type { ResolveResult } from "@opn/contracts/bindings/ResolveResult";
import type { GroupJoinAck } from "@opn/contracts/bindings/GroupJoinAck";
import type { UploadTicket } from "@opn/contracts/bindings/UploadTicket";

/** Every client→server command name, derived from the generated contract. */
export type CmdName = Cmd["cmd"];

/** Payload type for one command; commands without a payload map to `never`. */
export type CmdPayload<N extends CmdName> =
  Extract<Cmd, { cmd: N }> extends { payload: infer P } ? P : never;

/**
 * Argument tuple for `cmd()`: payload-less commands take no second argument,
 * everything else requires its exact contract payload.
 */
export type PayloadArgs<N extends CmdName> =
  Extract<Cmd, { cmd: N }> extends { payload: infer P } ? [payload: P] : [];

/** Every server→client event name, derived from the generated contract. */
export type EvtName = Evt["evt"];

/** Payload type for one pushed event. */
export type EvtPayload<N extends EvtName> =
  Extract<Evt, { evt: N }> extends { payload: infer P } ? P : never;

/**
 * Ack payloads are untyped JSON on the wire (OPN-CORE.md §7); this map is
 * hand-verified against Core's dispatch (gateway/dispatch.rs + primitives).
 * Commands not listed here ack `unknown` — narrow at the call site.
 */
export interface AckPayloads {
  "auth.refresh": { token: string };
  "identity.me": MePayload;
  "channels.send": { message_id: string; seq: number };
  "channels.open_direct": { channel_id: string };
  "channels.create": { channel_id: string };
  "channels.list": ChannelSummary[];
  "channels.members": ChannelMember[];
  "servers.create": { server_id: string };
  "servers.list": ServerSummary[];
  "servers.channel_create": { channel_id: string };
  "directory.contacts": ContactItem[];
  "directory.resolve": ResolveResult;
  "directory.listing_create": { id: string };
  "calls.start": { call_id: string };
  "calls.group.create": { call_id: string };
  "calls.group.join": GroupJoinAck;
  "media.request_upload": UploadTicket;
  "ledger.transfer": { transfer_id: string; balance: number };
  "ledger.hold": { hold_id: string };
  "ledger.capture": { transfer_id: string };
  "ledger.withdraw": { exchange_id: string };
  "feed.post": { post_id: string };
  "feed.comment": { comment_id: string };
}

export type AckPayload<N extends CmdName> =
  N extends keyof AckPayloads ? AckPayloads[N] : unknown;

/**
 * Uniform client error. `code` is either a wire `ErrCode` from Core or one of
 * the client-side codes: `closed` (socket dropped before the ack) and
 * `timeout` (no ack within `ackTimeoutMs`).
 */
export class OpnError extends Error {
  constructor(
    public readonly code: ErrCode | "closed" | "timeout",
    msg: string,
  ) {
    super(msg);
    this.name = "OpnError";
  }
}

/** Topic string builders (gateway/topic.rs). */
export const topics = {
  channel: (channelId: string) => `ch:${channelId}`,
  call: (callId: string) => `call:${callId}`,
  notify: (deviceId: string) => `notify:${deviceId}`,
  presence: (characterId: string) => `presence:${characterId}`,
  // The feed topic key IS the `app_id` (contract gap #9): Core has no separate
  // "slug" concept — `feed:<app_id>` is the topic, and `app_id` is what
  // `identity.me` exposes (accounts[].app_id / active_accounts keys). Subscribe
  // with the same app_id you pass to `feed.*` commands.
  feed: (appId: string) => `feed:${appId}`,
} as const;
