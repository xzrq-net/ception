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

echo "== spawn (trivial prompt) =="
ception spawn --label "$label" --cwd "$scratch" \
  "Reply with exactly the single word: pong. Do not run any commands."

echo
echo "== send follow-up on the same thread =="
ception send "$label" --cwd "$scratch" \
  "Reply with exactly the single word: pong2. Do not run any commands."

echo
echo "== goal: codex drives its own turns from the objective =="
ception goal "$label" --cwd "$scratch" \
  "Create a file named done.txt in the working directory whose only content is the word ok. Once that file exists with that content the objective is met and nothing further is needed. Do not do anything else."
test -f "$scratch/done.txt" || { echo "goal did not produce done.txt"; exit 1; }

echo
echo "== goal --pause / --show / --resume =="
ception goal "$label" --cwd "$scratch" --pause
ception goal "$label" --cwd "$scratch" --show
ception goal "$label" --cwd "$scratch" --resume
ception goal "$label" --cwd "$scratch" --clear

echo
echo "== list =="
ception list --cwd "$scratch"

echo
echo "== kill =="
ception kill "$label" --cwd "$scratch"

echo
echo "smoke OK"
