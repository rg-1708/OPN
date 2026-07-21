# @opn/client — OPN web template

The framework-agnostic backbone for building an OPN frontend: one typed
WebSocket client + HTTPS read wrappers over `@opn/contracts`. No UI, no UI
library, no state library — fork it and build yours on top.

```bash
npm install
npm test           # wire-layer unit tests
npm run example    # vanilla demo (example/main.ts)
npm run build      # emit dist/ (ESM + d.ts)
```

```ts
import { createOpnClient, topics } from "@opn/client";

const { socket, http } = createOpnClient({
  url: "wss://core.example.com/ws",
  token: async () => fetchSessionJwtFromYourBackend(),
});
await socket.connect();

const channels = await socket.cmd("channels.list");
socket.sub(topics.channel(channels[0].channel_id));
socket.on("channels.message", (m) => console.log(m));
```

**Read [GUIDE.md](GUIDE.md)** — the architecture, the connection lifecycle,
and the patterns (message lists, state wiring, LiveKit group calls, media
upload) for building a real UI on this.

Layout:

- `src/socket.ts` — the multiplexed WSS connection: auth-first-frame, ack
  correlation, topic subscribe/resume, reconnect, JWT refresh
- `src/http.ts` — cold reads (history, media, feed, inbox)
- `src/types.ts` — types derived from the generated contracts + ack map
- `example/` — smallest possible consumer, vanilla DOM
