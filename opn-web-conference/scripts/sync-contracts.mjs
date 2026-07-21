#!/usr/bin/env node
// Maintainer helper (monorepo only): refresh the vendored @opn/contracts types
// from the Rust crate's generated bindings. NOT part of a fork's workflow — a
// fork consumes @opn/contracts from npm (see packages/contracts/README.md).
//
//   just ts   # in opn-core, regenerates crates/contracts/bindings
//   node scripts/sync-contracts.mjs
//
import { cp, readdir, writeFile, rm } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const src = join(here, "..", "..", "opn-core", "crates", "contracts", "bindings");
const dst = join(here, "..", "packages", "contracts", "src");

await rm(dst, { recursive: true, force: true });
await cp(src, dst, { recursive: true });

// Regenerate the barrel (one `export *` per generated type; index.ts is derived,
// never hand-edited). The eventual npm publish of @opn/contracts will run this
// same step in CI.
const files = (await readdir(dst)).filter((f) => f.endsWith(".ts") && f !== "index.ts").sort();
await writeFile(dst + "/index.ts", files.map((f) => `export * from './${f.slice(0, -3)}';`).join("\n") + "\n");
console.log(`synced ${files.length} contract types from ${src}`);
