#!/usr/bin/env bash
# Sync the deployed clone (~/.claude/skills/ception) to this repo's master.
# Deploying a new rev is: advance master (jj bookmark set master -r REV), then
# run this. Running daemons keep the code they loaded; new spawns get the new rev.
set -euo pipefail

deploy="${CEPTION_DEPLOY_DIR:-$HOME/.claude/skills/ception}"

if [[ -n "$(git -C "$deploy" status --porcelain)" ]]; then
  echo "deployed clone is dirty; resolve by hand:"
  git -C "$deploy" status --short
  exit 1
fi

git -C "$deploy" fetch -q origin
before="$(git -C "$deploy" rev-parse HEAD)"
target="$(git -C "$deploy" rev-parse origin/master)"
if [[ "$before" == "$target" ]]; then
  echo "already deployed: $(git -C "$deploy" log -1 --oneline)"
  exit 0
fi

git -C "$deploy" reset -q --hard origin/master
if ! out="$(cd "$deploy" && npm test 2>&1)"; then
  echo "$out" | tail -20
  echo "tests FAIL at deployed rev; previous was ${before:0:12} (git reset --hard $before to roll back)"
  exit 1
fi
echo "deployed: $(git -C "$deploy" log -1 --oneline) (was ${before:0:12})"
