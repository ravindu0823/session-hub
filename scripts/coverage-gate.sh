#!/usr/bin/env bash
# Per-file gate for hub-core: 85% lines, 75% functions (platform.rs excluded).
# Fails closed: every hub-core source file that defines a function must be in the report (files
# with only `mod`/`use`/consts have no coverage data and are not expected).
set -euo pipefail
cd "$(dirname "$0")/.."
root=$(pwd)
json=$(cargo llvm-cov --locked -p hub-core --json --summary-only)
files=$(printf '%s' "$json" | jq -r --arg root "$root/" '
  .data[0].files[] | .filename | ltrimstr($root)
  | select(startswith("crates/hub-core/src/")) | select(endswith("/platform.rs") | not)' | sort)
expected=$(grep -rlE '^[[:space:]]*(pub(\([a-z]+\))? )?(const )?(unsafe )?fn ' crates/hub-core/src \
  | grep -v '/platform\.rs$' | sort)
if [ -z "$files" ]; then
  echo "coverage gate: no hub-core source file in the report" >&2
  exit 1
fi
missing=$(comm -23 <(printf '%s\n' "$expected") <(printf '%s\n' "$files"))
if [ -n "$missing" ]; then
  echo "coverage gate: hub-core files missing from the report:" >&2
  echo "$missing" >&2
  exit 1
fi
bad=$(printf '%s' "$json" | jq -r '
  .data[0].files[]
  | select(.filename | test("crates/hub-core/src/")) | select(.filename | test("/platform\\.rs$") | not)
  | select(.summary.lines.percent < 85 or .summary.functions.percent < 75)
  | "\(.filename): lines \(.summary.lines.percent)% functions \(.summary.functions.percent)%"')
if [ -n "$bad" ]; then
  echo "coverage below gate (85% lines, 75% functions):" >&2
  echo "$bad" >&2
  exit 1
fi
echo "coverage gate ok ($(printf '%s\n' "$files" | wc -l | tr -d ' ') files)"
