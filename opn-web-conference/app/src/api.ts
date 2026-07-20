import type { CharacterInfo, MessageItem } from "@opn/contracts";

// Same-origin HTTP surface. Vite (dev) / deploy/server.mjs (prod) proxy:
//   /join, /rooms  → dev-auth   |   /v1 → Core REST (JWT-authed by the browser)
// The browser never learns Core's URL or the tenant key.

export interface JoinResponse {
  token: string;
  session_id: string;
  character: CharacterInfo;
}
export interface RoomSummary {
  id: string;
  name: string;
  member_count: number;
}
export interface RoomMember {
  character_id: string;
  name: string;
}
interface ApiError {
  code: string;
  msg: string;
}

async function readError(res: Response): Promise<string> {
  const err = (await res.json().catch(() => null)) as ApiError | null;
  return err?.msg ?? `request failed (${res.status})`;
}

async function postJson<T>(url: string, body: unknown): Promise<T> {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(await readError(res));
  return (await res.json()) as T;
}

async function getJson<T>(url: string, token?: string): Promise<T> {
  const res = await fetch(url, {
    headers: token ? { Authorization: `Bearer ${token}` } : undefined,
  });
  if (!res.ok) throw new Error(await readError(res));
  return (await res.json()) as T;
}

export const api = {
  join: (name: string) => postJson<JoinResponse>("/join", { name }),

  listRooms: () => getJson<{ rooms: RoomSummary[] }>("/rooms").then((r) => r.rooms),

  createRoom: (name: string, characterId: string, characterName: string) =>
    postJson<RoomSummary>("/rooms", {
      name,
      character_id: characterId,
      character_name: characterName,
    }),

  joinRoom: (roomId: string, characterId: string, characterName: string) =>
    postJson<{ ok: boolean; room: RoomSummary }>(`/rooms/${roomId}/join`, {
      character_id: characterId,
      character_name: characterName,
    }),

  members: (roomId: string) =>
    getJson<{ members: RoomMember[] }>(`/rooms/${roomId}/members`).then((r) => r.members),

  /** Newest-first history page (`GET /v1/channels/:id/messages`), JWT-authed. */
  history: (channelId: string, token: string, beforeSeq?: number) => {
    const q = beforeSeq === undefined ? "" : `?before_seq=${beforeSeq}`;
    return getJson<MessageItem[]>(`/v1/channels/${channelId}/messages${q}`, token);
  },
};
