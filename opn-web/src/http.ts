import { OpnError } from "./types";
import type { MessageItem } from "@opn/contracts/bindings/MessageItem";
import type { MediaItem } from "@opn/contracts/bindings/MediaItem";
import type { TransferItem } from "@opn/contracts/bindings/TransferItem";
import type { InboxItem } from "@opn/contracts/bindings/InboxItem";
import type { PostItem } from "@opn/contracts/bindings/PostItem";
import type { CommentItem } from "@opn/contracts/bindings/CommentItem";

export interface Page<T> {
  items: T[];
  next_cursor: string | null;
}

export type PageOpts = {
  cursor?: string;
  limit?: number;
};

export interface OpnHttpOptions {
  /** e.g. `https://core.example.com` (no trailing slash). */
  baseUrl: string;
  /** Current session JWT — pass `() => socket.token` to stay in sync with refresh. */
  token: () => string | null;
  /** Injectable for tests / non-browser runtimes. */
  fetch?: typeof fetch;
}

/**
 * The HTTPS read side: bulk/cold loads that don't belong on the socket
 * (OPN-CORE.md §5). Same session JWT as the socket, `Authorization: Bearer`.
 */
export class OpnHttp {
  constructor(private readonly opts: OpnHttpOptions) {}

  /** Message history, newest first, keyset-paged on `before_seq`. */
  channelMessages(
    channelId: string,
    opts: { beforeSeq?: number; limit?: number } = {},
  ): Promise<MessageItem[]> {
    return this.get(`/v1/channels/${channelId}/messages`, {
      before_seq: opts.beforeSeq,
      limit: opts.limit,
    });
  }

  /** Media gallery; `url`/`thumb_url` are short-lived presigned S3 GETs. */
  media(opts: PageOpts = {}): Promise<Page<MediaItem>> {
    return this.get("/v1/media", opts);
  }

  ledgerHistory(opts: PageOpts = {}): Promise<Page<TransferItem>> {
    return this.get("/v1/ledger/history", opts);
  }

  notifyInbox(opts: PageOpts = {}): Promise<Page<InboxItem>> {
    return this.get("/v1/notify/inbox", opts);
  }

  feedHome(appId: string, opts: PageOpts = {}): Promise<Page<PostItem>> {
    return this.get("/v1/feed/home", { app_id: appId, ...opts });
  }

  feedProfile(
    account: string,
    appId: string,
    opts: PageOpts = {},
  ): Promise<Page<PostItem>> {
    return this.get(`/v1/feed/profile/${account}`, { app_id: appId, ...opts });
  }

  feedPost(
    postId: string,
    appId: string,
    opts: PageOpts = {},
  ): Promise<{ post: PostItem; comments: CommentItem[]; next_cursor: string | null }> {
    return this.get(`/v1/feed/posts/${postId}`, { app_id: appId, ...opts });
  }

  feedHashtag(tag: string, appId: string, opts: PageOpts = {}): Promise<Page<PostItem>> {
    return this.get(`/v1/feed/hashtags/${encodeURIComponent(tag)}`, {
      app_id: appId,
      ...opts,
    });
  }

  /** Unauthenticated; use `contracts_version` for a compat check at boot. */
  healthz(): Promise<{ status: string; contracts_version: string; core_version: string }> {
    return this.get("/healthz", {}, false);
  }

  private async get<T>(
    path: string,
    params: Record<string, unknown>,
    authed = true,
  ): Promise<T> {
    const qs = new URLSearchParams();
    for (const [k, v] of Object.entries(params)) {
      if (v !== undefined) qs.set(k, String(v));
    }
    const url = `${this.opts.baseUrl}${path}${qs.size ? `?${qs}` : ""}`;
    const headers: Record<string, string> = {};
    if (authed) {
      const token = this.opts.token();
      if (!token) throw new OpnError("unauthorized", "no session token");
      headers.authorization = `Bearer ${token}`;
    }
    const doFetch = this.opts.fetch ?? fetch;
    const res = await doFetch(url, { headers });
    if (!res.ok) {
      let body: { code?: string; msg?: string } | null = null;
      try {
        body = await res.json();
      } catch {
        // non-JSON error body
      }
      throw new OpnError(
        (body?.code as OpnError["code"]) ?? "internal",
        body?.msg ?? `HTTP ${res.status}`,
      );
    }
    return res.json() as Promise<T>;
  }
}
