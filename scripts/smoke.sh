#!/usr/bin/env bash
# Manual end-to-end smoke test against real codex (npx -y @openai/codex).
# Requires codex auth. Run by hand; not part of `npm test`.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
scratch="$(mktemp -d)"
label="smoke-$$"
trap 'node "$repo/bin/ception.mjs" kill "$label" --cwd "$scratch" >/dev/null 2>&1 || true; rm -rf "$scratch"' EXIT

ception() {
  node "$repo/bin/ception.mjs" "$@"
}

echo "== spawn (read-only, trivial prompt) =="
ception spawn --label "$label" --cwd "$scratch" --read-only \
  "Reply with exactly the single word: pong. Do not run any commands."

echo
echo "== send follow-up on the same thread =="
ception send "$label" --cwd "$scratch" \
  "Reply with exactly the single word: pong2. Do not run any commands."

echo
echo "== list =="
ception list --cwd "$scratch"

echo
echo "== kill =="
ception kill "$label" --cwd "$scratch"

echo
echo "smoke OK"
