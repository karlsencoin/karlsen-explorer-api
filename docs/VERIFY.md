# Verify against live `rest-v2` before cut-over

The schema does not say how some legacy fields are formatted, so each assumption is
compared with a real response. Captures live in `crates/explorer-api/tests/fixtures/`
(all taken from the legacy API on 2026-10-18); `scripts/capture-golden.sh` fetches more.

| Fixture | Request | Block |
| --- | --- | --- |
| `golden-block.json` | `includeTransactions=true` | tip block, coinbase only |
| `golden-block-with-txs.json` | `includeTransactions=true` | chain block, coinbase + 1 regular transaction |
| `golden-block-color.json` | `includeColor=true` | same block |
| `golden-block-notx.json` | `includeTransactions=false` | same block |

The unit tests in `routes/blocks.rs` rebuild the response from each fixture, using the
same functions as the handler, and compare it with the capture field by field.

## Confirmed

- `verboseData.difficulty` = `2^255 / target(bits)` (two blocks, identical to the legacy value).
- Types: `timestamp`, `daaScore`, `blueScore`, `nonce`, `blueWork` are strings; `bits`,
  `amount`, `verboseData.blockTime`, `mass`, `computeMass` are numbers.
- `header.nonce` is a decimal string and `blocks.nonce` is big-endian: block `e5ed608d…`
  stores `70078f6bb67ce81a`, and legacy returns `8072578549870815258` for it.
- `header.blueWork` is lowercase hex without leading zeros; the stored `blocks.blue_work`
  is already trimmed.
- `scriptPublicKey.version` is `0`; `scriptPublicKeyType` is `pubkey`; the address
  derived from the raw script equals the legacy address.
- `lockTime`, `gas` and `version` are `0` (coinbase and regular transaction); input
  `sequence` is `0`.
- `isChainBlock` is `false` for a tip block and `true` for an accepted chain block.
- The coinbase comes first in `transactionIds` / `transactions`.
- `includeTransactions=false`: `transactions` is present and `null`.
- `includeColor=true`: `extra.color` is `"blue"`; `minerAddress` and `minerInfo` stay `null`.

## Fixed after comparing

- `extra` (`{color, minerAddress, minerInfo}`) is always present; it was missing.
- `transactions` is `null` with `includeTransactions=false`; it used to be omitted.
- `parents[0]` lists the selected parent first (legacy order). The remaining parents are
  sorted by hash and `childrenHashes` is sorted by hash; the legacy order for those is
  not reproducible from the database, so callers must not depend on it.

## Known deviations

- **`header.parents`**: legacy returns one `parentHashes` entry per DAG level (41 in every
  capture). The database stores only level 0 (`block_parent`), so this API returns a single
  entry. Higher levels can only come from the node, which suggests legacy reads this
  endpoint from the node rather than from Postgres. Open decision: accept the single
  level, or add a node-backed path for `/blocks/{hash}`.
- **Transaction `mass`**: for the regular transaction legacy returns `mass: 172` and
  `computeMass: 2036`. The schema has a single `mass` column and this API emits it for
  both fields, so `mass` differs (it is probably the storage mass, which is not stored).
  Coinbase transactions have `0` for both and match.
- **`includeColor=true`** is answered with 501. Legacy returns `"blue"` or `"red"`, most
  likely from a node call; deriving it from the database would need a lookup in
  `merge_set_blues_hashes` / `merge_set_reds_hashes` of the accepting chain block.

## Still unverified

- `header.blueWork` with an odd number of hex digits (leading zero nibble): all captured
  values are even-length, so the two trimming rules cannot be told apart.
- Order of several regular transactions in one block (currently coinbase first, then by
  id; the captures hold at most one regular transaction).
- Non-zero `sequence`, `lockTime`, `gas` or transaction `version`.
- Outputs that are not plain `pubkey` scripts (`scripthash`, `pubkeyecdsa`, non-standard)
  and the exact `scriptPublicKeyType` strings for them.
- `isChainBlock` from `transactions_acceptances` for chain blocks whose acceptance data is
  empty.
