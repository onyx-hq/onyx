#!/usr/bin/env bash
# Run one of this repo's custom apps on your machine, against a cloud oxy's
# real data.
#
# It is two processes, and neither is guessable from the outside — which is
# why this file exists at all. Before it, running an app locally needed three
# commands in two terminals and knowledge that was written down nowhere:
#
#   oxyc proxy --env <env>    background, listens on :3000. Forwards the app's
#                             /api calls to the cloud, signed with the token
#                             `oxyc login --env <env>` cached.
#   pnpm dev  (in the app)    foreground, Vite on :5173, which proxies /api to
#                             :3000. That target is the Vite plugin's default,
#                             so the proxy's port is not really negotiable —
#                             hence no --port flag here.
#
# Started in that order and torn down together. The proxy is killed on EXIT,
# INT and TERM, because a proxy left holding :3000 after a Ctrl-C makes the
# NEXT run fail with a port conflict that names nothing anyone remembers.
#
# Usage: pnpm dev --env <env> [app] [--allow-writes] [--allow-events] [--yes]

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Discovery is NOT reimplemented here.
#
# `.github/scripts/discover-apps.sh` is this repo's one answer to "which apps
# does it hold, and where does each build land", and publish.yaml calls the
# same file — so a dev run and a publish can never disagree about what counts
# as an app. Calling a script out of .github/ from a dev script reads oddly,
# and the tidier-looking fix (move it to scripts/, repoint the workflow) trades
# that oddness for a workflow whose script path is wrong on the first push.
# The oddness is cheaper. It lives there because that is where the workflow
# that also uses it looks for it.
DISCOVER="$root/.github/scripts/discover-apps.sh"

# `oxyc proxy`'s own default, and what @oxy-hq/vite-plugin proxies /api to.
PROXY_PORT=3000
# Vite's default. Printed, never bound by this script.
DEV_PORT=5173

# --- output -----------------------------------------------------------------

# Every line this script writes goes through one of these, so a reader can
# always tell our diagnostics from the two child processes' own output.
say() { printf '%s\n' "$@"; }
die() {
  printf 'dev: %s\n' "$1" >&2
  shift
  if [ "$#" -gt 0 ]; then printf '%s\n' "$@" >&2; fi
  exit 1
}

