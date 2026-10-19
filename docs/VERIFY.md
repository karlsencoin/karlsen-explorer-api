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

## How `/blocks/{hash}` is served

Same model as the Kaspa REST server (`kaspa-ng/kaspa-rest-server`) and the original
Karlsen REST server: **karlsend first, database as fallback.** All four captures came
from the node (41 parent levels and `mass: 172` exist nowhere else).

- **Node path** (`tests/node.rs`): a mock karlsend replays every capture as gRPC
  messages; the response must equal the capture exactly, with the one intended
  difference below.
- **Database path** (`routes/blocks.rs` unit tests, `tests/blocks.rs`): used when the
  node does not have the block body (pruned, header only) or does not answer within
  `NODE_TIMEOUT_MS`. Marked with `X-Data-Source: Database`, as in Kaspa.

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
- `includeColor=true`: `extra.color` is `"blue"` for a chain block.

## Fixed after comparing

- `extra` (`{color, minerAddress, minerInfo}`) is always present; it was missing.
- `transactions` is `null` with `includeTransactions=false`; it used to be omitted.
- `includeColor=true` is implemented (it returned 501). Chain blocks are `"blue"`;
  otherwise the node is asked (`getCurrentBlockColor`), or on the database path the
  block's future is searched (up to 64 blocks) for the chain block whose merge set
  lists it. `null` when it cannot be decided yet. Kaspa's database lookup checks
  only direct children; a merging chain block can sit deeper.
- Database path: `parents[0]` lists the selected parent first (legacy order); other
  parents and `childrenHashes` are sorted by hash for stable output.

## Intended difference from legacy

- **`extra.minerAddress` / `extra.minerInfo`** are decoded from the coinbase payload
  (as Kaspa does). Legacy `rest-v2` returns `null` for both, most likely because its
  address helper does not accept the `karlsen` prefix (the HRP bug seen elsewhere).
  Example: `karlsen:qpeusc8t…` / `3.1.0/WoolyPooly US1-kls-1`. Only filled when the
  response includes transactions (also as in Kaspa).

## Known gaps on the database path only

Kaspa's database fallback has the same two gaps, because the shared schema does not
store this data. On the node path both match legacy.

- **`header.parents`**: only level 0 is stored (`block_parent`), so one entry instead
  of one per DAG level (41 in the captures). Both explorers display only `parents[0]`.
- **Transaction `mass`**: one column is stored (the compute mass, 2036 in the capture);
  the node also reports the storage mass (172). The database path returns the stored
  value for both `mass` and `computeMass`. The explorer reads `mass` from
  `/transactions`, not from `/blocks`.

## Still unverified

- Real karlsend gRPC behaviour of `getCurrentBlockColor` for a red block, and of
  `getBlock` for a pruned block (expected: error or `isHeaderOnly`, both handled as
  "not in node").
- `header.blueWork` with an odd number of hex digits (leading zero nibble): all captured
  values are even-length.
- Order of several regular transactions in one block on the database path (coinbase
  first, then by id; the captures hold at most one regular transaction).
- Non-zero `sequence`, `lockTime`, `gas` or transaction `version`.
- Outputs that are not plain `pubkey` scripts (`scripthash`, `pubkeyecdsa`, non-standard)
  and the exact `scriptPublicKeyType` strings for them.
