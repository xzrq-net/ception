#!/usr/bin/env bash
# Manual end-to-end smoke test against real codex (npx -y @openai/codex).
# Requires codex auth. Run by hand; not part of `cargo test`. Uses
# CEPTION_BIN if set, else the debug build.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d)"
label="smoke-$$"
bin="${CEPTION_BIN:-$repo/target/debug/ception}"
trap '"$bin" kill "$label" --cwd "$scratch" >/dev/null 2>&1 || true; rm -rf "$scratch"' EXIT

ception() {
  "$bin" "$@"
}

echo "== spawn (trivial prompt) =="
ception spawn "$label" --cwd "$scratch" \
  "Reply with exactly the single word: pong. Do not run any commands."

echo
echo "== send follow-up on the same thread =="
ception send "$label" --cwd "$scratch" \
  "Reply with exactly the single word: pong2. Do not run any commands."

echo
echo "== goals are off: codex has no goal tools =="
out="$(ception send "$label" --cwd "$scratch" \
  "Is a tool named create_goal available to you right now? Reply with exactly one word, yes or no. Do not run any commands.")"
echo "$out"
grep -qiE '^no\.?$' <<<"$out" || { echo "codex reports goal tools (features.goals override not applied?)"; exit 1; }

echo
echo "== list =="
ception list --cwd "$scratch"

echo
echo "== kill =="
ception kill "$label" --cwd "$scratch"

echo
echo "smoke OK"
