# OPN-CORE

## Quickstart

Prerequisites: [Rust](https://rustup.rs), [Docker](https://docs.docker.com/engine/install/), [just](https://github.com/casey/just).

```sh
cp .env.example .env
just dev
cargo run -p opn-core
curl localhost:8080/healthz   # -> 200
```

See [../docs/OPN-CORE.md](../docs/OPN-CORE.md) for architecture and [../docs/opn-core-roadmap.md](../docs/opn-core-roadmap.md) for the roadmap.
