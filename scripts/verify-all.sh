#!/usr/bin/env bash
#
# One command for both halves of the suite.
#
# Group A (`scripts/fleet-assert.sh`) fires every declared route at a real
# three-node fleet and compares bodies. Group B drives the browser flows. Both
# already existed; what did not was a way to run them without knowing which of
# four different invocations each flow needs. Getting that wrong does not fail
# loudly — it fails as a locator timeout that reads exactly like a product bug,
# which cost a full session to diagnose once already.
#
# The four invocations, and why they differ:
#
#   fleet   flows aimed at the docker fleet's serve replica: the 8 that
#           `scripts/fleet-assert.sh` owns, plus the 6 nothing used to run
#           (phase 2). Need a minted session (OXY_SESSION_TOKEN) plus
#           OXY_PATH_PREFIX, or every page redirects to /login and every step
#           times out. Phase 2 splits into five invocations — see the comment
#           there for which constraint each boundary is.
#   local   flows the runner backs with `oxy start --local --enterprise`.
#   cloud   flows against an `oxy start --enterprise` this script starts (NOT
#           the runner's `--clean` one) and fills with `oxy seed` first — the
#           admin surfaces and the airway run page have nothing to render on an
#           empty database, and no UI path creates an org any more.
#   obs     flows that read ClickHouse. No seed path exists for observability
#           data, so the spans are inserted directly (the shape
#           `just clickhouse-obs-verify` uses) rather than paying for agent runs.
#
# EVERY flow phase seeds first, through `scripts/seed-fixtures.sh`, and that
# script verifies its own work over HTTP before any flow starts. It is not
# ceremony: an unseeded target fails a flow on its FIRST locator, three minutes
# in, with a message that reads exactly like a broken page. Nine of the fourteen
# fleet/obs flows here failed that way and none of them were product bugs.
#
# Exit codes match fleet-assert's contract, because a runner that cannot tell
# them apart is worse than no runner:
#   0  everything asserted and passed
#   1  the product is wrong
#   2  could not test (missing key, image older than the source, docker down)
#
# Usage:
#   just verify                 # everything, against what is already built
#   just verify --build         # rebuild the fleet image first
#   just verify --group a       # HTTP only
#   just verify --group b       # browser flows only
#   just verify --keep          # leave the stacks up to poke at
#   just verify --dry-run       # print every flow invocation, run none of them
#
# `--dry-run` exists because "which flows does this actually run?" was, until
# now, a question you answered by reading three files and hoping. It prints the
# exact argv each phase would hand the runner — the SAME array, through the same
# `flow_run` door, not a hand-maintained copy that can drift — and touches
# nothing: no seed, no container toggle, no backend spawn, no API key spent.
set -uo pipefail

cd "$(dirname "$0")/.."
REPO="$PWD"
BUILD=0 KEEP=0 GROUP=all EXPENSIVE=0 DRY=0
for a in "$@"; do
  case "$a" in
    --build) BUILD=1 ;;
    --keep)  KEEP=1 ;;
    --expensive) EXPENSIVE=1 ;;
    --dry-run) DRY=1 ;;
    --group) shift ;;
    a|b)     GROUP="$a" ;;
    --group=*) GROUP="${a#--group=}" ;;
    -h|--help) sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown flag: $a" >&2; exit 2 ;;
  esac
