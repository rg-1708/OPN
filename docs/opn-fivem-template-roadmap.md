# OPN FiveM Resource UI Template — Roadmap (draft v0.1)

Companion to [OPN.md](OPN.md) §§7, 10 (perf budget, devices & SDK, lb-compat)
and [OPN-CORE.md](OPN-CORE.md). Same reading rules: sprints
are **scope-bound, not time-bound**; Goal / Depends on / Work items /
Test plan / Exit criteria per sprint; OPN.md wins on any conflict (ADR-5
governs lb-compat); Core-side changes go through a CDR first.

## What this template is

`opn-fivem` — the reference phone: the FiveM resource (Lua game plane) plus
the NUI frontend (shell + first-party apps), structured exactly as OPN.md
§10.1 prescribes so a server forks it into a private repo and reskins
without touching logic:

```
opn-fivem/                the FiveM resource (fxmanifest, client/server Lua)
web/
  packages/client         @opn/client — wire runtime (built by the web
                          template roadmap, consumed here)
  packages/sdk            @opn/sdk — headless React hooks/stores, zero JSX
  packages/shell-phone    springboard, status bar, nav, overlays, tokens
  packages/apps/*         first-party apps (Messages, Dialer, Contacts, …)
  packages/lb-compat      optional lb-phone third-party app shim
```

MIT, per the OPN.md §10.1 licensing split. The fork story *is* the product:
tokens for ~90% of visual identity, component fork for the rest, headless
layer untouched.

### On "100% lb-phone backwards compatibility"

The stated goal is that third-party lb-phone apps run on OPN unmodified.
ADR-5 already records the honest ceiling: lb's API is **unversioned and
partly undocumented**, so "100%" is not a testable claim against an
unenumerable surface. This roadmap therefore pins compat to an enumerated,
versioned target instead:

- **100% of the official lb-phone custom-app template surface** — every
  export, injected global, and `fetchNui` convention the template uses,
  clean-room reimplemented from signatures.
- **A named compat matrix of popular free lb apps** (picked in F6, tracked
  as a table in the repo) — each row green = installs via `AddCustomApp`
  and its core flows work.

The matrix is the definition of done; anything outside it found broken is a
bug report against the matrix (add a row), not a violated guarantee. If true
100%-of-everything ever becomes the requirement, that is an ADR-5 amendment
conversation, not a sprint item.

## Stack (decided — performance-first, 103-safe)

Modern libraries are welcome wherever they buy measured performance; every
candidate passes two gates before entering `package.json`: (a) output runs
on Chromium 103 ([restrictions.md](restrictions.md)) — verified in CEF, not
assumed; (b) it earns its bundle bytes (per-app lazy chunks stay small).

- **React 18** — fixed by OPN.md §10.1 (the SDK's public surface is React +
  contracts client). Concurrent features (`useSyncExternalStore`,
  transitions) used where they help input latency.
- **zustand** — per-app stores (the no-god-context rule); selector
  subscriptions keep cross-app re-renders at zero.
- **TanStack Virtual** — every scrollable surface (threads, galleries,
  contact lists). Virtualization is the single biggest CEF win.
- **Vite + Lightning CSS** — build; `browserslist: chrome 103` makes the
  transpile/reject gate tooling, not memory.
- **Tailwind v3 (pinned)** — styling in `shell-*` and `apps/*` only; never
  in `sdk` (rule 5). Build-time atomic CSS, zero runtime cost, 103-safe
  output. Theme values map to CSS variables
  (`colors: { primary: 'var(--opn-primary)' }`) so the token reskin story
  (OPN.md §10.1) stays vars-only — no tailwind config edit to reskin.
  **v4 is banned**: its output leans on `oklch()`/`color-mix()`
  (Chrome 111+, restrictions.md) and dies in CEF 103; the CI grep gate
  enforces the ban mechanically.
- **TanStack Query only if a real need appears** — most reads are
  WS-subscription-driven through the SDK; don't add a cache layer the
  resume/seq machinery already provides. (ponytail: revisit if HTTP-heavy
  apps — Gallery paging — measurably want it.)
