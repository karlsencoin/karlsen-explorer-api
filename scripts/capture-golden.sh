#!/bin/sh
# Capture responses from a running API for later comparison.
# Usage: capture-golden.sh BASE_URL OUT_DIR PATH [PATH...]
set -eu
base="$1"; out="$2"; shift 2
mkdir -p "$out"
for p in "$@"; do
  f="$out/$(printf '%s' "$p" | tr '/?&=:' '_____').json"
  curl -sS --max-time 20 "$base$p" -o "$f" -w "%{http_code} $p\n"
done
