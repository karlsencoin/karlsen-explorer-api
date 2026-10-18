# Verify against live `rest-v2` before cut-over

The schema does not say how some legacy fields are formatted, so each assumption is
compared with a real response. Captures live in `crates/explorer-api/tests/fixtures/`;
`scripts/capture-golden.sh` fetches more.

Current reference: `golden-block.json`, `GET /blocks/{hash}?includeTransactions=true`
on a tip block, captured 2026-10-18. It holds only a coinbase transaction, so
everything about regular transactions is still open (see the last section).
`routes::blocks::tests::serialization_matches_golden_wire_format` rebuilds the response
from this file and compares it with the golden, field by field.

## Confirmed by the golden block

- `verboseData.difficulty` = `2^255 / target(bits)`: bits `0x1c7133d7` gives
  `4856392896.376908`, identical to the legacy value.
- Types: `timestamp`, `daaScore`, `blueScore`, `nonce`, `blueWork` are strings; `bits`,
  `amount`, `verboseData.blockTime` are numbers.
- `header.nonce` is a decimal string, and `blocks.nonce` is big-endian: block `e5ed608d…`
  stores `70078f6bb67ce81a`, and legacy returns `8072578549870815258` for it.
- `header.blueWork` is lowercase hex without `0x` and without leading zeros.
- `scriptPublicKey.version` is `0`; `scriptPublicKeyType` is `pubkey`; the address
  derived from the raw script equals the legacy address.
- `lockTime`, `gas`, `version`, `mass`, `computeMass` are `0` on the coinbase, and the
  coinbase has `inputs: []`.
- `isChainBlock` is `false` for a tip block.

## Corrected

- Responses always carry `extra: {"color": null, "minerAddress": null, "minerInfo": null}`
  (all `null` without `includeColor`). It was missing and is now emitted.

## Known deviation: `header.parents`

The legacy response has one `parentHashes` entry per DAG level (41 in the golden). The
database stores only level 0 (`block_parent`), so this API emits a single entry. Levels
above 0 can only come from the node, which suggests legacy reads this endpoint from the
node rather than from Postgres. Open decision: accept the single level, or add a
node-backed path for `/blocks/{hash}`.

## Still unverified (needs a block with regular transactions or database access)

- `header.blueWork` when the value has an odd number of hex digits (leading zero nibble).
  Stored `blocks.blue_work` is already trimmed (9 bytes, not padded), but the golden
  values are even-length, so they cannot tell the two trimming rules apart.
- `verboseData.isChainBlock` for accepted chain blocks (assumed: the block appears as
  `block_hash` in `transactions_acceptances`), and `childrenHashes` on a block with children.
- Transaction order inside a block: the schema stores no index; currently coinbase
  first, then by id.
- Regular transactions: input `signatureScript`, `sigOpCount`, `sequence` (emitted as
  `0`), transaction `version`, `lockTime`, `gas`, and `mass` versus
  `verboseData.computeMass`.
- `includeColor=true`: `extra.color` and whether `minerAddress` / `minerInfo` are filled
  (currently answered with 501).
- `includeTransactions=false`: whether legacy omits `transactions` or returns `null`.

## Captures still needed

Run on the legacy host (`BASE` is the legacy base URL). The script walks the selected
parent chain from the tip until it finds a block with more than one transaction, then
saves three variants:

```sh
python3 - <<'EOF'
import json, urllib.request
base = "BASE"
h = json.load(urllib.request.urlopen(base + "/info/blockdag"))["tipHashes"][0]
for _ in range(2000):
    raw = urllib.request.urlopen(f"{base}/blocks/{h}?includeTransactions=true").read()
    b = json.loads(raw)
    if len(b["transactions"]) > 1:
        open("golden-block-with-txs.json", "wb").write(raw)
        for name, q in (("color", "includeTransactions=true&includeColor=true"),
                        ("notx", "includeTransactions=false")):
            open(f"golden-block-{name}.json", "wb").write(
                urllib.request.urlopen(f"{base}/blocks/{h}?{q}").read())
        print("saved for", h, "-", len(b["transactions"]), "txs")
        break
    h = b["verboseData"]["selectedParentHash"]
else:
    print("no block with more than one transaction found")
EOF
```

And the stored bytes for the golden block (nonce / blue work endianness and trimming):

```sql
SELECT encode(nonce, 'hex') AS nonce, encode(blue_work, 'hex') AS blue_work
FROM blocks
WHERE hash = decode('ba735a630066816f939c0a1de2ed81e9975f7e75f2649a49900f3eaa2cf39234', 'hex');
```
