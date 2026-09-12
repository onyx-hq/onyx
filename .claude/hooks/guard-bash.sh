#!/usr/bin/env bash
# .claude/hooks/guard-bash.sh — PreToolUse(Bash) guard for CLAUDE.md's build discipline.
# Permission rules match a command PREFIX only, so they cannot see a flag in the middle
# of a command (`cargo build -p oxy --release`). Those rules live here; the prefix-shaped
# ones (npm/npx/yarn/cargo test) stay in permissions.deny where they cost no subprocess.
#
# Every rule anchors to a COMMAND POSITION — a segment's first word, after env assignments.
# Matching the raw string instead blocks anything that merely MENTIONS a flag: a commit
# message, a doc edit, a grep pattern. That cost a real commit before this was fixed.
#
# Silence = allow. Fails open if jq is missing: this is discipline, not a security boundary,
# and `bash -c "cargo build --release"` slips past it by design rather than by oversight.
set -uo pipefail

command -v jq >/dev/null 2>&1 || exit 0
cmd=$(jq -r '.tool_input.command // empty' 2>/dev/null) || exit 0
[ -n "$cmd" ] || exit 0

deny() {
  jq -nc --arg r "$1" '{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason:$r}}'
  exit 0
}

# Flag present as a whole word, not as a substring of some longer token.
has_flag() { [[ $1 =~ (^|[[:space:]])${2}([[:space:]]|$) ]]; }

NO_LOCAL="Never run oxy with --local — default to --enterprise, or 'just up' for a dev stack (skill: oxy-run-and-verify)."

while IFS= read -r seg; do
  seg=${seg#"${seg%%[![:space:]]*}"} # ltrim
  while [[ $seg =~ ^[A-Za-z_][A-Za-z0-9_]*=[^[:space:]]*[[:space:]]+ ]]; do
    seg=${seg#"${BASH_REMATCH[0]}"} # drop leading VAR=val assignments
  done
  argv0=${seg%%[[:space:]]*}
  rest=${seg#"$argv0"}

  case "${argv0##*/}" in
  cargo)
    has_flag "$seg" --release &&
      deny "CLAUDE.md: never build --release locally or in CI checks — debug only. Drop the flag."
    has_flag "$seg" --local && deny "$NO_LOCAL"
    ;;
  oxy)
    has_flag "$seg" --local && deny "$NO_LOCAL"
    ;;
  rm)
    [[ $rest =~ (^|[[:space:]])-[a-zA-Z]*[rR][a-zA-Z]*([[:space:]]|$) ]] &&
      [[ $rest =~ (^|[[:space:]])[^[:space:]]*target([[:space:]/]|$) ]] &&
      deny "Use 'cargo clean' to drop build artifacts, never 'rm -rf target'."
    ;;
  esac
done < <(printf '%s\n' "$cmd" | tr ';|&' '\n')

exit 0
