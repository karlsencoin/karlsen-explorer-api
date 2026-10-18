# Verify against live `rest-v2` before cut-over

The schema does not say how some legacy fields are formatted. The current
implementation follows the legacy OpenAPI document; each item below must be
compared with a real response (`scripts/capture-golden.sh`).

`GET /blocks/{hash}`:
- `header.nonce`: decimal string from big-endian bytes (assumed)
- `header.blueWork`: lowercase hex without leading zeros (assumed)
- `verboseData.difficulty`: `2^255 / target(bits)` (assumed)
- `verboseData.isChainBlock`: block appears as accepting block in `transactions_acceptances` (assumed)
- transaction order inside a block: schema stores no index; currently coinbase first, then by id
- `lockTime`, `gas`, `version`, `sequence`: not stored, emitted as 0
- `scriptPublicKey.version`: emitted as 0; the stored bytes are assumed to be the raw script
