# Endpoint scope

Derived from 14 days of nginx access logs (aggregated counts only; no client data is
kept here). Callers: explorer, KGI, the website, the mobile wallet, the deposit
watcher and a few external integrations.

## To port (26)

- `/info`: `blockdag`, `dag-window`, `hashrate`, `price`, `market-data`, `coinsupply`,
  `coinsupply/circulating`, `blockreward`, `halving`, `karlsend`, `network`, `health`,
  `nodes`, `seeder-nodes`, `miners/top`, `peers/geo`
- Blocks and transactions: `GET /blocks`, `GET /blocks/{hash}`,
  `GET /transactions/{hash}`, `POST /transactions/search`
- Addresses: `GET /addresses/{addr}/balance|utxos|full-transactions|transactions-count`,
  `POST /addresses/balances`
- Analytics: `/analytics/addresses/top|distribution-summary|range|total|distribution`,
  `/analytics/addresses/{addr}/history`
- Misc: `/docs`, `/openapi.json`

`/bridge/reserves` is not part of rest-v2: it is served by the separate proof-of-reserves
service (`bridge_reserves.py`, port 8010). That service reads the vault balance from
the REST API (`KARLSEN_REST_URL`, now rest-v2 on 8003) and must be pointed at this API at
cut-over.

## Left behind

Everything else in the legacy API (30+ paths with 0-3 calls in 14 days). During the
migration nginx keeps routing unknown paths to the legacy service and logs them, so
real demand shows up before anything is dropped for good.

## Known caller bugs found in the logs

- KGI calls `GET /transactions/search` (~48k/14d); every call fails. The route is POST.