done

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✓\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31m✗\033[0m %s\n' "$*"; }
die()  { printf '\033[31mfatal:\033[0m %s\n' "$*" >&2; exit 2; }

RESULTS=()
record() { RESULTS+=("$1|$2|$3"); }   # phase|status|detail

# The ONE door every flow invocation goes through. Two jobs: `--dry-run` gets
# the real argv rather than a second copy of it that drifts, and there is now a
# single place to look for "how is the runner called".
#
# fd 3 is a dup of the script's own stdout, taken before any phase runs. Real
# invocations are redirected into a log file (`> "$LOG_DIR/…" 2>&1`), and a
# dry-run line printed on plain stdout would land in that log instead of on the
# terminal — three of the phases would silently print nothing.
exec 3>&1
flow_run() {
  if [ "$DRY" = 1 ]; then
    printf '  [dry-run] pnpm -C web-app test:agentic %s\n' "$*" >&3
    return 0
  fi
  pnpm -C web-app test:agentic "$@"
}
# Guard for anything that changes state — seeds, container toggles, backend
# spawns. A dry run that stopped the ide node would not be a dry run.
wet() { [ "$DRY" != 1 ]; }

# ── preconditions, all of them, before anything expensive ────────────────────
bold "0. Preconditions"
docker info >/dev/null 2>&1 || die "docker is not running"
[ -f .env ] || die "no .env — the flows need ANTHROPIC_API_KEY and the fleet needs GH_TOKEN"
if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  ANTHROPIC_API_KEY=$(grep -E '^ANTHROPIC_API_KEY=' .env | head -1 | cut -d= -f2- | tr -d '"'"'"'')
  export ANTHROPIC_API_KEY
fi
[ -n "${ANTHROPIC_API_KEY:-}" ] || die "no ANTHROPIC_API_KEY — group B picks its actions with an LLM"
ok "docker up, ANTHROPIC_API_KEY present"

WS="${WS:-70787bb2-e11b-5488-b2c3-02e60d5fc7d3}"
FLEET_URL=http://127.0.0.1:3000
LOG_DIR="${TMPDIR:-/tmp}/oxy-verify-$$"; mkdir -p "$LOG_DIR"
echo "  logs: $LOG_DIR"

# ── group A + the 8 fleet-aimed flows ────────────────────────────────────────
if [ "$GROUP" = all ] || [ "$GROUP" = a ] || [ "$GROUP" = b ]; then
  bold "1. Fleet: HTTP routes across three nodes${GROUP:+ }"
  FLAGS=(docker --keep)
  [ "$BUILD" = 1 ] && FLAGS+=(--build)
  [ "$GROUP" != a ] && FLAGS+=(--flows)
  if [ "$DRY" = 1 ]; then
    printf '  [dry-run] bash scripts/fleet-assert.sh %s\n' "${FLAGS[*]}"
    printf '  [dry-run]   …which runs FLEET_FLOWS: %s\n' \
      "$(sed -n '/^FLEET_FLOWS=(/,/^)/p' scripts/fleet-assert.sh | sed '1d;$d' | tr -d ' ' | tr '\n' ' ')"
    record "A: fleet HTTP + 8 fleet flows" SKIP "dry run"
  elif bash scripts/fleet-assert.sh "${FLAGS[@]}" > "$LOG_DIR/a.log" 2>&1; then
    record "A: fleet HTTP + 8 fleet flows" PASS "$(grep -oE '[0-9]+ passed, [0-9]+ failed' "$LOG_DIR/a.log" | tail -1)"
    ok "$(grep -oE '[0-9]+ passed, [0-9]+ failed, [0-9]+ skipped' "$LOG_DIR/a.log" | tail -1)"
  else
    rc=$?
    [ $rc = 2 ] && { tail -6 "$LOG_DIR/a.log"; die "fleet-assert could not run — see $LOG_DIR/a.log"; }
    record "A: fleet HTTP + 8 fleet flows" FAIL "$(grep -oE '[0-9]+ passed, [0-9]+ failed' "$LOG_DIR/a.log" | tail -1)"
    bad "$(grep -oE '[0-9]+ passed, [0-9]+ failed, [0-9]+ skipped' "$LOG_DIR/a.log" | tail -1)"
  fi
fi

[ "$GROUP" = a ] && { bold "done (group A only)"; exit 0; }

# ── the fleet-shaped flows the harness does not own ──────────────────────────
# These need the fleet up (it is, --keep above) plus the same session env the
# harness hands its own list. Without OXY_SESSION_TOKEN the browser is signed
# out and every one of them times out on its first locator.
bold "2. Fleet-shaped flows outside FLEET_FLOWS"
#
# SIX flows, FIVE invocations, and the split is not tidiness — each boundary is
# a constraint that turns into a locator timeout if you cross it:
#
#   identity      the runner injects exactly ONE session per invocation
#                 (runtimes/bespoke.ts). `flow@oxy.local` is an org owner with no
#                 platform standing and is what every WORKSPACE flow needs; the
#                 SPA bounces staff off a tenant workspace into /admin. The admin
#                 and partner surfaces need the opposite — `e2e@oxy.tech`, which
#                 `/api/customer-apps` and `/api/admin/partners` answer and a
#                 non-staff caller gets a 403 from.
#   backend_mode  admin-airhouse-fleet declares `cloud`, the other five `local`,
#                 and cli.ts refuses a mixed invocation outright.
#   assume-role   partners-console-fleet opens a 60-minute assume session as
#                 Acme. While it is live, ANY e2e@oxy.tech flow expecting /admin
#                 is client-side redirected to /partners — so it runs alone, and
#                 after admin-airhouse-fleet, and its last case stops the session.
#   ide state     ide-unavailable-fleet asserts what a DEAD ide node looks like.
#                 The harness deliberately cannot stop a container from inside a
#                 flow (see that flow's header), so the toggle is bracketed here
#                 and it necessarily runs last.
cases_line() { grep -oE '[0-9]+/[0-9]+ cases passed' "$1" | tail -1; }

fleet_flows() { # label email flow…
  local label="$1" email="$2" slug tok usr log
  shift 2
  slug=$(printf '%s' "$label" | tr -cs 'a-zA-Z0-9' '-')
  if ! wet; then
    printf '  as %-14s ' "$email"
    flow_run "$@" --no-auto-backend
    return
  fi
  curl -s --max-time 10 "$FLEET_URL/api/auth/dev-login?email=${email}" -o "$LOG_DIR/sess-$slug.json"
  tok=$(jq -r '.token // empty' "$LOG_DIR/sess-$slug.json" 2>/dev/null || true)
  usr=$(jq -c '.user // {}' "$LOG_DIR/sess-$slug.json" 2>/dev/null || true)
  if [ -z "$tok" ]; then
    record "B: $label" SKIP "dev-login as $email gave no token"
    bad "$label — dev-login as $email gave no token (OXY_DEV_LOGIN_EMAILS on the fleet?)"
    return
  fi
  log="$LOG_DIR/b-$slug.log"
  if OXY_BASE_URL="$FLEET_URL" OXY_HEALTH_URL="$FLEET_URL/api/health" \
    OXY_SESSION_TOKEN="$tok" OXY_SESSION_USER="$usr" \
    OXY_FIXTURE_WORKSPACE="$WS" OXY_PATH_PREFIX="/local/workspaces/$WS" \
    flow_run "$@" --no-auto-backend >"$log" 2>&1; then
    record "B: $label" PASS "$(cases_line "$log")"
    ok "$label — $(cases_line "$log")"
  else
    record "B: $label" FAIL "$(cases_line "$log")"
    bad "$label — $(cases_line "$log") — see $log"
  fi
}

if [ "$DRY" = 1 ] || curl -sf --max-time 8 "$FLEET_URL/api/health" >/dev/null 2>&1; then
  # The fixture layer, and it runs BEFORE any flow rather than being assumed.
  # An unseeded fleet answers /api/orgs with `[]` and 404s the published app,
  # and every flow below then fails on its first locator looking exactly like a
  # product bug — which is what this phase used to do.
  if ! wet; then
    printf '  [dry-run] bash scripts/seed-fixtures.sh fleet\n'
  elif bash scripts/seed-fixtures.sh fleet >"$LOG_DIR/fixtures-fleet.log" 2>&1; then
    record "B: fleet fixtures" PASS "$(grep -oE '[0-9]+ verified, [0-9]+ missing' "$LOG_DIR/fixtures-fleet.log" | tail -1)"
    ok "fixtures — $(grep -oE '[0-9]+ verified, [0-9]+ missing' "$LOG_DIR/fixtures-fleet.log" | tail -1)"
  else
    record "B: fleet fixtures" FAIL "$(grep -oE '[0-9]+ verified, [0-9]+ missing' "$LOG_DIR/fixtures-fleet.log" | tail -1)"
    bad "fixtures incomplete — see $LOG_DIR/fixtures-fleet.log; flows below will fail for that, not for the product"
  fi

  fleet_flows "fleet workspace flows" flow@oxy.local \
    chat-ask-fleet context-graph-fleet customer-apps-oxy-starter-fleet
  # Authored against `examples/` — the fleet's workspace — not `demo_project/`,
  # which is what `backend_mode: local` spawns against. `ide-create-file` waits
  # for the `agents` folder (its own comment names `examples/agents/`) and
  # `semantic-monitors-inbox` needs the 13 monitors in `examples/.monitor.yml`;
  # `demo_project` has neither, so run locally they can only time out, and the
  # timeout is indistinguishable from a product bug.
  fleet_flows "fleet ide file write" flow@oxy.local ide-create-file
  fleet_flows "fleet monitors + inbox" flow@oxy.local semantic-monitors-inbox
  fleet_flows "fleet admin airhouse" e2e@oxy.tech admin-airhouse-fleet
  fleet_flows "fleet partner console" e2e@oxy.tech partners-console-fleet

  # Last, because it takes the ide down. Both halves run even if the first
  # fails — leaving the fleet with a stopped ide would poison every later phase
  # and every later session on this machine.
  bold "2b. ide outage flow (brackets a container stop/start)"
  wet && docker compose -f docker-compose.fleet.yml stop ide >/dev/null 2>&1
  fleet_flows "ide outage: down" flow@oxy.local ide-unavailable-fleet --tag ide-down
  wet && docker compose -f docker-compose.fleet.yml start ide >/dev/null 2>&1
  for _ in $(seq 1 60); do
    [ "$DRY" = 1 ] && break
    curl -sf --max-time 3 http://127.0.0.1:3010/api/health >/dev/null 2>&1 && break
    sleep 3
  done
  if [ "$DRY" = 1 ] || curl -sf --max-time 3 http://127.0.0.1:3010/api/health >/dev/null 2>&1; then
    fleet_flows "ide outage: recovery" flow@oxy.local ide-unavailable-fleet --tag ide-recovery
  else
    record "B: ide outage: recovery" SKIP "the ide never came back on :3010"
    bad "the ide never came back on :3010 — recovery half not run"
  fi
else
  record "B: fleet-shaped flows" SKIP "fleet not reachable"
  bad "fleet not reachable at $FLEET_URL"
fi

wet && [ "$KEEP" != 1 ] && docker compose -f docker-compose.fleet.yml down >/dev/null 2>&1

# ── local-backed flows ───────────────────────────────────────────────────────
bold "3. Local-backed flows"
# Full filenames, not bare names: the runner matches a flow if its filename
# CONTAINS any positional arg (runner/cli.ts, `globs`). `chat-ask` therefore
# also selects `chat-ask-fleet`, and `ide-world-model-graph` also selects
# `ide-world-model-graph-fleet` — both of which are aimed at the docker fleet
# and prove nothing (or fail outright) against a local backend. Passing the
# `.flow.test.yml` suffix makes each one match exactly itself.
wet || printf '  as %-14s ' '(no session — the runner spawns its own local backend)'
LOCAL_FLOWS=(chat-ask.flow.test.yml chat-early-run-failure.flow.test.yml
             ide-compile-error.flow.test.yml ide-save.flow.test.yml
             ide-world-model-graph.flow.test.yml ide-yaml-diagnostics.flow.test.yml
             launcher-home.flow.test.yml semantic-builder-ask.flow.test.yml)

# Opt-in, because it costs more than the other ten together. Measured twice on
# a cold cache: $3.42 and $3.16, against ~$0.03 for the whole rest of this
# phase. It drives a five-step wizard the LLM has to re-find each time, and
# both runs ended on an IDE still loading its file tree rather than on the
# QuickBooks form — so the money buys a red that is about the fixture, not the
# credentials card. Run it deliberately: `just verify --expensive`.
EXPENSIVE_FLOWS=(ide-pipeline-quickbooks.flow.test.yml)
[ "$EXPENSIVE" = 1 ] && LOCAL_FLOWS+=("${EXPENSIVE_FLOWS[@]}")
if flow_run "${LOCAL_FLOWS[@]}" > "$LOG_DIR/b-local.log" 2>&1; then
  record "B: local flows" PASS "$(grep -oE '[0-9]+/[0-9]+ cases passed' "$LOG_DIR/b-local.log" | tail -1)"
  ok "$(grep -oE '[0-9]+/[0-9]+ cases passed' "$LOG_DIR/b-local.log" | tail -1)"
else
  record "B: local flows" FAIL "$(grep -oE '[0-9]+/[0-9]+ cases passed' "$LOG_DIR/b-local.log" | tail -1)"
  bad "$(grep -oE '[0-9]+/[0-9]+ cases passed' "$LOG_DIR/b-local.log" | tail -1) — see $LOG_DIR/b-local.log"
fi

# ── cloud-backed flows: backend up, `oxy seed`, then the admin surfaces ──────
bold "4. Cloud-backed flows (oxy seed, then the admin surfaces)"
# Self-serve org creation is gone, and with it the UI-driven seed that used to
# run first here (`onboarding-blank-workspace`). `oxy seed` writes the dev
# stack's fixtures into the backend's Postgres instead: the `local` org + a
# compiled, promoted Demo workspace (examples/, which carries a `health_check:`
# block) plus the partner tenants. Start the backend OURSELVES, without
# --clean, before any flow: the runner reuses a healthy :3001, whereas one it
# spawns boots `--clean` and would wipe the seed.
#
# `.env` is dotenvx-shaped and not shell-sourceable; read single values out.
OWNER=$(grep -E '^OXY_OWNER=' .env 2>/dev/null | head -1 | cut -d= -f2- | tr -d '"'"'"'' || true)
GLOBAL_ADMINS=$(grep -E '^OXY_GLOBAL_ADMINS=' .env 2>/dev/null | head -1 | cut -d= -f2- | tr -d '"'"'"'' || true)
# `oxy start`'s own container (crates/app/src/cli/commands/status.rs).
CLOUD_DB="${OXY_CLOUD_DATABASE_URL:-postgresql://postgres:postgres@localhost:15432/oxy}"
if [ "$DRY" = 1 ]; then
  CLOUD_UP=1
elif curl -sf --max-time 5 http://localhost:3001/api/health >/dev/null 2>&1; then
  CLOUD_UP=1
else
  ( OXY_DEV_LOGIN_EMAILS="${OWNER:-hello@oxy.tech}" \
      ./target/debug/oxy start --enterprise > "$LOG_DIR/cloud-backend.log" 2>&1 ) &
  for _ in $(seq 1 60); do
    curl -sf --max-time 3 http://localhost:3001/api/health >/dev/null 2>&1 && break; sleep 4
  done
  curl -sf --max-time 3 http://localhost:3001/api/health >/dev/null 2>&1 && CLOUD_UP=1 || CLOUD_UP=0
fi
if [ "$CLOUD_UP" = 1 ]; then
  # OXY_GLOBAL_ADMINS reaches the SEED process only: it binds those emails as
  # Owner of `local`. OWNER is appended so the staff session below is a member
  # there — otherwise airway-pipeline-run's /local/workspaces/… page answers
  # 403 assume_role_required instead of rendering.
  if ! wet; then
    printf '  [dry-run] OXY_DATABASE_URL=…@%s ./target/debug/oxy seed --workspace-path ./examples\n' "${CLOUD_DB##*@}"
  elif OXY_DATABASE_URL="$CLOUD_DB" OXY_GLOBAL_ADMINS="${GLOBAL_ADMINS:+$GLOBAL_ADMINS,}${OWNER:-hello@oxy.tech}" \
       ./target/debug/oxy seed --workspace-path ./examples > "$LOG_DIR/cloud-seed.log" 2>&1; then
    record "B: cloud seed" PASS "oxy seed (compiled + promoted)"
    ok "oxy seed: local org + Demo workspace compiled + promoted"
  else
    record "B: cloud seed" FAIL "see $LOG_DIR/cloud-seed.log"
    bad "oxy seed failed — the five flows below have nothing to render; see $LOG_DIR/cloud-seed.log"
  fi
  curl -s --max-time 10 "http://localhost:3000/api/auth/dev-login?email=${OWNER:-hello@oxy.tech}" -o "$LOG_DIR/staff.json" 2>/dev/null
  STOK=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["token"])' "$LOG_DIR/staff.json" 2>/dev/null || true)
  SUSR=$(python3 -c 'import json,sys;print(json.dumps(json.load(open(sys.argv[1])).get("user",{})))' "$LOG_DIR/staff.json" 2>/dev/null || true)
  # The admin surfaces are staff-only. Without this the pages redirect and the
  # flows time out on their first locator — indistinguishable from a real bug.
  if OXY_SESSION_TOKEN="$STOK" OXY_SESSION_USER="$SUSR" \
     flow_run admin-airway-config admin-custom-apps-tabs \
       admin-tenants-cockpit admin-workspace-health airway-pipeline-run \
       > "$LOG_DIR/b-cloud.log" 2>&1; then
    record "B: cloud admin flows" PASS "$(grep -oE '[0-9]+/[0-9]+ cases passed' "$LOG_DIR/b-cloud.log" | tail -1)"
    ok "$(grep -oE '[0-9]+/[0-9]+ cases passed' "$LOG_DIR/b-cloud.log" | tail -1)"
  else
    record "B: cloud admin flows" FAIL "$(grep -oE '[0-9]+/[0-9]+ cases passed' "$LOG_DIR/b-cloud.log" | tail -1)"
    bad "$(grep -oE '[0-9]+/[0-9]+ cases passed' "$LOG_DIR/b-cloud.log" | tail -1) — see $LOG_DIR/b-cloud.log"
  fi
