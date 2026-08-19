#!/usr/bin/env bash
# Sync the upstream zeronsh/comet repo into this fork.
#
# Strategy (simple, works for a single-person fork):
#   1. Fetch upstream
#   2. Merge upstream/main into local main (fast-forward only — main is
#      the upstream mirror and must never carry local-only commits)
#   3. Merge main into custom (resolve conflicts here)
#
# Usage:
#   ./scripts/sync-upstream.sh          # fetch + merge
#   ./scripts/sync-upstream.sh --check  # only report if upstream has new commits
set -euo pipefail

cd "$(dirname "$0")/.."

# Ensure remotes exist (first-run guard).
git remote get-url upstream &>/dev/null || {
  echo "Missing 'upstream' remote. Add it with:"
  echo "  git remote add upstream git@github.com:zeronsh/comet.git"
  exit 1
}

echo "=== fetching upstream ==="
git fetch upstream

UPSTREAM_HEAD=$(git rev-parse upstream/main)
LOCAL_HEAD=$(git rev-parse main)
CUSTOM_HEAD=$(git rev-parse custom)

if [ "$UPSTREAM_HEAD" = "$LOCAL_HEAD" ]; then
  echo "=== main is already at upstream/main ($(git rev-parse --short upstream/main)) ==="
else
  echo "=== upstream/main: $(git rev-parse --short upstream/main) ==="
  echo "=== local main:    $(git rev-parse --short main) ==="
fi

if [ "${1:-}" = "--check" ]; then
  if [ "$UPSTREAM_HEAD" != "$LOCAL_HEAD" ]; then
    echo "upstream has new commits"
    exit 1
  fi
  echo "up to date"
  exit 0
fi

# Step 1: fast-forward main to upstream/main.
echo ""
echo "=== fast-forwarding main to upstream/main ==="
git checkout main
git merge --ff-only upstream/main

# Step 2: merge main into custom.
echo ""
echo "=== merging main into custom ==="
git checkout custom
if git merge --no-edit main; then
  echo ""
  echo "=== sync complete ==="
  echo "upstream/main → main ✓"
  echo "main → custom ✓"
else
  echo ""
  echo "=== CONFLICTS DETECTED ==="
  echo "Resolve them in the working tree, then:"
  echo "  git add -A && git commit"
  echo ""
  echo "Conflicting files:"
  git diff --name-only --diff-filter=U
  exit 2
fi