- Animation: CSS transform/opacity first; a JS spring lib (motion) admitted
  only for gesture-driven shell nav if CSS provably can't hit frame budget.

Rule of thumb: fancy is fine, but each lib is admitted by a measured win on
a mid-range client in-game, and the 103 gate is non-negotiable.

## Cross-cutting rules (every sprint)

1. **Chromium 103 gate, tooling-enforced** (OPN.md §7,
   [restrictions.md](restrictions.md)): `browserslist: chrome 103` in every
   web package; stylelint/eslint rules reject the known-missing list
   (`backdrop-filter`, `:has()`, container queries, `dvh/svh`, `oklch`,
   nesting, subgrid, …). Forks inherit the guard. Never rely on memory.
2. **0.00 ms idle** on the game plane: no per-frame loops while the phone
   is closed; statebags for open/prop state; no polling. Checked every
   sprint with the resource monitor, recorded in the sprint notes.
3. **Zero app data over FiveM netcode** (OPN.md §1.1). The Lua surface
   stays the small game-plane RPC set; any PR adding a net event carrying
   app data is wrong by definition.
4. **Contracts from npm, exact pin**; `@opn/client` shared with the web
   template. No fork of the wire layer.
5. **`@opn/sdk` public surface = React + contracts client only** (OPN.md
   §10.1). No state/styling library leaks; enforced by a dependency-lint in
   CI (`sdk`'s `package.json` dependency allowlist).
6. **CEF render budget rules** (OPN.md §7): lazy route chunk per app,
   per-app stores, virtualized lists, transform/opacity animations only,
   thumbnails in lists. Reviewed per app at merge, spot-profiled in-game.
7. **Every sprint ends runnable in-game**: a dev FXServer recipe boots the
   resource against the dockerized Core dev stack; the sprint demo is
   performed on a real client, not just a browser.

## Sprint sequence at a glance

| # | Name | Nominal | Depends on | Delivers |
|---|---|---|---|---|
| F0 | Scaffold + NUI boot | 1–2 w | web template W0 | workspace, fxmanifest, ui_page SPA boot, chrome-103 lint gate, dev FXServer recipe |
| F1 | Game plane + auth chain | 2 w | F0; Core ≥ S2 | framework bridge seam, token mint, phone open/close, WS live from NUI |
| F2 | `@opn/sdk` headless layer | 2 w | F1; Core ≥ S4 | useChannel/usePresence/useNotify/useCall + stores: subscriptions, optimistic sends, seq reconciliation, resume |
| F3 | Shell | 2–3 w | F2 | springboard, status bar, notifications, settings, manifests, tokens, overlays |
| F4 | MVP apps | 2–3 w | F3; Core ≥ S6 | Messages, Dialer (+ voice targets via pma-voice), Contacts |
| F5 | Media apps | 2 w | F4; Core ≥ S5 | Camera (game render), Gallery, attachments in Messages |
| F6 | lb-compat shim | 2–3 w | F3 (tier 1), F5 (tier 3) | AddCustomApp, iframe host, injected globals, pickers, createGameRender; compat matrix |
| F7 | Reskin ergonomics + release | 1–2 w | F4+F6 | tokens/fork guide, example reskin, CI, template packaging |

Total nominal: ~15–19 weeks. F5 and F6-tier-1 can overlap with a second
developer; F0–F4 are serial (each hardens what the next builds on, same
argument as the Core roadmap's ordering).

## Sprint F0 — Scaffold + NUI boot

**Goal**: an empty phone opens in-game with the whole build/lint/perf
skeleton already enforcing the rules forks will inherit.

**Depends on**: web template W0 (`@opn/client` exists).

### Work items

1. **Workspace scaffold** per the layout above (pnpm workspaces); Vite
   building one `ui_page` SPA; React (the SDK layer promises React hooks —
   OPN.md §10.1 — so the shell commits to it here).
2. **Chromium 103 enforcement**: browserslist, Lightning CSS transpile/
   reject config, lint rules for the restrictions.md list, and one CI check
   that greps built CSS for the forbidden features (belt and braces — the
   build output is what CEF actually sees).
3. **fxmanifest + minimal Lua**: resource loads, `ui_page` serves the built
   SPA, keybind toggles NUI focus, statebag for phone-open. 0.00 ms idle
   verified.
4. **Dev recipe**: docs + script for a local FXServer (txAdmin or bare)
   pointing at the Core dev compose stack; `OPN_CORE_URL` + tenant key
   config convention for the resource.

### Test plan

- CI: typecheck, lint (including 103 gate), build.
- Manual in-game: resource loads clean, phone opens/closes, resmon 0.00 idle.

### Exit criteria

- [ ] Phone frame opens in-game showing a placeholder shell.
- [ ] A PR using `backdrop-filter` or `:has()` fails CI.
- [ ] resmon: 0.00 ms with phone closed.

## Sprint F1 — Game plane + auth chain

**Goal**: the NUI holds a live authenticated WS to Core, minted the
production way (OPN.md §3), with the framework bridge as a clean seam.

**Depends on**: F0; Core Sprints 1–2.

### Work items

1. **Framework bridge seam** (server Lua): one file exposing
   `GetCharacterRef(source)` etc.; ESX/QBCore adapters as thin
   implementations, plus a standalone stub (fork point for custom
   frameworks). Money/items bridge deferred to the wallet app era — seam
   named, not built (YAGNI until an app consumes it).
2. **Token mint flow**: on character select, server Lua →
   `POST /v1/tenants/self/sessions` (API key, server-side only) → token to
   NUI via a single net event (the one game-plane message auth needs) →
   `@opn/client` connect from the `cfx-nui-*` origin. Refresh + reconnect
   already come with the client.
3. **Game-plane RPC surface v0**: phone open/close, focus handling,
   `useGame()` plumbing in the SDK-to-be (postMessage/NUI callback
   conventions, typed).
4. **Tenant link consumer stub** (server Lua): connect `wss://core/link`,
   version handshake, log events — voice-target handling lands in F4 with
   the Dialer; establishing the connection early surfaces proxy/TLS issues
   on real hosts.

### Test plan

- Lua-side: mint against real Core from the dev FXServer; wrong key / dead
  Core paths degrade to a visible "phone offline" state, never a stuck UI.
- NUI: reuse `@opn/client` smoke (connect, takeover, reconnect) from inside
  CEF — run once in-game, scripted where possible.

### Exit criteria

- [ ] Character select → phone shows `live` with the character's number.
- [ ] Core restart while in-game: phone self-heals without resource restart.
- [ ] API key absent from every byte served to NUI (grep the built assets
      + traffic).

## Sprint F2 — `@opn/sdk` headless layer

**Goal**: the correctness 80% (OPN.md §10.1) as headless hooks any reskin
inherits: subscriptions, optimistic sends, seq reconciliation, resume —
zero JSX.

**Depends on**: F1; Core Sprint 4 (channels complete).

### Work items

1. Per-app store architecture (zustand-style, one store per app — the
   no-god-context rule) with the contracts client injected at the root.
2. Hooks over `@opn/client`: `useConnection`, `usePresence`,
   `useChannel(id)` (messages window, virtualized-list-friendly paging,
   optimistic send with `client_uuid`, receipts, typing), `useNotifyInbox`,
   `useCall()` (FSM snapshots + signaling passthrough), `useSettings(scope)`.
3. Notification routing: `notify.event` → per-class handling surface
   (ring/alert/silent) the shell consumes in F3.
4. Dependency-lint: `sdk` may depend on React + `@opn/client` +
   `@opn/contracts`, nothing else (cross-cutting rule 5).

### Test plan

- Hook tests against the mock WS server from the web template (shared test
  harness — same wire, same scripts).
- One in-CEF sanity run: `useChannel` end-to-end against real Core.

### Exit criteria

- [ ] A 20-line React component using `useChannel` sends/receives with
      optimistic reconciliation — demoed in-game.
- [ ] `sdk` builds with the dependency allowlist green; no styling imports.

## Sprint F3 — Shell

**Goal**: the phone chrome: springboard, status bar, notifications,
settings — composed from app manifests, themed by tokens.

**Depends on**: F2.

### Work items

1. **App manifest** (OPN.md §10): id, icons, targets, required primitives,
   notification types; launcher composes from manifests — installing an app
   is data.
2. **Springboard + navigation**: app grid, open/close/back, lazy route
   chunk per app, `content-visibility` on off-screen surfaces.
3. **Status bar + notification overlays**: ring/alert/silent presentation,
   notification center reading `useNotifyInbox`.
4. **Settings app**: device/character settings via `identity.get/set_settings`
   (wallpaper, ringtone, airplane, per-app toggles) — settings jsonb schema
   is ours, Core only caps size.
5. **Design tokens**: CSS variables (colors, radius, fonts, wallpaper) as
   the documented reskin surface; icons via manifest.
6. **Shell services API** (internal): overlays (popup/context menu), media
   picker and contact picker *interfaces* (implementations arrive with F5
   apps) — defined now because lb-compat (F6) maps onto exactly these.

### Test plan

- Component tests for launcher/manifest composition.
- In-game perf pass: springboard open, app open/close under resmon +
  CEF profiling; no re-render of app A on activity in app B (store
  isolation asserted with a render-count harness).

### Exit criteria

- [ ] Apps install by dropping a manifest + route chunk; launcher updates
      with zero shell code changes.
- [ ] Notification classes render correctly from a real `notify` event.
- [ ] Token-only reskin demo: swap one tokens file → visibly different
      phone, zero component edits.

## Sprint F4 — MVP apps: Messages, Dialer, Contacts

**Goal**: the daily-driver trio, each a thin component layer over the SDK.

**Depends on**: F3; Core Sprint 6 (calls + tenant link).

### Work items

1. **Messages**: thread list (`channels.list`), SMS/DM via `open_direct`,
   groups via `create`; virtualized history, typing, receipts, reactions,
   pins. Attachments UI stubbed until F5.
2. **Contacts**: directory CRUD, blocks (block = unreachable, matching
   Core's privacy semantics), avatar via media once F5 lands.
3. **Dialer**: `calls.start/accept/decline/hangup` over `useCall`; ring via
   notify class `ring` (works with the app closed — the shell holds the
   WS); call UI states from `calls.state` snapshots.
4. **Voice targets** (server Lua, tenant link): consume
   `calls.voice { set_targets | clear }` → pma-voice call channels + phone
   EQ submix; re-sync active calls on link reconnect
   (`GET /v1/tenants/self/calls/active`).
5. **Video calls**: WebRTC video track only, audio stays in pma-voice
   (OPN.md §6); face-camera Lua surface + `useGameRender` capture,
   ~480p cap; STUN + optional coturn.

### Test plan

- SDK-level flows already covered; app tests are thin component tests.
- Two-client in-game session (the real test): text, group chat, voice call
  with audible pma-voice submix, video call. Scripted checklist, performed
  each sprint-end from here on.

### Exit criteria

- [ ] Two players text and group-chat; offline player's phone catches up on
      reconnect, no dupes.
- [ ] Voice call connects with proximity audio correctly replaced by call
      audio, and cleanly restored on hangup (link `clear`).
- [ ] Video call renders both faces at stable frame rate on a mid-range
      client.

## Sprint F5 — Media apps: Camera, Gallery, attachments

**Goal**: the media pipeline end to end: capture in-game, upload presigned,
attach, view.

**Depends on**: F4; Core Sprint 5 (media).

### Work items

1. **Game render surface**: `createGameRender`-equivalent (WebGL game feed
   into NUI) — mounted-only lifecycle, destroyed on unmount (the ~10–30 MB
   CEF rule); photo + short video capture.
2. **Camera app**: capture → presigned upload → `media.commit` → saved.
3. **Gallery app**: `media.list` cursor paging, thumbnails only in grid,
   favourites, tombstones for expired objects.
4. **Messages attachments**: media picker service (F3 interface) now real;
   send with `media_ids`, thumbnail render in threads.

### Test plan

- Upload/commit/expiry flows against real MinIO in the dev stack, including
  the pending-timeout janitor path (upload, don't commit, verify tombstone).
- In-game: photo → attach → other player views; memory profile before/after
  camera app close (render surface actually freed).

### Exit criteria

- [ ] Photo taken in-game appears in another player's thread as thumbnail →
      full view.
- [ ] Camera close releases the game render (measured, not assumed).

## Sprint F6 — lb-compat shim

**Goal**: third-party lb-phone apps install and run unmodified, per the
enumerated compat target above.

**Depends on**: F3 (tier 1); F5 (tier 3). Optional package — the phone
ships without it; forks that don't want lb apps delete one folder.

### Work items (the ADR-5 / OPN.md §10.2 tiers, in order)

1. **Tier 1 — core shim**:
   - `AddCustomApp` export in `opn-fivem`: lb manifest → OPN manifest
     mapping, forwarded to the shell.
   - Shell iframe host: `cfx-nui-<res>` origin apps as the first consumer
     of the third-party iframe seam; mount on open, kill on close.
   - Injected globals over shell services: `sendNotification`→notify,
     `getSettings`/settings mapping (unsupported fields stubbed),
     popup/context menu→shell overlays. `fetchNui`/`useNuiEvent` need no
     shim (iframe posts to its own origin) — verify, don't build.
2. **Tier 2 — pickers**: gallery, emoji, gif, color, contact, share —
   mapped onto the F3/F5 shell services.
3. **Tier 3 — `createGameRender`**: the WebGL game-feed export for
   camera-style apps; heaviest, last, and only if a matrix app needs it.
4. **Compat matrix** (`packages/lb-compat/COMPAT.md`): rows = official lb
   template + the chosen popular free apps (selection criterion: download
   counts + surface diversity, picked at sprint start); columns = install,
   core flows, pickers, game render. CI-run where headless-testable,
   in-game checklist otherwise. Clean-room note per ADR-5: signatures only,
   no lb source consulted.

### Test plan

- Shim unit tests per injected global against recorded template behavior.
- The matrix itself is the test plan: every row exercised each time the
  shim changes.

### Exit criteria

- [ ] Official lb-phone custom-app template: 100% of its surface green.
- [ ] Every named matrix app installs via `AddCustomApp` and passes its
      core-flow row in-game.
- [ ] Deleting `packages/lb-compat` leaves the phone building and running
      (optionality proven).

## Sprint F7 — Reskin ergonomics + release

**Goal**: the fork story delivered: a server reskins in a day, and the
template is packaged for it.

**Depends on**: F4 (F6 in parallel is fine).

### Work items

1. **Reskin guide**: tokens reference, manifest/icon swap, component-fork
   path for full re-themes (headless layer untouched — the promise, in
   writing, with an example).
2. **Example reskin**: one alternate tokens file + a couple of forked
   components, kept in-repo as living proof the seams hold.
3. **CI + release**: build, lint gates, SDK dependency allowlist, contracts
   pin policy (bump = PR + in-game smoke checklist), tagged releases of the
   template; `@opn/client` npm publish happens now (second consumer
   exists — closes the web-template W3 gate).
4. **Fork-friendliness pass**: no hardcoded server names, all config
   surfaced, `packages/apps/*` deletable individually.

### Exit criteria

- [ ] Someone other than the author produces a visibly distinct phone from
      the reskin guide in ≤ a day, without touching `sdk` or `client`.
- [ ] Tagged v0.1 of the template installable on a clean FXServer against a
      released Core.

## Risks worth naming

- **lb's unversioned surface drifts** under the shim (their template
  updates). Parry: the matrix pins tested versions; a matrix re-run is the
  upgrade ritual, and ADR-5's ceiling is already accepted.
- **CEF 103 is the permanent floor** — no `backdrop-filter` ever
  (restrictions.md). Parry: tooling gate from F0; designs reviewed against
  the restrictions list before build, not after.
- **The SDK becoming the kitchen sink.** Every reskin inherits whatever
  leaks into `sdk`. Parry: the dependency allowlist lint + "React +
  contracts client only" as a hard review rule.
- **Voice-target edge cases** (link down mid-call, FXServer restart
  mid-call) are where players will actually judge stability. Parry: F4's
  re-sync item is exit-criteria-gated, and the two-client scripted session
  runs every sprint from F4 on.