else
  record "B: cloud admin flows" SKIP "no cloud backend on :3001"
  bad "no cloud backend came up — see $LOG_DIR/cloud-backend.log"
fi

bold "5. Observability flows (ClickHouse)"
# TWO fixtures, and only one of them used to exist. seed-observability.sh puts
# spans in ClickHouse; that was never enough. `/ide/observability/*` is a
# WORKSPACE surface — with `/api/orgs` answering `[]` the SPA has no project to
# render, so all four flows timed out on their first locator with perfectly
# good trace data sitting in ClickHouse behind them. seed-fixtures.sh native
# seeds the workspace into the same Postgres that server is wired to.
OBS_URL=http://localhost:3000
if ! wet; then
  printf '  [dry-run] bash scripts/seed-observability.sh\n'
  printf '  [dry-run] OXY_BASE_URL=%s bash scripts/seed-fixtures.sh native\n' "$OBS_URL"
  printf '  as %-14s ' flow@oxy.local
  flow_run observability-clusters observability-execution-analytics \
    observability-metrics observability-traces --no-auto-backend
elif bash scripts/seed-observability.sh > "$LOG_DIR/obs-seed.log" 2>&1; then
  ok "ClickHouse up and seeded"
  if OXY_BASE_URL="$OBS_URL" bash scripts/seed-fixtures.sh native > "$LOG_DIR/fixtures-obs.log" 2>&1; then
    ok "workspace fixtures — $(grep -oE '[0-9]+ verified, [0-9]+ missing' "$LOG_DIR/fixtures-obs.log" | tail -1)"
  else
    bad "workspace fixtures incomplete — $(grep -oE '[0-9]+ verified, [0-9]+ missing' "$LOG_DIR/fixtures-obs.log" | tail -1); see $LOG_DIR/fixtures-obs.log"
  fi
  # Same session plumbing the fleet phase needs, for the same reason: without
  # it every page redirects to /login and the timeout is indistinguishable from
  # a broken page. `/ide/...` is workspace-scoped, so it needs the prefix too.
  curl -s --max-time 10 "$OBS_URL/api/auth/dev-login?email=flow@oxy.local" -o "$LOG_DIR/sess-obs.json"
  OTOK=$(jq -r '.token // empty' "$LOG_DIR/sess-obs.json" 2>/dev/null || true)
  OUSR=$(jq -c '.user // {}' "$LOG_DIR/sess-obs.json" 2>/dev/null || true)
  if OXY_BASE_URL="$OBS_URL" OXY_HEALTH_URL="$OBS_URL/api/health" \
     OXY_SESSION_TOKEN="$OTOK" OXY_SESSION_USER="$OUSR" \
     OXY_FIXTURE_WORKSPACE="$WS" OXY_PATH_PREFIX="/local/workspaces/$WS" \
     flow_run observability-clusters observability-execution-analytics \
       observability-metrics observability-traces --no-auto-backend \
       > "$LOG_DIR/b-obs.log" 2>&1; then
    record "B: observability flows" PASS "$(cases_line "$LOG_DIR/b-obs.log")"
    ok "$(cases_line "$LOG_DIR/b-obs.log")"
  else
    record "B: observability flows" FAIL "$(cases_line "$LOG_DIR/b-obs.log")"
    bad "$(cases_line "$LOG_DIR/b-obs.log") — see $LOG_DIR/b-obs.log"
  fi
else
  record "B: observability flows" SKIP "could not stand up ClickHouse — see $LOG_DIR/obs-seed.log"
  bad "could not stand up ClickHouse"
fi

if wet && [ "$KEEP" != 1 ]; then
  pkill -f "oxy start --enterprise" 2>/dev/null || true
  docker compose -f docker-compose.fleet.yml down >/dev/null 2>&1 || true
fi

echo
bold "── summary ──"
FAILED=0
for r in "${RESULTS[@]}"; do
  IFS='|' read -r phase status detail <<< "$r"
  case "$status" in
    PASS) printf '  \033[32m%-4s\033[0m %-34s %s\n' "$status" "$phase" "$detail" ;;
    SKIP) printf '  \033[33m%-4s\033[0m %-34s %s\n' "$status" "$phase" "$detail" ;;
    *)    printf '  \033[31m%-4s\033[0m %-34s %s\n' "$status" "$phase" "$detail"; FAILED=1 ;;
  esac
done
echo "  logs: $LOG_DIR"
exit $FAILED
