#!/usr/bin/env bash
# 100% line coverage for the pure modules (admit, weight, key, eta), measured
# from the unit tests alone so the gate cannot be satisfied by accident
# through the end-to-end binary tests.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo llvm-cov --bin canirun --json --output-path target/llvm-cov.json \
  -- admit:: weight:: key:: eta:: >/dev/null
python3 - <<'PY'
import json, sys
data = json.load(open("target/llvm-cov.json"))
want = {"src/admit.rs", "src/weight.rs", "src/key.rs", "src/eta.rs"}
seen = set()
bad = []
for f in data["data"][0]["files"]:
    name = f["filename"]
    rel = name[name.find("src/"):] if "src/" in name else name
    if rel not in want:
        continue
    seen.add(rel)
    lines = f["summary"]["lines"]
    pct = lines["percent"]
    print(f"{rel}: {lines['covered']}/{lines['count']} lines ({pct:.1f}%)")
    if lines["covered"] != lines["count"]:
        bad.append(rel)
missing = want - seen
if missing:
    print("not measured:", ", ".join(sorted(missing)))
    sys.exit(1)
if bad:
    print("below 100%:", ", ".join(bad))
    sys.exit(1)
PY