usage() {
  cat <<EOF
Run one of this repo's custom apps locally, against a cloud oxy's real data.

  pnpm dev --env <env> [app] [--allow-writes] [--allow-events] [--yes]

  --env <env>     REQUIRED. Which oxy to proxy to. There is deliberately no
                  default: \`oxyc proxy\`'s own default is production, and a
                  \`pnpm dev\` that silently reads production is not something
                  this repo will do. One of:
                    dev  staging  production
                  a full URL (--env https://acme.oxygen-hq.com), or any name
                  the app's own oxy-app.json declares under "environments".
  [app]           Which app to run, when this repo holds more than one. Its
                  directory (apps/acme/reports) or just its name (reports).
  --allow-writes  Forward side-effecting calls instead of holding them.
  --allow-events  Forward tracking events instead of dropping them.
  --yes           Confirm that --env production is really what you meant.
  -h, --help      This text.

First run, once per env:  oxyc login --env <env>
EOF
}

# --- arguments --------------------------------------------------------------

env_name=""
app_arg=""
allow_writes=0
allow_events=0
confirm_yes=0

while [ "$#" -gt 0 ]; do
  case "$1" in
    --env)
      if [ "$#" -lt 2 ] || [ -z "$2" ]; then
        die "--env needs a value." "Run 'pnpm dev --help' for the choices."
      fi
      env_name="$2"
      shift 2
      ;;
    --env=*)
      env_name="${1#--env=}"
      if [ -z "$env_name" ]; then
        die "--env needs a value." "Run 'pnpm dev --help' for the choices."
      fi
      shift
      ;;
    --allow-writes) allow_writes=1; shift ;;
    --allow-events) allow_events=1; shift ;;
    --yes|-y) confirm_yes=1; shift ;;
    -h|--help) usage; exit 0 ;;
    --) shift ;;
    -*)
      die "unknown option: $1" "Run 'pnpm dev --help' to see what this accepts."
      ;;
    *)
      if [ -n "$app_arg" ]; then
        die "more than one app named: '$app_arg' and '$1'." \
            "This runs one app at a time."
      fi
      app_arg="$1"
      shift
      ;;
  esac
done

# --- teardown ---------------------------------------------------------------
#
# Armed BEFORE anything is started, so a temp directory is never left behind
# by an early exit either. `proxy_pid` stays empty until there is something to
# kill, and cleanup is idempotent — the INT and TERM handlers exit, which runs
# the EXIT handler on top of them.

tmpdir="$(mktemp -d)"
proxy_pid=""

# Has this child finished? An empty state means it is gone from the process
# table; `Z` means it exited and is waiting for this shell to reap it. `kill -0`
# can tell neither from a live process — it succeeds on a zombie — which is why
# this asks the table instead.
process_finished() {
  local state
  state="$(ps -o state= -p "$1" 2>/dev/null || true)"
  state="${state// /}"
  case "$state" in
    ""|Z*) return 0 ;;
  esac
  return 1
}

cleanup() {
  if [ -n "$proxy_pid" ]; then
    # The proxy is the whole reason this trap exists. TERM, then reap it, so
    # :3000 is actually released before this shell returns to the prompt.
    kill "$proxy_pid" 2>/dev/null || true

    # Bounded, and the bound is the point. A bare `wait` here is unbounded, so
    # a proxy that ignores SIGTERM holds the terminal forever — which is WORSE
    # than leaking one: the person is now stuck in their shell rather than
    # merely running a stale process they can kill later. Ask for ~5s, then
    # stop asking.
    local ticks=0
    while [ "$ticks" -lt 50 ] && ! process_finished "$proxy_pid"; do
      sleep 0.1
      ticks=$((ticks + 1))
    done
    if ! process_finished "$proxy_pid"; then
      printf 'dev: the proxy (pid %s) ignored SIGTERM for 5s; sending SIGKILL.\n' \
        "$proxy_pid" >&2
      kill -9 "$proxy_pid" 2>/dev/null || true
    fi

    wait "$proxy_pid" 2>/dev/null || true
    proxy_pid=""
  fi
  rm -rf "$tmpdir"
}

# The EXIT handler is spelled bare rather than quoted so that shellcheck can
# see cleanup is reachable (SC2329 otherwise reports it as dead code, and this
# suite treats a shellcheck finding as a failure). The signal handlers must
# exit as well as clean up: a returning INT handler would drop the shell back
# where it was, with the proxy already dead.
#
# What each of the three actually buys, measured rather than assumed — bash 3.2,
# signal delivered while this script sits in the liveness `sleep` below:
#
#   EXIT   does the work on every path tried, INCLUDING death by an untrapped
#          SIGTERM: bash runs it there too. It is the one that matters.
#   INT    load-bearing, and not for the reason it looks. A job started in the
#          background inherits SIGINT set to IGNORE, so WITHOUT this line a
#          SIGINT aimed at this script alone is discarded — the script sails on
#          and starts the dev server the person just tried to stop (measured:
#          exit 0, no teardown). Installing the trap un-ignores the signal.
#   TERM   defensive, and honestly so: every window tried already produced 143
#          and a clean teardown through EXIT. It is kept because it makes the
#          status explicit instead of leaning on bash running an EXIT handler
#          for a fatal signal, which is not a portable guarantee — but there is
#          no test for it, because none could fail.
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

# --- which apps does this repo hold? ----------------------------------------

if [ ! -f "$DISCOVER" ]; then
  die "this repo has no $DISCOVER." \
      "That script is what both this and .github/workflows/publish.yaml use to" \
      "find apps. Restore it from the customer-tooling template."
fi

discover_status=0
bash "$DISCOVER" "$root" >"$tmpdir/apps" 2>"$tmpdir/discover.err" || discover_status=$?

if [ "$discover_status" -ne 0 ]; then
  cat "$tmpdir/discover.err" >&2
  die "could not work out which apps this repo holds — see above."
fi

# discover-apps.sh speaks GitHub Actions. On the zero-app path it prints
# "publish: no custom apps under apps/ ...", which is the wrong words for a
# `pnpm dev` — this script says its own thing below. Everything else it puts
# on stderr is a real diagnostic (an app directory named `dist` that was
# skipped, say) and has to survive.
grep_status=0
grep -v '^publish: ' "$tmpdir/discover.err" >&2 || grep_status=$?
# grep exits 1 when it selects NO lines, which is the ordinary answer here:
# the only line may be the one just filtered out, or there may have been no
# stderr at all. Exactly that status is tolerated. Anything above it is grep
# itself failing, and swallowing that would hide a real diagnostic — the
# failure mode a bare `|| true` produces.
if [ "$grep_status" -gt 1 ]; then
  die "could not read the discovery diagnostics (grep exited $grep_status)."
fi

app_dirs=()
while IFS=$'\t' read -r dir _; do
  [ -n "$dir" ] || continue
  app_dirs+=("$dir")
done < "$tmpdir/apps"

# Zero apps is checked FIRST, ahead of --env and ahead of every tool check: a
# freshly scaffolded repo has none, someone will type `pnpm dev` in it on day
# one, and "here is how to add an app" is a more useful answer than a lecture
# about a flag they have nothing to use it on. Exit 0 — day one is green.
if [ "${#app_dirs[@]}" -eq 0 ]; then
  say "" \
      "dev: this repo has no custom apps yet, so there is nothing to run." \
      "" \
      "An app is a React + Vite bundle in its own directory under apps/:" \
      "" \
      "  apps/<org>/<app>/       e.g. apps/acme/sales-dashboard/" \
      "    oxy-app.json          \"slug\" and \"orgSlug\" — the app's identity" \
      "    package.json          with a \"dev\" script (Vite)" \
      "    src/…" \
      "" \
      "Scaffold one, from the repo root:" \
      "" \
      "  pnpm dlx @oxy-hq/create-oxy-app apps/<org>/<app>" \
      "  pnpm install          # writes pnpm-lock.yaml — commit it" \
      "  pnpm dev --env dev" \
      ""
  exit 0
fi

# --- which one? -------------------------------------------------------------

list_apps() {
  local d
  for d in "${app_dirs[@]}"; do
    printf '  %s\n' "$d"
  done
}

app_dir=""
if [ -n "$app_arg" ]; then
  # Tab completion adds the trailing slash, and typing the whole path is the
  # obvious thing to do; so is typing just the name.
  want="${app_arg%/}"
  matches=()
  for d in "${app_dirs[@]}"; do
    if [ "$want" = "$d" ] || [ "$want" = "${d#apps/}" ] || [ "$want" = "${d##*/}" ]; then
      matches+=("$d")
    fi
  done
  if [ "${#matches[@]}" -eq 0 ]; then
    die "no app called '$app_arg' in this repo. It holds:" "$(list_apps)"
  fi
  if [ "${#matches[@]}" -gt 1 ]; then
    printf "dev: '%s' matches more than one app — name one by its directory:\n" "$app_arg" >&2
    printf '  %s\n' "${matches[@]}" >&2
    exit 1
  fi
  app_dir="${matches[0]}"
elif [ "${#app_dirs[@]}" -eq 1 ]; then
  app_dir="${app_dirs[0]}"
else
  # Refusing rather than picking the first: which app you get would then depend
  # on sort order, and the wrong one starts silently and looks like the right
  # one until you notice the data.
  printf 'dev: this repo holds %s apps — name the one to run:\n' "${#app_dirs[@]}" >&2
  list_apps >&2
  printf '\n  pnpm dev --env dev %s\n' "${app_dirs[0]##*/}" >&2
  exit 1
fi

if [ ! -f "$root/$app_dir/package.json" ]; then
  die "$app_dir has an oxy-app.json but no package.json, so there is no dev server to start."
fi

# --- which oxy? -------------------------------------------------------------

if [ -z "$env_name" ]; then
  die "--env is required." \
      "" \
      "  pnpm dev --env dev ${app_dir##*/}" \
      "" \
      "It says which cloud oxy the proxy reads from: dev, staging, production," \
      "a full URL (--env https://acme.oxygen-hq.com), or a name the app's" \
      "oxy-app.json declares under \"environments\"." \
      "" \
      "There is no default on purpose. 'oxyc proxy' defaults to PRODUCTION, and" \
      "a 'pnpm dev' that silently serves production data is not a thing this" \
      "repo will do quietly."
fi

if [ "$env_name" = "production" ] && [ "$confirm_yes" -ne 1 ]; then
  # `oxyc proxy` refuses a production target without --yes on its own. Catching
  # it here matters because the proxy runs in the BACKGROUND: its refusal (or
  # its prompt) would scroll past under Vite's banner, and the app would just
  # fail every request with nothing on screen explaining why.
  die "--env production is real customer data." \
      "" \
      "  pnpm dev --env production --yes ${app_dir##*/}" \
      "" \
      "Add --yes if that is genuinely what you want. For everyday work use" \
      "--env dev or --env staging."
fi

# --- preconditions ----------------------------------------------------------

if ! command -v oxyc >/dev/null 2>&1; then
  die "the 'oxyc' CLI is not on your PATH, and the proxy is oxyc." \
      "" \
      "  npm install -g @oxy-hq/cli" \
      "" \
      "A global install, not npx: the proxy is killed by pid on Ctrl-C, and a" \
      "wrapper process in between would keep the signal from reaching it."
fi

if ! command -v pnpm >/dev/null 2>&1; then
  die "'pnpm' is not on your PATH, and it is what runs the app's dev server." \
      "This repo is pnpm-only — never npm or yarn." \
      "" \
      "  corepack enable pnpm"
fi

# Who, if anyone, is already listening on a port. Empty when the port is free,
# when lsof is not installed, or when lsof cannot tell — this is a diagnostic,
# and it must never be the thing that stops a run.
port_holder=""
read_port_holder() {
  local out="" st=0
  command -v lsof >/dev/null 2>&1 || return 0
  out="$(lsof -nP -iTCP:"$1" -sTCP:LISTEN 2>/dev/null)" || st=$?
  # lsof exits 1 for "nothing matched" AND for its own errors, and there is no
  # way to tell them apart, so both read as "nothing to report".
  if [ "$st" -ne 0 ] || [ -z "$out" ]; then
    return 0
  fi
  port_holder="$(printf '%s\n' "$out" |
    awk 'NR > 1 { print $1 " (pid " $2 ")" }' | LC_ALL=C sort -u | tr '\n' ' ')"
}

read_port_holder "$PROXY_PORT"
if [ -n "$port_holder" ]; then
  die "port $PROXY_PORT is already taken by: $port_holder" \
      "" \
      "'oxyc proxy' has to listen there — the app's dev server proxies /api to" \
      "that exact port — so this cannot just move out of the way. Usually it is" \
      "a proxy from an earlier run that outlived its terminal, or a local" \
      "'oxy serve'. Stop it and try again:" \
      "" \
      "  lsof -nP -iTCP:$PROXY_PORT -sTCP:LISTEN"
fi

# --- go ---------------------------------------------------------------------

proxy_args=(proxy --env "$env_name")
if [ "$allow_writes" -eq 1 ]; then proxy_args+=(--allow-writes); fi
if [ "$allow_events" -eq 1 ]; then proxy_args+=(--allow-events); fi
if [ "$confirm_yes" -eq 1 ]; then proxy_args+=(--yes); fi

say "" \
    "  app     $app_dir" \
    "  env     $env_name" \
    "  proxy   oxyc ${proxy_args[*]}   ->  http://localhost:$PROXY_PORT" \
    "  dev     pnpm dev in $app_dir   ->  http://localhost:$DEV_PORT" \
    ""

# Said once, up front, and not negotiable: "my function isn't running" with no
# visible cause is an hour of someone's day. The proxy holds these back so an
# afternoon of local clicking cannot change cloud state.
if [ "$allow_writes" -eq 1 ]; then
  say "  ! --allow-writes: side-effecting calls (/fn handlers, agent runs," \
      "    automation runs) are being FORWARDED to $env_name for real."
else
  say "  Side-effecting calls — /fn handlers, agent runs, automation runs — are" \
      "  HELD, not sent. A function that 'does nothing' is usually this, not a" \
      "  bug. Pass --allow-writes to forward them."
fi
if [ "$allow_events" -eq 1 ]; then
  say "  ! --allow-events: tracking events are being FORWARDED to $env_name."
else
  say "  Tracking events are DROPPED, so local clicking never shows up in the" \
      "  customer's analytics. Pass --allow-events to forward them."
fi
say "" "  Ctrl-C stops both. If requests come back 401, run: oxyc login --env $env_name" ""

oxyc "${proxy_args[@]}" &
proxy_pid=$!

# A proxy that dies on the first second — a bad --env, a production host
# without --yes — would
# otherwise leave Vite up and every request failing, with the reason scrolled
# off the top. One second is enough to catch the immediate exits and cannot
# produce a false alarm: a proxy still running after it is simply left alone.
sleep 1
if process_finished "$proxy_pid"; then
  proxy_pid=""
  die "'oxyc proxy --env $env_name' exited immediately — see its output above." \
      "" \
      "Most often --env did not resolve to a deployment, or it names a" \
      "production host (an org URL under oxygen-hq.com) and needs --yes." \
      "If it did start but every request is a 401, cache a token for it:" \
      "" \
      "  oxyc login --env $env_name"
fi

# The dev server, in the foreground, and the LAST statement in this file.
#
# A subshell so the script keeps its own cwd for the teardown, and `exec` so
# the dev server IS that process — a wrapper in between would swallow the exit
# status a Ctrl-C produces.
#
# Deliberately not followed by `exit "$status"`. Under errexit this line's
# status is already the script's, the EXIT trap still runs and still kills the
# proxy, and a trailing `exit` costs something real: shellcheck 0.11 stops
# tracing the EXIT trap past one, and reports `cleanup` as dead code (SC2329) —
# a finding this repo's suite treats as a failure.
( cd "$root/$app_dir" && exec pnpm dev )
