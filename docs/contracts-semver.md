# Contracts versioning policy

How the wire surface is versioned, gated, and surfaced. Roadmap Sprint 11 item
6; policy source of truth OPN.md §10.1. Companion:
[releasing.md](releasing.md) (where the version number gets cut and shipped).

## The surface

The [`contracts`](../opn-core/crates/contracts) crate
(`opn-core/crates/contracts`, currently **`0.1.0`**) is the **single** wire-type
surface. Every `Cmd`, `Evt`, `ErrCode`, and shared DTO the client and server
exchange is defined there, once. It is exported to TypeScript —

```sh
cd opn-core
just ts        # cargo run -p contracts --bin export_ts
```

— into [`crates/contracts/bindings/`](../opn-core/crates/contracts/bindings)
as one `.ts` file per type (e.g. `LinkHello.ts`), which is published as the npm
package **`@opn/contracts`**. The crate is the source; the `.ts` bindings are
generated output; the npm package is a mirror of the bindings. There is no
second hand-maintained copy of a wire type anywhere — that is the whole point
of the crate.

## Policy: additive-only within a major

OPN.md §10.1. Within a major version the wire surface may only *grow*:

| Change | Bump |
|--------|------|
| New **optional** field on an existing type | **minor** |
| New `Cmd` variant | **minor** |
| New `Evt` variant | **minor** |
| Remove or rename a field | **major** |
| Change a field's wire shape / type / required-ness | **major** |
| New `ErrCode` variant | **major** (the enum is CLOSED — see below) |

The rule the number encodes: a client built against `X.y` keeps working against
any `X.z, z ≥ y`. New variants are additive because a well-behaved client
already tolerates unknown `Cmd`/`Evt` kinds; a removed or reshaped field breaks
a client that reads it, so it is a major, full stop.

### `ErrCode` is closed

`ErrCode` is a **closed enum**: the client exhaustively matches every variant,
so an unrecognized code is not "unknown error, degrade gracefully" — it is a
type that does not typecheck. Adding a variant therefore breaks every consumer
and is a **contracts-major event** (established roadmap Sprint 0). If you reach
for a new error code, you are cutting a major. Prefer reusing an existing code
with a message over widening the enum.

## The drift gate (CI, since Sprint 0)

The bindings cannot silently fall out of sync with the crate. The
`contracts-drift` job in [`ci.yml`](../.github/workflows/ci.yml) (`name: CI`,
every push/PR) regenerates the `.ts` from the crate and fails on any diff:

```sh
cargo run -p contracts --bin export_ts
git diff --exit-code -- crates/contracts/bindings
```

Change a wire type without running `just ts` and committing the regenerated
bindings → red CI. The committed `.ts` is thus provably the crate's current
shape, and any wire change is visible as a bindings diff in review — which is
also where a reviewer catches an accidental major (a *removed* line in
`bindings/` is a breaking change by definition).

## `CONTRACTS_VERSION` at runtime (live this slice)

The crate embeds its own version at compile time —

```rust
// crates/contracts/src/lib.rs
pub const CONTRACTS_VERSION: &str = env!("CARGO_PKG_VERSION");
```

— and the running Core surfaces it in two places so a deploy or an incident
triage can confirm *which contract* is live without shelling in:

- **`/healthz` JSON body** — `contracts_version` (alongside `core_version`),
  on both the `200` and the `503` path (`crates/core/src/http/mod.rs`).
- **`/link` hello ack payload** — `contracts_version`
  (`crates/core/src/gateway/link.rs`), so a client learns the server's contract
  version on connect.

```sh
curl -sf https://<host>/healthz | jq '{contracts_version, core_version}'
```

Because the value comes from `CARGO_PKG_VERSION`, bumping the crate version is
the *only* action needed to change what the wire reports — no second constant
to keep in sync.

## npm publish on git tag — *planned (Sprint 11 part B)*

Cutting a git tag does **not** yet publish `@opn/contracts`. The
publish-on-tag pipeline is roadmap Sprint 11 part B and is **not wired**. Until
it lands, publishing the npm package is a manual step and the tag only pins the
verified commit (see [releasing.md](releasing.md) step 1).

## Coverage guarantee

No command or event can exist without a named integration test. The exhaustive
`Cmd`/`Evt` match-tests (roadmap cross-cutting rule 2) are compiler-enforced:
adding a variant makes the match non-exhaustive until you add its arm, and the
arm asserts against a real integration test. So a *minor* bump (a new variant)
cannot ship untested — the type system refuses to compile the coverage test
until the test exists. This is why "additive-only" is safe to move fast on:
every addition is a tested addition.
