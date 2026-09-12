#!/usr/bin/env bash
# .claude/hooks/code-review-graph.sh <update|status>
#   update — after Edit/Write; --skip-flows, so flow/community data lags a full update
#   status — at session start; stdout is injected into Claude's context
# Drains stdin first: hooks are fed JSON there and would otherwise SIGPIPE the caller.
# Exits 0 when the tool or a git repo is missing — this index is advisory, never a gate.
set -uo pipefail

cat >/dev/null || true

command -v code-review-graph >/dev/null 2>&1 || exit 0
repo=$(git rev-parse --show-toplevel 2>/dev/null) || exit 0
[ -n "$repo" ] || exit 0

case "${1:-status}" in
update) code-review-graph update --skip-flows --repo "$repo" || true ;;
status) code-review-graph status --repo "$repo" || true ;;
*)
  echo "usage: ${0##*/} <update|status>" >&2
  exit 2
  ;;
esac
