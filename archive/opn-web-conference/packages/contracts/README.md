# @opn/contracts (vendored)

These are **generated TypeScript wire types**, mirrored from the Rust `contracts`
crate in `opn-core`. They are **vendored** — committed here as a stand-in until the
`@opn/contracts` npm package publishes.

**Do not hand-edit** anything under `src/`. Changes belong in the Rust crate.

## Regenerating (maintainers only)

Not part of a fork's workflow. Maintainers refresh the vendored copy from the crate's
generated bindings:

```bash
node scripts/sync-contracts.mjs
```

(Run after regenerating the bindings in `opn-core`, e.g. `just ts`.) The script also
rebuilds the `src/index.ts` barrel.

## After the npm package ships

Once `@opn/contracts` is published, a fork will simply depend on the npm version and
**delete this vendored copy**. Until then, this directory is the source of truth for
the wire types.
