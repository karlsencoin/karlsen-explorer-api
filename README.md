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

## Configuration for `/info`

`BPS` (default `1`, as the legacy production setting) feeds the hashrate and halving
formulas; `MAX_SUPPLY_KLS` (default `4960848817`) is reported by `/info/coinsupply`.
Floats are written the way Python's `repr` writes them (`src/pyfloat.rs`), because
clients were built against the Python service (`8.3549e-05`, not `8.3549e-5`).

## Live feed

One background task holds a single persistent `notifyBlockAdded` subscription to
karlsend and fans blocks out to every `/live/stream` client, so node load does not
grow with the number of viewers. Blocks are batched before sending. It reconnects
with backoff (1 s up to 30 s) and also when the node stays silent for
`LIVE_IDLE_TIMEOUT_S`.

| Variable | Default | Meaning |
|---|---|---|
| `LIVE_BATCH_MS` | `250` | collect blocks this long before one batch is sent |
| `LIVE_RECENT_BLOCKS` | `50` | blocks in the snapshot a new client receives |
| `LIVE_IDLE_TIMEOUT_S` | `60` | reconnect when no message arrives for this long |
| `LIVE_MAX_CLIENTS` | `500` | concurrent stream clients; more get 503 |

Behind nginx the stream needs `proxy_buffering off;` (the response also sends
`X-Accel-Buffering: no`) and a `proxy_read_timeout` above the 15 s keep-alive.
Amounts are sompi strings; the first output's address is derived from its script.

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
| `GET /addresses/{address}/balance` | karlsend UTXO index; invalid address → 422; node failure → 503 |
| `GET /addresses/{address}/utxos` | karlsend UTXO index. New, optional: `limit` (1..500) and `offset` page the set newest first and add `X-Total-Count` / `X-Total-Amount` (sompi); the sorted set is cached 15 s per address. Without `limit`: the full list, as legacy |
| `POST /addresses/balances` | `{"addresses": [...]}`, at most 1000 per request |
| `POST /addresses/utxos` | `{"addresses": [...]}`, at most 1000 per request |
| `GET /addresses/{address}/full-transactions` | `limit` 1..500, `offset`, `fields`, `resolve_previous_outpoints=no\|light\|full` |
| `GET /addresses/{address}/transactions-count` | `{"total": n}`; kept in memory 8 s / 20 s / 60 s / 10 min by size (<10k / 10k / 100k / 1M+), one query per address at a time |
| `GET /transactions/{transactionId}` | `inputs`, `outputs`, `resolve_previous_outpoints` |
| `POST /transactions/search` | `transactionIds` (max 1000) or `acceptingBlueScores` (range max 100); `fields`, `acceptance` |
| `GET /info/blockdag`, `/info/network` | node |
| `GET /info/hashrate` | `difficulty × 2 × BPS / 1e12` (TH/s), `stringOnly` |
| `GET /info/coinsupply`, `/coinsupply/circulating`, `/coinsupply/total` | node; plain text for the last two, `in_billion` |
| `GET /info/blockreward`, `/info/halving` | Karlsen emission table (`src/emission.rs`), `stringOnly`, `field` |
| `GET /info/karlsend` | node; P2P id only as SHA-256 |
| `GET /info/price`, `/info/market-data` | NonKYC KLS/USDT ticker, cached 60 s; last good value if NonKYC fails, 503 if none |
| `GET /info/mempool` | new (not in legacy): pending transactions from karlsend `getMempoolEntries`, highest fee first; `count` is the full size, at most 100 listed |
| `GET /info/dag-window` | database; newest `limit` (1..1000, default 200) blocks, or blocks after `since_daa` (ascending, `no-store`) |
| `GET /info/miners/top`, `/info/miners/network`, `/info/miners/address/{address}` | KGI reward tracker SQLite (`KGI_TRACKER_DB`), opened read-only; `limit`, `recent_hours`, `hours` |
| `GET /info/nodes`, `/info/peers/geo`, `/info/seeder-nodes` | KGI scanner files (`discovered_nodes.json`, `seeder_history.json`, `geo_cache.json`) + `peer_geo` history (7-day window) + `node_history.db` inbound streaks, all read-only; IPs masked to /16 |
| `GET /admin/seed-candidates` | full IPs for the operator; `X-KGI-Admin` must equal `KGI_ADMIN_TOKEN` (404 when unset) |
| `GET /blocks` | karlsend `getBlocks` from `lowHash`; `includeBlocks`, `includeTransactions`; `extra` is `null`, empty lists on node failure (as legacy) |
| `GET /analytics/addresses/top`, `/range`, `/total`, `/distribution` | `balances` table; rich list with 24h trend from `top_balance_snapshots`; equal balances ordered by address |
| `GET /analytics/addresses/distribution-summary` | tier counts and balances with 1d / 7d deltas from `distribution_snapshots` |
| `GET /analytics/addresses/{address}/history` | received / sent / net per transaction, newest first; `limit` 1..200 |
| `GET /live/stream` | new: Server-Sent Events. `snapshot` (recent blocks) on connect, then `blocks` batches; `lagged` if a client fell behind. One shared karlsend `BlockAdded` subscription for all clients; 503 above `LIVE_MAX_CLIENTS` |
| `GET /live/recent` | new: the same snapshot as JSON; `X-Live-Connected` tells whether the node subscription is up |
