# karlsen-explorer-api

Read-only HTTP API for the Karlsen block explorer, KGI, the website and external
integrations. It is a wire-compatible Rust replacement for the legacy Python
`rest-v2` service and is designed for a 10 blocks-per-second network.

Status: **early scaffold**. See [docs/SCOPE.md](docs/SCOPE.md) for the endpoint
list (chosen from 14 days of real traffic) and [docs/VERIFY.md](docs/VERIFY.md)
for what must be checked against the live legacy API before cut-over.

## Principles

- **Errors are errors.** Backend failures return 5xx with `{"detail": ...}`; they are
  never turned into an empty success such as `{"balance": 0}`.
- **Addresses are derived from `script_public_key`** at read time (`src/address.rs`),
  not from stored text columns.
- **Money is integer sompi** end to end.
- **Configuration only through environment variables.** No credentials, e-mail
  addresses or hostnames live in this repository (see `.env.example`).

## Data sources

Like the Kaspa REST server, block lookups ask **karlsend** first (gRPC,
`KARLSEND_GRPC_URL`): only the node has every parent level, the real transaction
`mass` and the block color. If the node does not have the block (pruned, header only)
or cannot be reached within `NODE_TIMEOUT_MS`, the response is built from **Postgres**
and carries `X-Data-Source: Database`. The gRPC client is generated from the vendored
rusty-karlsen protos in `crates/explorer-api/proto/` (ISC, see `NOTICE.md`); no
`protoc` is needed to build.

## Run

```sh
cp .env.example .env     # fill in DATABASE_URL locally; .env is git-ignored
cargo run --release -p karlsen-explorer-api
```

## Test

Unit tests need nothing. Integration tests create a throw-away database from
`tests/fixtures/schema-v20.sql` plus synthetic seed data:

```sh
TEST_DATABASE_URL=postgres://USER@127.0.0.1:5432/postgres cargo test
```

Without `TEST_DATABASE_URL` the database tests skip themselves. The node tests
(`tests/node.rs`) need nothing: they run a mock karlsend that replays the captured
legacy responses in `tests/fixtures/golden-*.json`.

## Implemented so far

| Endpoint | Notes |
|---|---|
| `GET /info/health` | 200 only if the database answers, otherwise 503 |
| `GET /blocks/{blockId}` | Node first, database fallback (`X-Data-Source: Database`); `includeTransactions`, `includeColor` |
