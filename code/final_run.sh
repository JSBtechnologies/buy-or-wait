#!/usr/bin/env bash
# The one-command final submission run (PLAN.md §5 Phase 3): a cold run produces the
# submitted output.csv and usage_report.md; a warm rerun must reproduce them byte-for-byte
# (determinism check) and reports the cache hit rate; then signoff gates the shipped files,
# and code.zip is rebuilt from the exact commit that produced them.
#
# Run from `code/`: `bash final_run.sh`
set -euo pipefail
cd "$(dirname "$0")"

export CARGO_TARGET_DIR=target

echo "== cold run (produces the submitted output.csv + usage_report.md) =="
cargo run --release -- --cold
cp ../output.csv ../output.cold.csv
cp evaluation/usage_report.md evaluation/usage_report.cold.md

echo "== warm rerun (determinism + cache-hit-rate check only) =="
cargo run --release

echo "== determinism check: cold vs warm output.csv =="
determinism_ok=1
if cmp -s ../output.cold.csv ../output.csv; then
    echo "byte-identical: PASS"
else
    echo "byte-identical: FAIL (cold and warm runs produced different output.csv)" >&2
    determinism_ok=0
fi

echo "== restoring the cold run's output.csv and usage_report.md as the shipped files =="
mv ../output.cold.csv ../output.csv
mv evaluation/usage_report.cold.md evaluation/usage_report.md

if [ "$determinism_ok" -ne 1 ]; then
    echo "aborting before signoff/zip: fix the non-determinism first" >&2
    exit 1
fi

echo "== verify signoff (against the cold run's output.csv + usage_report.md) =="
cargo run --release -- verify signoff --output ../output.csv --usage evaluation/usage_report.md

echo "== build code.zip =="
mkdir -p ../dist
git -C .. archive --format=zip -o dist/code.zip HEAD -- code
echo "done: ../dist/code.zip ($(wc -c < ../dist/code.zip) bytes)"
