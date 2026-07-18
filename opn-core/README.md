# OPN-CORE

## Quickstart

Prerequisites: [Rust](https://rustup.rs), [Docker](https://docs.docker.com/engine/install/), [just](https://github.com/casey/just).

```sh
cp .env.example .env
just dev
cargo run -p opn-core --bin opn-core
curl localhost:8080/healthz   # -> 200
```

## Mint a session

Create a tenant (owner-role CLI; the API key is printed once, only its hash
is stored):

```sh
cargo run -p opn-core --bin opn-core -- admin create-tenant --name demo --new-world demoworld
```

Then mint a session with the printed key:

```sh
curl -X POST localhost:8080/v1/tenants/self/sessions \
  -H "Authorization: Bearer <api key>" \
  -H "Content-Type: application/json" \
  -d '{"framework_ref":"steam:110000112345678"}'
# -> { token, session_id, character: { number: "555-XXXX", ... }, device }
```

See [../docs/OPN-CORE.md](../docs/OPN-CORE.md) for architecture and [../docs/opn-core-roadmap.md](../docs/opn-core-roadmap.md) for the roadmap.
