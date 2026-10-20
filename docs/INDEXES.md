# Database indexes the API relies on

The API reads the indexer's schema (v20) as-is. Every hot query must be
answerable from an existing index; a query that is not falls back to a
sequential scan of a multi-GB table.

| Table | Index | Used by |
| --- | --- | --- |
| `blocks` | `blocks_pkey (hash)` | `/blocks/{hash}`, joins from `blocks_transactions` |
| `blocks` | `idx_blocks_daa_score (daa_score DESC)` | `/info/dag-window` (both forms, plus an incremental sort on `hash`) |
| `blocks` | `blocks_blue_score_idx (blue_score)` | blue-score lookups |

## Rules

- Write `ORDER BY` clauses that match an index's direction **and** NULL order.
  `daa_score DESC` in Postgres means `DESC NULLS FIRST`; adding `NULLS LAST`
  makes the index unusable for that sort.
- Check every new query with `EXPLAIN (ANALYZE, BUFFERS)` against the live
  database (read-only user) before deploying. No `Seq Scan` on `blocks`,
  `transactions`, `addresses_transactions`, `blocks_transactions` or
  `block_parent`.
- Every pooled connection runs with `statement_timeout`
  (`DB_STATEMENT_TIMEOUT_MS`, default 25000). A runaway query is cancelled
  instead of stacking up behind nginx's 30 s proxy timeout.

## Incident 2026-10-19

`/info/dag-window` (no `since_daa`) sorted with `daa_score DESC NULLS LAST,
hash DESC`, which no index matched. Each call sorted all ~18M blocks (~110 s).
The KGI DAG view polls this every few seconds, so calls stacked up, Postgres
read the 9 GB table continuously, the VPS swapped and CPU stayed near 100 %
for ~18 hours. nginx timed the calls out and fell back to legacy rest-v2,
which is why `/info/dag-window` dominated `legacy-fallback.log`.

Hotfix on the VPS: `idx_blocks_daa_nl_hash (daa_score DESC NULLS LAST, hash DESC)`.
Once a build with the corrected query is deployed, that index is no longer
needed and can be dropped (`DROP INDEX CONCURRENTLY idx_blocks_daa_nl_hash;`).
