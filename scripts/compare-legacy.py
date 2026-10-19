#!/usr/bin/env python3
"""Compare the new API with the legacy rest-v2 on live data.

Usage: compare-legacy.py [LEGACY_BASE] [NEW_BASE]
       (defaults: http://127.0.0.1:8003 http://127.0.0.1:8013)

Sends the same requests to both services and reports differences. Differences that
are intended (see docs/VERIFY.md) are normalised away before comparing:
  - legacy drops the "karlsen:" prefix in some transaction address fields
  - legacy leaves extra.minerAddress / extra.minerInfo null
  - UTXO lists come back in node order, which changes between calls
Only the Python standard library is used; nothing is written to disk.

The mining pool address (the coinbase address picked below) has 200k+ UTXOs: its
UTXO list is skipped unless HEAVY=1 is set, because loading it in legacy, the new
API and this script at once can exhaust the VPS memory.
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

LEGACY = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8003"
NEW = sys.argv[2] if len(sys.argv) > 2 else "http://127.0.0.1:8013"
DEPTH = 12  # compare blocks this far behind the tip, so they no longer change
ADDRESSES = [
    "karlsen:qrc9rgx9x8gyekvrd7533hj3tl0cxd5r9ggxkg5l4a6xgllethrtuvp48fc3n",
    "karlsen:qzsflg24vlkuhqzgjm2v39rkmhjg8e3qwtaylccpxqu97n72vx0s7faphh8a7",
]

results = {"pass": 0, "fail": 0, "live": 0}
SLOW_MS = 1000


def call(base, path, body=None):
    status, value, _ = timed_call(base, path, body)
    return status, value


def timed_call(base, path, body=None):
    t0 = time.monotonic()
    status, value = _call(base, path, body)
    return status, value, int((time.monotonic() - t0) * 1000)


def _call(base, path, body=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(base + path, data=data, method="POST" if data else "GET")
    if data:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=60) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, raw.decode(errors="replace")


def first_diff(a, b, path="$"):
    if type(a) is not type(b):
        return f"{path}: legacy={json.dumps(a)[:120]} new={json.dumps(b)[:120]}"
    if isinstance(a, dict):
        for k in list(a) + [k for k in b if k not in a]:
            if k not in a or k not in b:
                return f"{path}.{k}: only in {'new' if k not in a else 'legacy'}"
            d = first_diff(a[k], b[k], f"{path}.{k}")
            if d:
                return d
        return None
    if isinstance(a, list):
        if len(a) != len(b):
            return f"{path}: length legacy={len(a)} new={len(b)}"
        for i, (x, y) in enumerate(zip(a, b)):
            d = first_diff(x, y, f"{path}[{i}]")
            if d:
                return d
        return None
    return None if a == b else f"{path}: legacy={json.dumps(a)[:120]} new={json.dumps(b)[:120]}"


def prefix_addresses(v):
    if isinstance(v, dict):
        for k, x in v.items():
            if k in ("script_public_key_address", "previous_outpoint_address") and isinstance(x, str):
                if not x.startswith("karlsen:"):
                    v[k] = "karlsen:" + x
            else:
                prefix_addresses(x)
    elif isinstance(v, list):
        for x in v:
            prefix_addresses(x)
    return v


def check(name, path, body=None, normalise=None, expect_status=None):
    """PASS / FAIL, or LIVE when legacy itself changed while checking (busy address)."""
    verdict = "FAIL"
    for attempt in range(2):  # retry once in case data moved between the calls
        ls, lv, lms = timed_call(LEGACY, path, body)
        ns, nv, nms = timed_call(NEW, path, body)
        if normalise:
            lv, nv = normalise(lv, nv)
        if expect_status is not None:
            ok, why = ns == expect_status, f"new status {ns}, expected {expect_status} (legacy {ls})"
        else:
            diff = first_diff(lv, nv) if ls == ns else f"status legacy={ls} new={ns}"
            ok, why = diff is None, diff
        if ok:
            verdict = "PASS"
            break
    if verdict == "FAIL" and expect_status is None:
        # Ask legacy again: if its own answer moved, the data is live, not wrong.
        ls2, lv2 = call(LEGACY, path, body)
        if normalise:
            lv2, _ = normalise(lv2, nv)
        if ls2 == ls and first_diff(lv, lv2) is not None:
            verdict = "LIVE"
            why = f"data changed during the check (legacy before vs after: {first_diff(lv, lv2)})"
    results[verdict.lower()] += 1
    timing = f"legacy {lms} ms, new {nms} ms"
    slow = " SLOW" if max(lms, nms) >= SLOW_MS else ""
    print(f"{verdict:<5} {name}  ({timing}){slow}" + ("" if verdict == "PASS" else f"\n      {why}"))
    return nv


def norm_block(lv, nv):
    for v in (lv, nv):
        if isinstance(v, dict) and isinstance(v.get("extra"), dict):
            if lv.get("extra", {}).get("minerAddress") is None:
                v["extra"]["minerAddress"] = v["extra"]["minerInfo"] = None
    return lv, nv


def norm_utxos(lv, nv):
    key = lambda u: (u["outpoint"]["transactionId"], u["outpoint"].get("index", 0))
    if isinstance(lv, list) and isinstance(nv, list):
        return sorted(lv, key=key), sorted(nv, key=key)
    return lv, nv


def norm_halving(lv, nv):
    # The timestamp is "now + remaining time": compare it to the minute.
    for v in (lv, nv):
        if isinstance(v, dict):
            v["nextHalvingTimestamp"] = int(v.get("nextHalvingTimestamp", 0)) // 60
            v["nextHalvingDate"] = str(v.get("nextHalvingDate", ""))[:16]
    return lv, nv


def norm_dag_window(lv, nv):
    # Legacy leaves the order of equal DAA scores to Postgres and a LIMIT can cut a
    # tie group differently: sort by (daa, hash) and drop the newest score on both.
    if not (isinstance(lv, dict) and isinstance(nv, dict)):
        return lv, nv
    top = max([b["daaScore"] for v in (lv, nv) for b in v.get("blocks", [])], default=None)
    for v in (lv, nv):
        v["blocks"] = sorted((b for b in v.get("blocks", []) if b["daaScore"] != top),
                             key=lambda b: (b["daaScore"], b["hash"]))
        v["count"] = v["newestDaa"] = v["oldestDaa"] = None
    return lv, nv


def norm_kgi(lv, nv):
    # Ages are computed at request time: compare them to the minute.
    def walk(v):
        if isinstance(v, dict):
            return {k: (x // 60 if k in ("inboundUptimeSec", "lastSeenAgoSec")
                        and isinstance(x, int) else walk(x)) for k, x in v.items()}
        if isinstance(v, list):
            return [walk(x) for x in v]
        return v
    return walk(lv), walk(nv)


def norm_rich(lv, nv):
    # Legacy left equal balances in Postgres' order; the new API sorts them by address.
    def srt(entries):
        return sorted(entries, key=lambda e: (-e["amount"], e["address"]))
    for v in (lv, nv):
        if isinstance(v, dict) and isinstance(v.get("top_addresses"), list):
            v["top_addresses"] = srt(v["top_addresses"])
    if isinstance(lv, list) and isinstance(nv, list):
        return srt(lv), srt(nv)
    return lv, nv


def norm_blocks_list(lv, nv):
    # getBlocks runs up to the moving tip: compare the first few blocks only.
    for v in (lv, nv):
        if isinstance(v, dict):
            v["blockHashes"] = (v.get("blockHashes") or [])[:5]
            v["blocks"] = (v.get("blocks") or [])[:5]
    return lv, nv


def norm_count(lv, nv):
    # Big counts are cached in memory up to 10 minutes: a few new transactions
    # behind legacy (at most 0.1%) is the cache working, not a mismatch.
    try:
        lt, nt = lv["total"], nv["total"]
    except (TypeError, KeyError):
        return lv, nv
    if 0 < lt - nt <= max(1, lt // 1000) and lt >= 100_000:
        print(f"      cached count: new is {lt - nt} behind legacy (expected)")
        nv = dict(nv, total=lt)
    return lv, nv


def norm_tx(lv, nv):
    return prefix_addresses(lv), nv


def main():
    print(f"legacy: {LEGACY}\nnew:    {NEW}\n")
    _, dag = call(LEGACY, "/info/blockdag")
    h = dag["tipHashes"][0]
    blocks, tx_ids = [], []
    for _ in range(DEPTH + 30):  # walk back along selected parents
        _, b = call(LEGACY, f"/blocks/{h}?includeTransactions=true")
        blocks.append(b)
        h = b["verboseData"]["selectedParentHash"]
    for b in blocks[DEPTH:]:
        tx_ids += [t["verboseData"]["transactionId"] for t in b.get("transactions") or []]

    print("-- blocks")
    for b in blocks[DEPTH:DEPTH + 5]:
        bh = b["verboseData"]["hash"]
        check(f"block {bh[:12]}", f"/blocks/{bh}", normalise=norm_block)
        check(f"block {bh[:12]} no tx", f"/blocks/{bh}?includeTransactions=false", normalise=norm_block)
        check(f"block {bh[:12]} color", f"/blocks/{bh}?includeColor=true", normalise=norm_block)
    merged = [m for b in blocks[DEPTH:DEPTH + 5] for m in b["verboseData"]["mergeSetBluesHashes"]
              + b["verboseData"]["mergeSetRedsHashes"]]
    for m in merged[:5]:
        check(f"merged block {m[:12]} color", f"/blocks/{m}?includeColor=true", normalise=norm_block)

    print("-- addresses")
    miner = (blocks[DEPTH].get("transactions") or [{}])[0]
    miner_addr = ((miner.get("outputs") or [{}])[0].get("verboseData") or {}).get("scriptPublicKeyAddress")
    addrs = ADDRESSES + ([miner_addr] if miner_addr else [])
    heavy = os.environ.get("HEAVY") == "1"
    for a in addrs:
        check(f"balance {a[:20]}", f"/addresses/{a}/balance")
        if a == miner_addr and not heavy:
            print(f"SKIP  utxos {a[:20]}  (pool address, 200k+ UTXOs; set HEAVY=1 to include)")
        else:
            check(f"utxos {a[:20]}", f"/addresses/{a}/utxos", normalise=norm_utxos)
        check(f"tx count {a[:20]}", f"/addresses/{a}/transactions-count", normalise=norm_count)
        for q in ("limit=5", "limit=5&offset=5", "limit=3&resolve_previous_outpoints=light",
                  "limit=2&resolve_previous_outpoints=full", "limit=5&fields=transaction_id,block_time"):
            check(f"full-tx {a[:20]} {q}", f"/addresses/{a}/full-transactions?{q}", normalise=norm_tx)
    check("POST balances", "/addresses/balances", {"addresses": addrs})
    check("POST utxos", "/addresses/utxos", {"addresses": ADDRESSES[:1]}, normalise=norm_utxos)

    print("-- transactions")
    for t in tx_ids[:10]:
        check(f"tx {t[:12]}", f"/transactions/{t}", normalise=norm_tx)
    check("search by ids", "/transactions/search", {"transactionIds": tx_ids[:20]}, normalise=norm_tx)
    bs = int(blocks[DEPTH]["header"]["blueScore"])
    check("search by blue score", "/transactions/search",
          {"acceptingBlueScores": {"gte": bs - 3, "lt": bs}}, normalise=norm_tx)

    print("-- info (node-backed; tip values move, so some checks may show LIVE)")
    for path in ("/info/blockdag", "/info/network", "/info/hashrate", "/info/hashrate?stringOnly=true",
                 "/info/coinsupply", "/info/coinsupply/circulating", "/info/coinsupply/circulating?in_billion=true",
                 "/info/coinsupply/total", "/info/blockreward", "/info/blockreward?stringOnly=true",
                 "/info/karlsend", "/info/halving?field=nextHalvingAmount",
                 # Price data is cached up to 60 s on both sides, at different moments:
                 # a mismatch here right after a price move is expected.
                 "/info/price", "/info/price?stringOnly=true", "/info/market-data"):
        check(path, path)
    check("/info/halving (amount, date to the minute)", "/info/halving", normalise=norm_halving)

    print("-- dag-window (database)")
    old_daa = int(blocks[-1]["header"]["daaScore"]) - 10
    check("dag-window since_daa (deep, 50 blocks)", f"/info/dag-window?since_daa={old_daa}&limit=50",
          normalise=norm_dag_window)
    check("dag-window shape", "/info/dag-window?since_daa=999999999999")

    print("-- miners (KGI tracker; refreshed every snapshot, so LIVE is possible)")
    check("/info/miners/network", "/info/miners/network")
    top = check("/info/miners/top", "/info/miners/top")
    check("/info/miners/top?limit=5&recent_hours=0", "/info/miners/top?limit=5&recent_hours=0")
    for m in ((top or {}).get("miners") or [])[:2]:
        check(f"miner detail {m['address'][:20]}", f"/info/miners/address/{m['address']}?hours=6")

    print("-- KGI node map (scanner files; legacy caches them up to 60 s, so LIVE is possible)")
    for path in ("/info/nodes", "/info/peers/geo", "/info/seeder-nodes"):
        check(path, path, normalise=norm_kgi)

    print("-- analytics (balances table; refreshed hourly, so LIVE is possible)")
    for path in ("/analytics/addresses/top", "/analytics/addresses/top?limit=50&offset=20",
                 "/analytics/addresses/range?min_amount=100000&max_amount=1000000"):
        check(path, path, normalise=norm_rich)
    for path in ("/analytics/addresses/total", "/analytics/addresses/distribution-summary",
                 "/analytics/addresses/distribution?min_amount=1000&max_amount=10000",
                 "/analytics/addresses/distribution?min_amount=10000000"):
        check(path, path)
    for a in ADDRESSES:
        check(f"history {a[:20]}", f"/analytics/addresses/{a}/history?limit=10")

    print("-- GET /blocks (node getBlocks; first 5 blocks after lowHash)")
    low = blocks[DEPTH + 5]["verboseData"]["hash"]
    check("blocks hashes only", f"/blocks?lowHash={low}", normalise=norm_blocks_list)
    check("blocks with blocks", f"/blocks?lowHash={low}&includeBlocks=true",
          normalise=norm_blocks_list)
    check("blocks with transactions",
          f"/blocks?lowHash={low}&includeBlocks=true&includeTransactions=true",
          normalise=norm_blocks_list)

    print("-- intended differences (new behaviour checked, legacy shown)")
    bad = ADDRESSES[0][:-1] + ("q" if ADDRESSES[0][-1] != "q" else "p")
    check("bad checksum is 422", f"/addresses/{bad}/balance", expect_status=422)

    print(f"\n{results['pass']} passed, {results['fail']} failed, "
          f"{results['live']} live (data moved while checking; not a mismatch)")
    sys.exit(1 if results["fail"] else 0)


if __name__ == "__main__":
    main()
