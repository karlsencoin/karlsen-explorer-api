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
"""

import json
import sys
import urllib.error
import urllib.request

LEGACY = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8003"
NEW = sys.argv[2] if len(sys.argv) > 2 else "http://127.0.0.1:8013"
DEPTH = 12  # compare blocks this far behind the tip, so they no longer change
ADDRESSES = [
    "karlsen:qrc9rgx9x8gyekvrd7533hj3tl0cxd5r9ggxkg5l4a6xgllethrtuvp48fc3n",
    "karlsen:qzsflg24vlkuhqzgjm2v39rkmhjg8e3qwtaylccpxqu97n72vx0s7faphh8a7",
]

results = {"pass": 0, "fail": 0}


def call(base, path, body=None):
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
    for attempt in range(2):  # live data can move between the two calls; retry once
        ls, lv = call(LEGACY, path, body)
        ns, nv = call(NEW, path, body)
        if normalise:
            lv, nv = normalise(lv, nv)
        if expect_status is not None:
            ok, why = ns == expect_status, f"new status {ns}, expected {expect_status} (legacy {ls})"
        else:
            diff = first_diff(lv, nv) if ls == ns else f"status legacy={ls} new={ns}"
            ok, why = diff is None, diff
        if ok:
            break
    results["pass" if ok else "fail"] += 1
    print(f"{'PASS' if ok else 'FAIL'}  {name}" + ("" if ok else f"\n      {why}"))
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
    for a in addrs:
        check(f"balance {a[:20]}", f"/addresses/{a}/balance")
        check(f"utxos {a[:20]}", f"/addresses/{a}/utxos", normalise=norm_utxos)
        check(f"tx count {a[:20]}", f"/addresses/{a}/transactions-count")
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

    print("-- intended differences (new behaviour checked, legacy shown)")
    bad = ADDRESSES[0][:-1] + ("q" if ADDRESSES[0][-1] != "q" else "p")
    check("bad checksum is 422", f"/addresses/{bad}/balance", expect_status=422)

    print(f"\n{results['pass']} passed, {results['fail']} failed")
    sys.exit(1 if results["fail"] else 0)


if __name__ == "__main__":
    main()
