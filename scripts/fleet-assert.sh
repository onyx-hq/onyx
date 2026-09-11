#!/usr/bin/env bash
#
# Assert the split-fleet contract over HTTP, with no browser involved.
#
# The browser flows in web-app/tests/agentic/ assert what a PAGE shows. This
# asserts what the FLEET does underneath: which node answers a route, whether
# two replicas agree, and what survives when the ide node dies. None of that is
# visible to a flow — a flow drives one base URL and cannot see the
# `x-oxy-served-by` header, let alone compare replicas.
#
# The failure this exists to catch is the one the compile boundary was built
# for: a read that needs the workspace FILES answering on a node that has none.
# Its signature is not an exception — it is a 200 with an empty body, or a route
# quietly answered by the wrong node. A route counts as covered here only when
# a REAL request (real session, valid parameters, a response worth comparing)
# reaches the handler — an unauthenticated probe that only reads
# `x-oxy-served-by` proves the router classified the route, never that the
# HANDLER behaves correctly on a node with no working copy. See
# internal-docs/2026-08-25-fleet-http-route-coverage.md for the inventory this
# harness's route table was built from, and its Methodology section for why
# `route_declarations()` is the source of truth rather than grep.
#
# WHAT THIS CANNOT PROVE. Both nodes here run on one machine, so the workspace
# directory physically exists for the `serve` process too. `OXY_ROLE=serve`
# makes it DECLARE that it owns no working copy, and every assertion below tests
# that the declaration is honoured. It cannot catch code that skips the
# capability check and reads the disk anyway — that read succeeds here and fails
# in production. Only docker-compose.fleet.yml, where the serve containers have
# no volume, makes the directory genuinely absent. Run this for the contract;
# run the fleet compose for the absence.
#
#   scripts/fleet-assert.sh                # assert against whatever is built
#   scripts/fleet-assert.sh --build        # build first, then assert  ← after a code change
#   scripts/fleet-assert.sh docker --build # same, against the compose fleet
#   scripts/fleet-assert.sh --keep         # leave the fleet running afterwards
#   scripts/fleet-assert.sh --skip-fixtures # only the "Now" bucket — no disposable creates
#   scripts/fleet-assert.sh docker --build --flows   # …and then drive the browser
#                                                    # flows at the same replica
#
# `--flows` is opt-in because it costs real money: the flows pick their actions
# with an LLM. Everything else here is free, so it must stay runnable on every
# save without a bill.
#
# `--build` is opt-in rather than the default so the two failure kinds stay
# distinguishable: a build error exits 2 ("could not test this"), a violated
# assertion exits 1 ("tested it, the product is wrong"). Collapsing them would
# make a red run ambiguous. Both builders are incremental, so `--build` is cheap
# to pass after a one-line edit and correct to pass after a merge.
#
# The assertions are identical in both modes — only where the URLs come from and
# how the ide is stopped differ. `docker` mode is the one that proves ABSENCE:
# there the serve containers have no state volume, so the workspace directory
# genuinely is not on them.
#
# ── The route table (scripts/fleet-routes.tsv) ──────────────────────────────
# 224 routes come out of `oxy_app::server::router::route_declarations()` — see
# that file's header for the exact regeneration recipe (a temporary #[test],
# run once, deleted). This script does not hand-maintain a route list; it reads
# that generated file, buckets every declared route (Now / Fixture / Destr /
# Ext / Struct), and REPORTS coverage honestly: what ran, what a disposable
# fixture created, and what was left out with a stated reason. If a new route
# lands in route_declarations() and nobody regenerates fleet-routes.tsv, phase
# 7 below prints it under "declared but not in the catalog" rather than
# silently ignoring it.
#
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

OXY_BIN="${OXY_BIN:-$REPO_ROOT/target/debug/oxy}"
PG_CONTAINER="${PG_CONTAINER:-oxy-fleet-assert-pg}"
PG_PORT="${PG_PORT:-5432}"
DB_URL="postgresql://admin:password@localhost:${PG_PORT}/oxy"
# Deterministic: Uuid::new_v5(NAMESPACE_DNS, "demo.oxy.local") — see seed.rs.
WS="${WS:-70787bb2-e11b-5488-b2c3-02e60d5fc7d3}"
# Deterministic local org, verified live (product-context.md: org "local").
ORG_ID="${ORG_ID:-00000000-0000-0000-0000-000000000000}"
DEV_EMAIL="dev@oxy.local"
ROUTES_FILE="$REPO_ROOT/scripts/fleet-routes.tsv"

MODE="${FLEET_MODE:-native}"
COMPOSE=(docker compose -f docker-compose.fleet.yml)
FLEET_IMAGE="${FLEET_IMAGE:-oxy-fleet:local}"
# Two identities, and the split matters. The HTTP probes sign in as staff, which
# reaches everything. The browser flows must NOT: the SPA bounces a Global Owner
# off every tenant workspace into /admin until they open an assume-role session,
# so a flow signed in as staff never reaches the surface it came to test. This
# one is seeded as an org owner and deliberately left out of the server's
# OXY_GLOBAL_ADMINS, so it carries no platform standing.
FLOW_EMAIL="${FLOW_EMAIL:-flow@oxy.local}"

# The flows `--flows` runs, and why it is a LIST rather than "all of them".
#
# Two hard constraints make an unfiltered run impossible. The runner refuses a
# mixed `backend_mode` in one invocation (cli.ts:289), and it injects exactly
# ONE session per invocation (runtimes/bespoke.ts) — while the admin flows need
# the staff identity and every workspace flow needs the non-staff one. So this
# list is the workspace-scoped, single-identity set, verified to pass against a
# diskless replica.
#
# Deliberately excluded, none of them a fleet fault:
#   ide-compile-error   — `wip`, NOT IN CI since 2026-05-10: Monaco has no SQL
#                         diagnostics service yet, so the feature it asserts
#                         does not exist on any deployment.
#   launcher-home       — two of its three cases need a workspace with NO apps;
#                         `oxy seed` always publishes oxy-starter, so that state
#                         is unreachable here rather than broken.
#   semantic-builder-ask— assumes an edit is visible to a semantic read without
#                         a compile. There is no compile-on-save by design
#                         (internal-docs/compile-boundary.md:13), so the flow's
#                         premise, not the fleet, is what fails.
#   admin-*             — need the staff identity; run them separately.
FLEET_FLOWS=(
  apps-data-app-charts
  automations-run-from-list
  builder-edits-app
  chat-panel-agent-switch
  ide-world-model-graph-fleet
  metric-tree
  semantic-object-detail
  threads-list
)

KEEP=0
BUILD=0
FLOWS=0
SKIP_FIXTURES=0
for arg in "$@"; do
  case "$arg" in
    --keep)          KEEP=1 ;;
    --build)         BUILD=1 ;;
    --flows)         FLOWS=1 ;;
    --skip-fixtures) SKIP_FIXTURES=1 ;;
    native|docker)   MODE="$arg" ;;
    *) printf 'unknown argument: %s (want --build | --flows | --keep | --skip-fixtures | native | docker)\n' "$arg" >&2; exit 2 ;;
  esac
done

# Every probe below goes to a PUBLIC port on purpose. `enforce_role` — the
# middleware that classifies a route and self-proxies IdeOnly to the ide — wraps
# only the public surface (crates/app/src/cli/commands/serve.rs). Drive the
# auth-disabled internal port and you bypass the very thing under test, and every
# IdeOnly route answers 503 locally instead of proxying.
case "$MODE" in
  native)
    IDE_PORT=3000;   IDE_INTERNAL=3001
    S1_PORT=3002;    S1_INTERNAL=3003
    S2_PORT=3004;    S2_INTERNAL=3005
    IDE_BASE="http://127.0.0.1:${IDE_PORT}"
    S1_BASE="http://127.0.0.1:${S1_PORT}"
    S2_BASE="http://127.0.0.1:${S2_PORT}"
    DEV_EMAIL="dev@oxy.local"
    ;;
  docker)
    # Published by docker-compose.fleet.yml: ide 3010, replicas 3000/3001.
    IDE_BASE="http://127.0.0.1:3010"
    S1_BASE="http://127.0.0.1:3000"
    S2_BASE="http://127.0.0.1:3001"
    # Already OXY_OWNER + OXY_GLOBAL_ADMINS in the compose env, so signing in as
    # this address lands an org owner without seeding a second identity.
    DEV_EMAIL="e2e@oxy.tech"
    WS_PATH="/var/lib/oxy/data/examples"
    ;;
  *) printf 'unknown FLEET_MODE=%s (want native|docker)\n' "$MODE" >&2; exit 2 ;;
esac

PASS=0; FAIL=0; SKIPPED=0
declare -a FAILURES=()
declare -a SKIP_NOTES=()
declare -a CLEANUP_CMDS=()   # "METHOD|URL" run at the very end, best-effort

ok()   { PASS=$((PASS+1)); printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); FAILURES+=("$1"); printf '  \033[31m✗\033[0m %s\n' "$1"; }
skip() { SKIPPED=$((SKIPPED+1)); SKIP_NOTES+=("$1"); printf '  \033[33m⊘\033[0m %s\n' "$1"; }
step() { printf '\n\033[1m%s\033[0m\n' "$1"; }
die()  { printf '\033[31mfatal:\033[0m %s\n' "$1" >&2; exit 2; }

# A route whose query targets a `dataset:` path INSIDE the workspace working
# copy (e.g. DuckDB "local", `dataset: .db/`, no `s3_mirror` configured on
# this fixture) can never succeed on a stateless replica — that node does not
# have the directory and never will. Demanding serve==ide there asserts the
# impossible; three cases did exactly that and were wrongly logged as product
# defects before this was caught. The contract that IS true, and the one this
# helper checks per replica: it must refuse LOUDLY (5xx) — never answer 2xx,
# and *especially* never a 2xx with an empty/wrong result, which is the
# failure-as-absence shape this whole harness exists to catch.
#
# `does not have` is not an arbitrary substring: it is the exact, tested
# phrase `ResolveWorkspaceFile`'s no-disk arm returns
# (crates/core/src/config/manager.rs:157-158, asserted verbatim by
# `a_read_only_manager_without_the_files_says_so` in the same file) — the
# single choke point every one of these routes' disk resolution goes
# through. But NOT every route puts that message in the HTTP body: reading
# each handler's error mapping (not guessing) found it genuinely differs —
#   - propagates the message into the JSON body: `sql/query`, `sql/{pathb64}`
#     (`agentic_error_response` → `SqlErrorResponse.message`, data.rs:198-206),
#     and `semantic`'s Warehouse branch (same helper).
#   - logs it, but the HTTP body is EMPTY (a bare `StatusCode`, no `Json`):
#     `databases/{name}/schema` (database.rs:96-113, doc comment says so
#     outright: "surface a 500 with the actual error logged"),
#     `databases/inspect-schemas` (database.rs:1113-1118),
#     `databases/inspect-schema-tables` (database.rs:1160-1167) — same
#     bare-`StatusCode`-no-`Json` pattern in all three.
#   - logs it, but the HTTP body is a GENERIC string that names nothing:
#     `metric-tree/explain`, `/opportunity`, `/distribution`, `/drill` — all
#     four fall through `MetricTreeError::Op(e.to_string())`, whose
#     `IntoResponse` arm discards `e` and sends the fixed string "Metric
#     tree operation failed" (metric_tree.rs:51-84).
# Asserting "the body names the working copy" against the second and third
# groups would just be this task's original mistake again, wearing a new
# shape — so the CALLER picks compare=workspace_local (status only) for
# those, and compare=workspace_local_named (status + body) only for the
# routes verified to actually carry the message. One mechanism, an honest
# split where the product itself is inconsistent — not seven exceptions.
assert_workspace_local_refusal() { # bucket label node_name code bodyfile compare method path
  local bucket="$1" label="$2" node="$3" code="$4" bodyfile="$5" cmp="$6" method="$7" path="$8"
  # With a blob mirror configured the expectation INVERTS: the warehouse lives
  # in S3, the replica attaches it over httpfs, and answering is correct — see
  # FLEET_HAS_MIRROR above. Refusing would now be the defect, so assert that
  # instead of loosening the check to "either is fine": a mirrored fleet that
  # goes back to refusing has lost the whole point of having replicas.
  # `000` is curl's "no response at all" — a timeout or a refused connection.
  # It says nothing about the split-fleet contract, and reporting it as one
  # sends the reader hunting a routing bug that isn't there. Name it for what
  # it is, and keep it red: a route this harness could not reach is a route it
  # did not test, and silently downgrading that to a skip is the exact
  # failure-as-absence shape the whole fleet contract exists to prevent.
  if [ "$code" = "000" ]; then
    bad "[$bucket] $label: $node did not respond (curl 000 — timeout or refused); this case was NOT tested, and the cause is the harness environment, not the fleet contract ($method $path)"
    return
  fi
  if [ "${FLEET_HAS_MIRROR:-0}" = 1 ]; then
    if [[ "$code" =~ ^2 ]]; then
      ok "[$bucket] $label: $node serves it from the mirror ($code) ($method $path)"
    else
      bad "[$bucket] $label: $node returned $code, but this fleet has a blob mirror — a mirrored warehouse must be readable from any node ($method $path)"
    fi
    return
  fi
  if [[ "$code" =~ ^2 ]]; then
    bad "[$bucket] $label: $node returned $code for a workspace-local dataset query — should refuse loudly, not answer ($method $path)"
  elif [[ "$code" =~ ^5 ]]; then
    if [ "$cmp" = "workspace_local_named" ] && ! grep -qi 'does not have' "$bodyfile" 2>/dev/null; then
      bad "[$bucket] $label: $node returned $code but the body doesn't name the working copy, expected it to ($method $path)"
    else
      ok "[$bucket] $label: $node refuses loudly ($code) ($method $path)"
    fi
  else
    bad "[$bucket] $label: $node returned $code — expected a 5xx naming a disk it does not have ($method $path)"
  fi
}

# ── curl helpers ──────────────────────────────────────────────────────────────
# Status only.
# 30s, matching req(). At 15s this disagreed with the very cases it replays:
# `databases/local/schema` on a replica attaches the mirrored DuckDB over
# httpfs and introspects it, measured at 13.3-13.8s with the ide up and
# 22.5-23.6s with the ide STOPPED — 200 both times. So the route honours its
# FleetOk declaration; it was phase 12's tighter deadline that turned a slow
# success into `000`, reported as "a persisted read was pinned to the stateful
# node". A replay must not be stricter than the measurement it replays.
#
# No `|| echo "000"` fallback: curl's own `-w '%{http_code}'` already prints
# `000` when no HTTP response arrived, so the fallback appended a second one
# and this printed the literal `000000` — the same double-print req() was
# fixed for. Harmless to the regexes, confusing to read, and it is what the
# ide-down failure line showed.
code() {
  local out
  out=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 -H "Authorization: ${TOKEN:-}" "$1" 2>/dev/null)
  [ -z "$out" ] && out="000"
  printf '%s' "$out"
}
# Status, plus the two routing stamps, as "code|served-by|forwarded".
stamp() {
  local hdr; hdr="$(mktemp)"
  local c; c=$(curl -s -o /dev/null -D "$hdr" -w '%{http_code}' --max-time 15 -H "Authorization: ${TOKEN:-}" "$1" 2>/dev/null || echo "000")
  local by fwd
  by=$(grep -i '^x-oxy-served-by:' "$hdr" | sed 's/.*: *//; s/@.*//; s/\r//' | head -1)
  grep -iq '^x-oxy-forwarded-via:' "$hdr" && fwd=yes || fwd=no
  rm -f "$hdr"
  echo "${c}|${by:-none}|${fwd}"
}
# Real, method-accurate request. Writes the body to $3, prints the status code.
# Backgroundable: callers that want the three nodes in parallel run this with
# `&` and `wait`, since the body/status land in files rather than a captured
# subshell stdout.
req() { # method url outfile [json_body]
  local extra=() out
  [ -n "${4:-}" ] && extra=(-H 'Content-Type: application/json' -d "$4")
  # `"${extra[@]+"${extra[@]}"}"`, not a bare `"${extra[@]}"`: on an EMPTY
  # array (every GET call), bash < 4.4 treats `${extra[@]}` as unset under
  # `set -u` and aborts with "unbound variable" — this is the exact bug that
  # silently zeroed out every GET request until it was caught. The `+` form
  # expands to nothing when the array is empty instead of erroring.
  #
  # Captured into `out` rather than a bare `curl ... || echo "000"`: on a
  # true connection failure (refused, DNS, or --max-time exceeded), curl's
  # own `-w '%{http_code}'` ALREADY prints "000" (its documented behaviour
  # for "no HTTP response received") before exiting non-zero — so the old
  # `|| echo "000"` fallback ran TOO, concatenating to "000000". Silently
  # wrong (every `^2`/`^5` regex check still correctly falls through to
  # "not matched", so no case's pass/fail verdict flipped), but confusing
  # to read and measured live (world-model-competitors, an upstream call to
  # the public OSM Overpass API, timed out on two of three nodes and
  # printed exactly "000000"). Only fall back to "000" when curl produced
  # NO output of its own.
  out=$(curl -s -o "$3" -w '%{http_code}' --max-time 30 -X "$1" -H "Authorization: ${TOKEN:-}" "${extra[@]+"${extra[@]}"}" "$2" 2>/dev/null)
  [ -z "$out" ] && out="000"
  printf '%s' "$out"
}
req_bg() { # method url outfile statusfile [json_body]
  ( c=$(req "$1" "$2" "$3" "${5:-}"); printf '%s' "$c" > "$4" ) &
}

# base64 helpers — the codebase is NOT consistent (see the report): app.rs,
# file.rs, semantic.rs and test_file.rs all decode with BASE64_STANDARD
# (tolerating unpadded input); automation.rs alone uses URL_SAFE_NO_PAD. Prefer
# the server's own `path_b64` field over hand-encoding wherever the list
# endpoint provides one (automations/procedures do).
pathb64_std() { printf '%s' "$1" | base64 | tr -d '\n'; }

# Recursively normalize known-volatile fields (see "Routes whose body
# legitimately differs" in the inventory doc) and canonicalize array-of-object
# ordering by `.id` so non-deterministic query ordering doesn't false-fail a
# body compare. Applied to every body comparison; the SPECIFIC per-process-cache
# routes (preagg-status, semantic/topic, semantic/view, metric-tree families,
# admin/compiles, admin/internal-jobs) are instead marked compare=status in the
# case table below, with a comment, because their volatility is not reducible
# to stripping one field.
NORMALIZE_JQ='
def sort_ids:
  if type == "array" then
    (map(sort_ids)) as $mapped
    | if ($mapped | length) > 0 and ($mapped[0] | type) == "object"
      then
        # `.id` first, then `.name`, then the whole object. Sorting on `.id`
        # alone left `/modeling/{project}/nodes` and `/lineage` comparing
        # unequal: dbt nodes key on `name`, and the handler serialises them
        # straight out of an in-memory map, so the twelve come back in a
        # different order on every call — serve-1 disagreed with serve-2,
        # which have identical state, so this was never about disk. Falling
        # back to the whole object keeps the sort total for shapes that carry
        # neither key, which is what makes the comparison deterministic rather
        # than merely usually-equal.
        if ($mapped[0] | has("id")) then ($mapped | sort_by(.id))
        elif ($mapped[0] | has("name")) then ($mapped | sort_by(.name))
        else ($mapped | sort_by(tojson))
        end
      else $mapped
      end
  elif type == "object" then
    with_entries(.value |= sort_ids)
  else
    .
  end;
def null_volatile:
  if type == "object" then
    with_entries(
      . as $kv
      | if (["idle_secs","elapsed_ms","checked_at","last_active_at","updated_at",
             "queue_depth","in_flight","pending","staleness_secs","age_secs",
             "served_by","node","generated_at","now","timestamp","pagination",
             "next_run_at","created_at","published_at","last_promoted_at",
             "last_synced_at","last_active"] | index($kv.key))
        then $kv | .value = "<normalized>"
        else $kv | .value |= null_volatile
        end
    )
  elif type == "array" then
    map(null_volatile)
  else
    .
  end;
sort_ids | null_volatile
'
# ── degrades_null helpers ────────────────────────────────────────────────────
# Fields a stateless replica cannot compute because counting them means walking
# the working copy. Kept in one place so the three checks below cannot drift.
DEGRADED_FIELDS='["agent_count","workflow_count","app_count"]'
dn_values() { # infile -> one JSON array of every degraded field's value
  jq --argjson f "$DEGRADED_FIELDS" \
    '[.. | objects | to_entries[] | select(.key as $k | $f | index($k)) | .value]' "$1" 2>/dev/null
}
dn_all_numeric() { [ "$(dn_values "$1" | jq -r 'length > 0 and all(type == "number")' 2>/dev/null)" = true ]; }
dn_all_null()    { [ "$(dn_values "$1" | jq -r 'length > 0 and all(. == null)'      2>/dev/null)" = true ]; }
dn_blank() { # infile -> canonical JSON with the degraded fields masked
  jq -S --argjson f "$DEGRADED_FIELDS" \
    '[.. | objects] as $_ | walk(if type == "object" then with_entries(if (.key as $k | $f | index($k)) then .value = "<degraded>" else . end) else . end)' \
    "$1" 2>/dev/null
}

normalize_body() { # infile -> canonical JSON on stdout, or raw bytes if not JSON
  if jq -e . "$1" >/dev/null 2>&1; then
    jq -S "$NORMALIZE_JQ" "$1" 2>/dev/null
  else
    cat "$1"
  fi
}

# Refuse to assert against a build older than the code it claims to test.
assert_fresh() { # ref_file label rebuild_hint
  local off
  off=$(find crates Cargo.toml Cargo.lock -type f -newer "$1" -print -quit 2>/dev/null)
  if [ -z "$off" ]; then ok "$2 is newer than every source file"; return; fi
  if [ "${ALLOW_STALE:-0}" = "1" ]; then
    ok "$2 is STALE ($off is newer) — continuing because ALLOW_STALE=1"
    return
  fi
  die "$2 is stale — $off is newer than it.
       Asserting against it would prove nothing about the current code.
       Rebuild:  $3
       Override: ALLOW_STALE=1"
}

wait_health() { # url deadline_secs
  local deadline=$((SECONDS+$2))
  until curl -sf -m 3 "$1" >/dev/null 2>&1; do
    [ $SECONDS -gt $deadline ] && return 1
    sleep 2
  done
  return 0
}

ide_stop() {
  if [ "$MODE" = docker ]; then "${COMPOSE[@]}" stop ide >/dev/null 2>&1
  else kill "${PID_ide:-0}" 2>/dev/null; fi
}
ide_start() {
  if [ "$MODE" = docker ]; then "${COMPOSE[@]}" start ide >/dev/null 2>&1
  else start_node ide ide "$IDE_PORT" "$IDE_INTERNAL" env OXY_INPROC_GLOBAL_WORKER=1; fi
}

cleanup() {
  # Best-effort teardown of every disposable fixture this run created —
  # regardless of --keep, since these are OUR scratch resources on the shared
  # fixture, not the fleet itself. Reverse order: children before parents.
  if [ "${#CLEANUP_CMDS[@]}" -gt 0 ]; then
    printf '\nCleaning up %d disposable fixture(s)…\n' "${#CLEANUP_CMDS[@]}"
    for ((i=${#CLEANUP_CMDS[@]}-1; i>=0; i--)); do
      IFS='|' read -r cm cu <<< "${CLEANUP_CMDS[$i]}"
      curl -s -o /dev/null --max-time 15 -X "$cm" -H "Authorization: ${TOKEN:-}" "$cu" 2>/dev/null
    done
  fi
  if [ "$MODE" = docker ]; then
    # The compose fleet is not ours to destroy — we only borrowed it. But phase
    # "kill the ide" stops it, so leaving without restarting it would hand back
    # a fleet that looks broken.
    "${COMPOSE[@]}" start ide >/dev/null 2>&1
    return
  fi
  [ "$KEEP" = "1" ] && { printf '\n(--keep) fleet left running: ide :%s  serve :%s  serve :%s\n' "$IDE_PORT" "$S1_PORT" "$S2_PORT"; return; }
  printf '\nTearing down…\n'
  pkill -f "$OXY_BIN serve" 2>/dev/null
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1
}
trap cleanup EXIT

start_node() { # name role port internal extra_env...
  local name=$1 role=$2 port=$3 internal=$4; shift 4
  OXY_DATABASE_URL="$DB_URL" \
  OXY_GLOBAL_ADMINS="internal@localhost,$DEV_EMAIL" \
  OXY_DEV_LOGIN_EMAILS="$DEV_EMAIL,$FLOW_EMAIL" \
  OXY_ROLE="$role" \
  "$@" \
  "$OXY_BIN" serve --enterprise --port "$port" --internal-port "$internal" \
    > "/tmp/oxy-fleet-assert-$name.log" 2>&1 &
  printf -v "PID_${name}" '%s' "$!"
}

# ══ BUILD (opt-in) ════════════════════════════════════════════════════════════
if [ "$BUILD" = "1" ]; then
  step "0. Build"
  if [ "$MODE" = native ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo not on PATH — install the Rust toolchain first"
    cargo build -p oxy-server || die "the build failed — fix that before testing the fleet"
    ok "oxy-server built"
  else
    docker info >/dev/null 2>&1 || die "docker is not running"
    if [ -z "${GH_TOKEN:-}" ]; then
      GH_TOKEN="$(gh auth token 2>/dev/null || true)"
      [ -n "$GH_TOKEN" ] || die "the fleet image needs a GitHub token for the private oxy-hq crates.
       Either: gh auth login    (then re-run)
       Or:     export GH_TOKEN=<a token with repo read access>"
      export GH_TOKEN
    fi
    printf '  first build on a machine is ~25 minutes (it compiles the workspace\n'
    printf '  in release mode inside the container); BuildKit caches it after that.\n'
    "${COMPOSE[@]}" up -d --build || die "the fleet image failed to build — fix that before testing the fleet"
    ok "fleet image built, fleet up"
  fi
fi

# ══ SETUP ═════════════════════════════════════════════════════════════════════
if [ "$MODE" = native ]; then

  step "1. Preflight"
  docker info >/dev/null 2>&1 || die "docker is not running"
  [ -x "$OXY_BIN" ] || die "no binary at $OXY_BIN — run: cargo build -p oxy-server"
  assert_fresh "$OXY_BIN" "the binary" "cargo build -p oxy-server"
  for p in $IDE_PORT $IDE_INTERNAL $S1_PORT $S1_INTERNAL $S2_PORT $S2_INTERNAL; do
    lsof -nP -iTCP:"$p" -sTCP:LISTEN >/dev/null 2>&1 && die "port $p already in use"
  done
  ok "binary present, ports free, docker up"

  step "2. Postgres"
  docker rm -f "$PG_CONTAINER" >/dev/null 2>&1
  docker run -d --name "$PG_CONTAINER" \
    -e POSTGRES_USER=admin -e POSTGRES_PASSWORD=password -e POSTGRES_DB=oxy \
    -p "${PG_PORT}:5432" postgres:18-alpine >/dev/null || die "could not start postgres"
  for _ in $(seq 1 120); do
    docker exec "$PG_CONTAINER" pg_isready -U admin -d oxy >/dev/null 2>&1 && break
    sleep 1
  done
  if ! docker exec "$PG_CONTAINER" pg_isready -U admin -d oxy >/dev/null 2>&1; then
    printf '\n--- %s log ---\n' "$PG_CONTAINER" >&2
    docker logs --tail 20 "$PG_CONTAINER" >&2 2>&1
    die "postgres never became ready"
  fi
  ok "postgres ready on :$PG_PORT"

  step "3. Start ide node"
  ide_start
  wait_health "http://127.0.0.1:${IDE_INTERNAL}/api/health" 180 || die "ide never became healthy (see /tmp/oxy-fleet-assert-ide.log)"
  ok "ide up on :$IDE_PORT"

  step "4. Seed + compile + promote"
  OXY_DATABASE_URL="$DB_URL" OXY_GLOBAL_ADMINS="internal@localhost,$DEV_EMAIL,$FLOW_EMAIL" \
    "$OXY_BIN" seed >/tmp/oxy-fleet-assert-seed.log 2>&1 || die "seed failed (see /tmp/oxy-fleet-assert-seed.log)"
  OXY_DATABASE_URL="$DB_URL" "$OXY_BIN" compile \
    --workspace-path "$REPO_ROOT/examples" --workspace-id "$WS" \
    --enterprise --promote --skip-migrations >/tmp/oxy-fleet-assert-compile.log 2>&1 \
    || die "compile failed (see /tmp/oxy-fleet-assert-compile.log)"
  promoted=$(docker exec "$PG_CONTAINER" psql -U admin -d oxy -t -A \
    -c "select coalesce(current_revision_id::text,'') from workspaces where id='$WS'" 2>/dev/null | tr -d ' ')
  [ -n "$promoted" ] || die "workspace $WS has no promoted revision — the serve fleet would 503 every compiled read"
  ok "compiled + promoted revision $promoted"

  step "5. Start two stateless serve replicas"
  start_node serve1 serve "$S1_PORT" "$S1_INTERNAL" env OXY_IDE_UPSTREAM="http://localhost:${IDE_INTERNAL}"
  start_node serve2 serve "$S2_PORT" "$S2_INTERNAL" env OXY_IDE_UPSTREAM="http://localhost:${IDE_INTERNAL}"
  wait_health "${S1_BASE}/api/health" 180 || die "serve-1 never became healthy"
  wait_health "${S2_BASE}/api/health" 180 || die "serve-2 never became healthy"
  ok "serve-1 and serve-2 up"

else # ── docker ────────────────────────────────────────────────────────────────

  step "1. Preflight (docker fleet)"
  docker info >/dev/null 2>&1 || die "docker is not running"
  created=$(docker image inspect "$FLEET_IMAGE" --format '{{.Created}}' 2>/dev/null) \
    || die "no image $FLEET_IMAGE — run: docker compose -f docker-compose.fleet.yml up -d --build"
  img_ref=$(mktemp)
  python3 -c 'import sys,os,re,datetime
m=re.match(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})", sys.argv[1])
t=datetime.datetime.fromisoformat(m.group(1)).replace(tzinfo=datetime.timezone.utc).timestamp()
os.utime(sys.argv[2],(t,t))' "$created" "$img_ref" 2>/dev/null \
    || die "could not read the build time of $FLEET_IMAGE"
  assert_fresh "$img_ref" "image $FLEET_IMAGE (built $created)" \
    "docker compose -f docker-compose.fleet.yml up -d --build"
  rm -f "$img_ref"
  for b in "$IDE_BASE" "$S1_BASE" "$S2_BASE"; do
    wait_health "${b}/api/health" 120 \
      || die "no healthy node at $b — is the fleet up? (docker compose -f docker-compose.fleet.yml up -d --build)"
  done
  ok "ide + two replicas healthy"

  step "2. Bootstrap the workspace inside the ide container"
  "${COMPOSE[@]}" exec -T ide rm -rf "${WS_PATH:?}" >/dev/null 2>&1
  "${COMPOSE[@]}" cp examples "ide:${WS_PATH}" >/dev/null 2>&1 || die "could not copy examples/ into the ide container"
  "${COMPOSE[@]}" exec -T -e OXY_GLOBAL_ADMINS="${DEV_EMAIL},${FLOW_EMAIL}" \
    -w /var/lib/oxy/data ide oxy seed >/tmp/oxy-fleet-assert-seed.log 2>&1 \
    || die "seed failed inside the ide container (see /tmp/oxy-fleet-assert-seed.log)"
  "${COMPOSE[@]}" exec -T ide oxy compile --workspace-path "$WS_PATH" --workspace-id "$WS" \
    --enterprise --promote --skip-migrations >/tmp/oxy-fleet-assert-compile.log 2>&1 \
    || die "compile failed inside the ide container (see /tmp/oxy-fleet-assert-compile.log)"
  promoted=$("${COMPOSE[@]}" exec -T postgres psql -U admin -d oxy -t -A \
    -c "select coalesce(current_revision_id::text,'') from workspaces where id='$WS'" 2>/dev/null | tr -d ' \r')
  [ -n "$promoted" ] || die "workspace $WS has no promoted revision — every compiled read would 503"
  ok "compiled + promoted revision $promoted"

  step "3. Confirm the replicas genuinely lack the files"
  if "${COMPOSE[@]}" exec -T ide test -d "$WS_PATH" 2>/dev/null; then
    ok "ide HAS $WS_PATH"
  else
    die "ide does not have $WS_PATH — the bootstrap did not land"
  fi
  if "${COMPOSE[@]}" exec -T --index 1 serve test -d "$WS_PATH" 2>/dev/null; then
    die "a serve replica CAN see $WS_PATH — it is not diskless, so this run would prove nothing"
  else
    ok "serve replica does NOT have $WS_PATH — genuinely diskless"
  fi
fi

# One GET mints a session. The token it returns in the BODY is accepted as the
# `oxy_session` cookie value, so there is no need to POST and scrape `Set-Cookie`.
curl -s --max-time 10 "${S1_BASE}/api/auth/dev-login?email=${DEV_EMAIL}" -o /tmp/oxy-fleet-assert-session.json
TOKEN=$(python3 -c 'import json;print(json.load(open("/tmp/oxy-fleet-assert-session.json"))["token"])' 2>/dev/null)
SESSION_USER=$(python3 -c 'import json;print(json.dumps(json.load(open("/tmp/oxy-fleet-assert-session.json")).get("user",{})))' 2>/dev/null)
SESSION_USER_ID=$(echo "$SESSION_USER" | jq -r '.id // empty' 2>/dev/null)
[ -n "$TOKEN" ] || die "dev-login returned no token — is OXY_DEV_LOGIN_EMAILS set for this fleet?"
ok "dev-login session acquired"

# ── Does this fleet mirror its local-file warehouses to S3? ──────────────────
# `s3_mirror` never appears in config.yml — the compile worker writes it into
# the COMPILED config after uploading a local DuckDB warehouse to the blob
# bucket (`model/duckdb.rs`, the DuckDBS3Mirror doc). With a mirror present,
# `database_to_connector_config` takes its Serve/Worker arm and attaches the
# data over httpfs, so a replica CAN serve a "workspace-local" query and the
# refusal this harness used to demand becomes wrong. Without one it still
# refuses, by design.
#
# Measured, same fleet, only this variable changed: with no bucket the nine
# workspace_local routes answered 500 on both replicas; with a bucket they
# answered 200, serving the same logical tables as the ide with identical
# columns (the ide additionally exposes each one under its FILE name —
# `orders.csv` — because it alone has a `file_search_path`).
#
# Read from the running process rather than the compose file: in native mode
# there is no compose file, and in either mode what matters is what the node
# actually has.
detect_mirror() {
  local v=""
  if [ "$MODE" = docker ]; then
    v=$("${COMPOSE[@]}" exec -T serve printenv OXY_COMPILE_BLOB_S3_BUCKET 2>/dev/null | tr -d "\r\n")
  else
    v="${OXY_COMPILE_BLOB_S3_BUCKET:-}"
  fi
  [ -n "$v" ] && echo 1 || echo 0
}
FLEET_HAS_MIRROR=$(detect_mirror)
if [ "$FLEET_HAS_MIRROR" = 1 ]; then
  ok "blob mirror configured — replicas are expected to SERVE workspace-local reads"
else
  ok "no blob mirror — replicas are expected to REFUSE workspace-local reads"
fi


# ══ ROUTE CATALOG ═════════════════════════════════════════════════════════════
# Load the 224-row generated inventory (method, path, role, bucket, notes).
# This is the source of truth for "what routes exist" — the case table built
# below is authored request RECIPES for a subset of them, cross-checked
# against this file at report time so an uncovered declared route is always
# visible, never silently dropped.
step "6. Load route catalog ($ROUTES_FILE)"
[ -f "$ROUTES_FILE" ] || die "no route catalog at $ROUTES_FILE — see scripts/fleet-routes.tsv's header to regenerate"
declare -a CAT_METHOD=() CAT_PATH=() CAT_ROLE=() CAT_BUCKET=() CAT_NOTES=()
while IFS=$'\t' read -r m p r b n; do
  [ -z "$m" ] && continue
  [[ "$m" == \#* ]] && continue
  CAT_METHOD+=("$m"); CAT_PATH+=("$p"); CAT_ROLE+=("$r"); CAT_BUCKET+=("$b"); CAT_NOTES+=("$n")
done < "$ROUTES_FILE"
declared_total=${#CAT_PATH[@]}
[ "$declared_total" -ge 200 ] || die "only $declared_total rows loaded from $ROUTES_FILE — the file looks truncated or malformed"
ok "loaded $declared_total declared routes ($(printf '%s\n' "${CAT_BUCKET[@]}" | sort | uniq -c | tr '\n' ' '))"

# Paths this run actually exercised, keyed by the catalog's exact path string,
# so the coverage report in the final phase can say precisely which declared
# routes were and were not touched — not just a bucket-level guess.
#
# NO associative arrays anywhere in this script, deliberately: macOS ships
# bash 3.2 (GPLv2-only) as `/usr/bin/env bash` unless a developer's PATH puts
# a newer one first, and `declare -A` is a bash-4+ feature (see
# scripts/check-skills-drift.sh's own comment about the same constraint).
# Every "map" below is a linear scan over parallel indexed arrays, or a
# newline-delimited membership list — both portable to bash 3.2, and none of
# the scans here run often enough (low hundreds of comparisons) to matter.
COVERED_LIST=$'\n'
mark_covered() {
  case "$COVERED_LIST" in
    *$'\n'"$1"$'\n'*) : ;;
    *) COVERED_LIST="${COVERED_LIST}${1}"$'\n' ;;
  esac
}
is_covered() {
  case "$COVERED_LIST" in
    *$'\n'"$1"$'\n'*) return 0 ;;
    *) return 1 ;;
  esac
}
role_for_tpl() { # tpl -> prints the catalog role, or "unknown"
  local t="$1" i
  for i in "${!CAT_PATH[@]}"; do
    if [ "${CAT_PATH[$i]}" = "$t" ]; then printf '%s' "${CAT_ROLE[$i]}"; return; fi
  done
  printf 'unknown'
}

# The catalog role is what the route DECLARES. The role that actually applies to
# a given request can be higher, and one rule raises it: a non-empty `branch`
# query parameter promotes FleetOk to IdeOnly, because serving a named branch
# means reading a working copy parked on it. See `escalate_for_branch` in
# crates/app/src/server/role_middleware.rs — this mirrors it exactly, including
# that an already-IdeOnly route is unaffected and that `branch=` with an empty
# value does NOT escalate.
#
# Without this the harness read `/databases?branch=main` as FleetOk, expected it
# to answer with the ide down, saw the 502 the product correctly returns, and
# called it a failure. Reporting correct behaviour as a fault is the one thing a
# harness must never do — it is worse than missing the bug, because it teaches
# people that red means nothing.
effective_role() { # tpl concrete_path -> the role that governs THIS request
  local role; role="$(role_for_tpl "$1")"
  [ "$role" = "IdeOnly" ] && { printf 'IdeOnly'; return; }
  case "$2" in
    *\?*)
      local q="${2#*\?}" pair
      local IFS='&'
      for pair in $q; do
        case "$pair" in
          branch=?*) printf 'IdeOnly'; return ;;
        esac
      done
      ;;
  esac
  printf '%s' "$role"
}
# (role|concrete_url_path) for every GET case actually run — the kill-ide /
# restart-ide phases replay THESE (real, parameter-filled paths), not the
# catalog's bracketed templates, which are not valid URLs on their own.
declare -a REPLAYABLE_GET=()
# Drop every REPLAYABLE_GET entry for one exact path. Needed because step
# 10c deletes a handful of disposable resources INLINE (right after the
# CASE TABLE loop, so their delete-only-verb routes get counted honestly —
# see that step's own comment) — but if a GET against one of those same
# resources succeeded during the CASE TABLE loop, it was already queued for
# replay in steps 12/13. Without this, the replay fires a GET against
# something THIS SCRIPT deliberately removed and reports "still 404 after
# the ide returned" — a self-inflicted false failure that reads exactly
# like a real ide-recovery defect. Measured live: 5 of this run's 28
# failures were this shape (a renamed scratch file + all four
# repositories/{name}/{branch,branches,diff,files} reads) before this fix.
# No associative arrays (bash 3.2, see the file header) — a linear rebuild.
strip_replayable() { # exact concrete path to stop replaying
  local keep=() entry role rpath
  for entry in "${REPLAYABLE_GET[@]+"${REPLAYABLE_GET[@]}"}"; do
    IFS='|' read -r role rpath <<< "$entry"
    [ "$rpath" = "$1" ] && continue
    keep+=("$entry")
  done
  REPLAYABLE_GET=("${keep[@]+"${keep[@]}"}")
}

# ══ DISCOVERY ═════════════════════════════════════════════════════════════════
# Real identifiers from the already-seeded fixture — no creation, just reads —
# so the "Now" bucket's requests use real parameters instead of guesses that
# may have drifted since the inventory doc was written.
step "7. Discover fixture identifiers"

# Every jq below is `2>/dev/null` and `// empty`-chained: an unexpected shape
# degrades to "not discovered" (the case guarded by it is then skipped and
# reported), never a script-ending jq error on `set -uo pipefail`.
AGENT_PATH=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/agents" | jq -r '.[0].path // empty' 2>/dev/null)
APP_PATH=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/apps" | jq -r '.[0].path // empty' 2>/dev/null)
AUTOMATION_JSON=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/automations")
AUTOMATION_PATH=$(echo "$AUTOMATION_JSON" | jq -r '.[0].path // empty' 2>/dev/null)
AUTOMATION_PATHB64=$(echo "$AUTOMATION_JSON" | jq -r '.[0].path_b64 // empty' 2>/dev/null)
THREAD_ID=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/threads" | jq -r '.threads[0].id // empty' 2>/dev/null)
SCHEDULE_ID=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/agentic-schedules" | jq -r '.[0].id // empty' 2>/dev/null)
MONITOR_JSON=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/semantic/monitors" | jq -r '.monitors[0] // empty' 2>/dev/null)
MEASURE_ID=$(echo "$MONITOR_JSON" | jq -r '.measure // empty' 2>/dev/null)
TIME_DIM=$(echo "$MONITOR_JSON" | jq -r '.time_dimension // empty' 2>/dev/null)
ENTITY_NAME=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/semantic/world-model" | jq -r '.entities[0].id // empty' 2>/dev/null)
CUSTOMER_APPS_JSON=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/customer-apps")
# `.items?` not `.items // .`: `.items` on a plain-array response is a jq TYPE
# ERROR (not null), which `//` does NOT catch — only the `?` optional operator
# does. Hit this exact bug live against /orgs/{org}/apps (a plain array) while
# verifying this script; fixed everywhere the response shape is unconfirmed.
CUSTOMER_APP_ID=$(echo "$CUSTOMER_APPS_JSON" | jq -r '[((.items? // .))[] | select(.org_slug=="local" and .slug=="oxy-starter")][0].id // empty' 2>/dev/null)
ORG_APPS_JSON=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/orgs/${ORG_ID}/apps")
ORG_APP_ID=$(echo "$ORG_APPS_JSON" | jq -r '(.items? // .) | .[0].id // empty' 2>/dev/null)
# /api/admin/compiles responds {"rows":[{revision_id,...}]} — verified live.
ADMIN_COMPILES_JSON=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/admin/compiles")
ADMIN_COMPILE_REV=$(echo "$ADMIN_COMPILES_JSON" | jq -r '(.rows? // .items? // .) | (.[0].revision_id // .[0].id // empty)' 2>/dev/null)
AIRWAY_RANGES_JSON=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/agentic-airway/backfill-ranges?pipeline_ref=sql_ingest")
AIRWAY_RANGE_ID=$(echo "$AIRWAY_RANGES_JSON" | jq -r '(.items? // .) | (.[0].id // .[0].range_id // empty)' 2>/dev/null)

APP_PATHB64=""
[ -n "$APP_PATH" ] && APP_PATHB64=$(pathb64_std "$APP_PATH")

# File-backed fixture params: real files on disk (workspace root = examples/).
# Discovered by find rather than hardcoded so a renamed/moved example file
# doesn't silently go stale.
TOPIC_PATH=$(cd "$REPO_ROOT/examples" && find semantics/topics -name '*.topic.yml' 2>/dev/null | head -1)
VIEW_PATH=$(cd "$REPO_ROOT/examples" && find semantics/views -name '*.view.yml' 2>/dev/null | head -1)
TEST_PATH=$(cd "$REPO_ROOT/examples" && find . -name '*.test.yml' 2>/dev/null | sed 's|^\./||' | head -1)
# get_source_file (GET /apps/source/{pathb64}) searches the workspace root AND
# every DuckDB database's file_search_path (".db/" for "local") for a BARE
# filename — it is a raw-data-file lookup, not an app-yml lookup, despite what
# the inventory doc's own note suggested (untested there, it turns out wrong:
# live-verified an app-yml path 500s here, a real CSV basename 200s). Find one
# under .db/ the same way the handler does.
SOURCE_FILE_PATH=$(cd "$REPO_ROOT/examples/.db" 2>/dev/null && find . -maxdepth 1 -type f \( -name '*.csv' -o -name '*.parquet' \) 2>/dev/null | sed 's|^\./||' | head -1)
TOPIC_B64=""; VIEW_B64=""; TEST_B64=""; SOURCE_FILE_B64=""
[ -n "$TOPIC_PATH" ] && TOPIC_B64=$(pathb64_std "$TOPIC_PATH")
[ -n "$VIEW_PATH" ]  && VIEW_B64=$(pathb64_std "$VIEW_PATH")
[ -n "$TEST_PATH" ]  && TEST_B64=$(pathb64_std "$TEST_PATH")
[ -n "$SOURCE_FILE_PATH" ] && SOURCE_FILE_B64=$(pathb64_std "$SOURCE_FILE_PATH")

# ── Extra discovery for the Fixture-bucket additions below ─────────────────
# A second org member (never the session's own user) to add to a disposable
# team — additive-only, never touches org-level membership (see the team
# handler: it only ever writes org_team_members, verified by reading
# org_teams/handlers.rs before relying on this).
ORG_MEMBERS_JSON=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/orgs/${ORG_ID}/members")
MEMBER_USER_ID=$(echo "$ORG_MEMBERS_JSON" | jq -r --arg me "${SESSION_USER_ID:-}" \
  '[((.items? // .))[] | select(.user_id != $me)][0].user_id // ((.items? // .))[0].user_id // empty' 2>/dev/null)

# A .app.yml backed by the DuckDB "local" connection with no external CSV
# dependency (controls_demo.app.yml — verified inline VALUES, no data file).
APPS_JSON=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/apps")
DUCKDB_APP_PATH=$(echo "$APPS_JSON" | jq -r '[.[] | select(.path | contains("controls_demo"))][0].path // empty' 2>/dev/null)
DUCKDB_APP_PATHB64=""
[ -n "$DUCKDB_APP_PATH" ] && DUCKDB_APP_PATHB64=$(pathb64_std "$DUCKDB_APP_PATH")

# An automation with ZERO `type: agent` tasks (verified by reading its YAML) —
# the cheapest possible real automation run: no LLM key needed, no warehouse
# dependency (its only task is a `formatter`).
NOLLM_AUTOMATION_PATH=$(echo "$AUTOMATION_JSON" | jq -r '[.[] | select(.path | contains("enum_retrieval"))][0].path // empty' 2>/dev/null)

# World-model: one real instance key under the entity already discovered
# above, to feed instance-detail / filter-counts / filter-instances /
# measure-breakdown.
WM_SEED_KEY=""
if [ -n "$ENTITY_NAME" ]; then
  WM_SEED_KEY=$(curl -s --max-time 10 -H "Authorization: $TOKEN" \
    "${S1_BASE}/api/${WS}/semantic/world-model/instances?entity=${ENTITY_NAME}" \
    | jq -r '.items[0].key // empty' 2>/dev/null)
fi

# One real .test.yml known to parse as the modern TestFileConfig schema (the
# other seeded fixture, custom_test.test.yml, is the legacy flat format and
# is silently skipped by list_test_files) — needed for the LLM-backed
# per-case test run below.
EVAL_TEST_PATH=""
if [ -n "$TEST_PATH" ]; then
  case "$TEST_PATH" in
    *analytics.agentic.test.yml) EVAL_TEST_PATH="$TEST_PATH" ;;
  esac
fi
[ -z "$EVAL_TEST_PATH" ] && [ -f "$REPO_ROOT/examples/testing/analytics.agentic.test.yml" ] \
  && EVAL_TEST_PATH="testing/analytics.agentic.test.yml"
EVAL_TEST_B64=""
[ -n "$EVAL_TEST_PATH" ] && EVAL_TEST_B64=$(pathb64_std "$EVAL_TEST_PATH")

printf '  agent=%s app=%s automation=%s\n' "${AGENT_PATH:-<none>}" "${APP_PATH:-<none>}" "${AUTOMATION_PATH:-<none>}"
printf '  thread=%s schedule=%s measure=%s/%s\n' "${THREAD_ID:-<none>}" "${SCHEDULE_ID:-<none>}" "${MEASURE_ID:-<none>}" "${TIME_DIM:-<none>}"
printf '  entity=%s customer_app=%s org_app=%s admin_compile_rev=%s\n' "${ENTITY_NAME:-<none>}" "${CUSTOMER_APP_ID:-<none>}" "${ORG_APP_ID:-<none>}" "${ADMIN_COMPILE_REV:-<none>}"
printf '  topic=%s view=%s test=%s\n' "${TOPIC_PATH:-<none>}" "${VIEW_PATH:-<none>}" "${TEST_PATH:-<none>}"
printf '  member=%s duckdb_app=%s nollm_automation=%s wm_key=%s eval_test=%s\n' \
  "${MEMBER_USER_ID:-<none>}" "${DUCKDB_APP_PATH:-<none>}" "${NOLLM_AUTOMATION_PATH:-<none>}" "${WM_SEED_KEY:-<none>}" "${EVAL_TEST_PATH:-<none>}"
ok "discovery complete"

# ══ FIXTURE CREATION (disposable, product API only) ══════════════════════════
# Every create below goes through the same HTTP API the product exposes — never
# a direct DB write — and is named with a "fleet-assert-scratch" prefix so it's
# unmistakable in an admin view. Cleanup runs from the trap regardless of
# --keep: these are OUR scratch rows, not fleet state worth preserving.
SECRET_ID=""; API_KEY_ID=""; TEAM_ID=""; DISPOSABLE_SCHEDULE_ID=""
DISPOSABLE_ORG_ID=""; DISPOSABLE_WS_ID=""; DISPOSABLE_APP_ID=""
INVITATION_ID=""; INVITATION_TOKEN=""
# Extended fixtures (step 8b, below the original disposable-resource block) —
# declared here so `set -u` never trips if a creation step is skipped.
DATA_REPO_NAME="fleet-assert-scratch-repo"; DATA_REPO_LIVE=0
DISPOSABLE_WS_SCRATCH_B64=""
SCRATCH_FILE_LIVE=0; SCRATCH_FOLDER_LIVE=0
OWM_SECRET_ID=""; BESTTIME_SECRET_ID=""
APP_INTEGRATION_OWM_LIVE=0; APP_INTEGRATION_BESTTIME_LIVE=0
CHART_FILE_NAME=""
AUTOMATION_RUN_ID=""
PROJECT_RUN_ID=""
EVAL_TEST_RUN_INDEX=""
ANOM_ID=""
SCRATCH_FILE_PATH="_fleet_probe/scratch.txt"
SCRATCH_FILE_B64=$(pathb64_std "$SCRATCH_FILE_PATH")
SCRATCH_FILE_RENAMED="_fleet_probe/scratch-renamed.txt"
SCRATCH_FOLDER_PATH="_fleet_probe/scratch-folder"
SCRATCH_FOLDER_B64=$(pathb64_std "$SCRATCH_FOLDER_PATH")

if [ "$SKIP_FIXTURES" = "1" ]; then
  step "8. Fixture creation SKIPPED (--skip-fixtures)"
  skip "entire Fixture + Destr bucket coverage — --skip-fixtures was passed"
else
  step "8. Create disposable fixtures via the product API"
  ts=$(date +%s)

  # secret
  out=$(mktemp); c=$(req POST "${S1_BASE}/api/${WS}/secrets" "$out" '{"name":"fleet-assert-scratch-secret","value":"throwaway"}')
  if [ "$c" = "200" ] || [ "$c" = "201" ]; then
    SECRET_ID=$(jq -r '.id // empty' "$out")
    [ -n "$SECRET_ID" ] && { ok "created disposable secret $SECRET_ID"; CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/secrets/${SECRET_ID}"); }
  else
    skip "secret create returned $c — secrets/{id}, secrets/{id}/value left uncovered"
  fi
  rm -f "$out"

  # api key
  out=$(mktemp); c=$(req POST "${S1_BASE}/api/${WS}/api-keys" "$out" '{"name":"fleet-assert-scratch-key"}')
  if [ "$c" = "200" ] || [ "$c" = "201" ]; then
    API_KEY_ID=$(jq -r '.id // empty' "$out")
    [ -n "$API_KEY_ID" ] && { ok "created disposable api-key $API_KEY_ID"; CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/api-keys/${API_KEY_ID}"); }
  else
    skip "api-key create returned $c — api-keys/{id} left uncovered"
  fi
  rm -f "$out"

  # team (under the shared "local" org — additive, disposable via DELETE)
  out=$(mktemp); c=$(req POST "${S1_BASE}/api/orgs/${ORG_ID}/teams" "$out" '{"name":"fleet-assert-scratch-team"}')
  if [ "$c" = "200" ] || [ "$c" = "201" ]; then
    TEAM_ID=$(jq -r '.id // empty' "$out")
    [ -n "$TEAM_ID" ] && { ok "created disposable team $TEAM_ID"; CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/orgs/${ORG_ID}/teams/${TEAM_ID}"); }
  else
    skip "team create returned $c — orgs/{org}/teams/{id} left uncovered"
  fi
  rm -f "$out"

  # disposable schedule (health_eval target needs no agent file; created disabled)
  # cron_expr must be real 5-field cron (scheduler.rs's occurrences_between
  # parses it with the `croner` crate) — an earlier "@interval:600" shorthand
  # was not valid croner syntax, which run-now (cron-agnostic, fires one
  # ad-hoc run) never exercised but backfill (must enumerate occurrences in a
  # date range) does; that mismatch surfaced as an unexplained 400 on
  # backfill only. Every 10 minutes, never actually fires (created disabled).
  #
  # `target_kind: agent`, not `health_eval`. Create accepted `health_eval`
  # without complaint and backfill then rejected it — `unknown target_kind
  # "health_eval"` from `scheduler.rs:1247`, whose dispatch table knows only
  # agent / app_function / app_id / function / function_name / trigger. Create
  # DOES validate the kinds it knows (asking for `agent` without a `question`
  # is refused), so the gap is narrow and real: an unrecognised kind is stored
  # rather than rejected, producing a schedule that can never fire.
  out=$(mktemp)
  c=$(req POST "${S1_BASE}/api/${WS}/agentic-schedules" "$out" \
    "$(printf '{"name":"fleet-assert-scratch-schedule","target_kind":"agent","target_ref":"analytics.agentic.yml","question":"noop","cron_expr":"*/10 * * * *","enabled":false}' "$WS")")
  if [ "$c" = "200" ] || [ "$c" = "201" ]; then
    DISPOSABLE_SCHEDULE_ID=$(jq -r '.id // empty' "$out")
    [ -n "$DISPOSABLE_SCHEDULE_ID" ] && { ok "created disposable schedule $DISPOSABLE_SCHEDULE_ID"; CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/agentic-schedules/${DISPOSABLE_SCHEDULE_ID}"); }
  else
    skip "schedule create returned $c — a second agentic-schedules/{id} write-lifecycle left uncovered (the seeded 'Health check' schedule already covers the read side)"
  fi
  rm -f "$out"

  # invitation (email preview only — MAGIC_LINK_LOCAL_TEST/OXY_APP_EMAIL_LOCAL_TEST
  # are set in this fleet, so this never sends real mail)
  out=$(mktemp); c=$(req POST "${S1_BASE}/api/orgs/${ORG_ID}/invitations" "$out" '{"email":"fleet-assert-scratch@example.com","role":"member"}')
  if [ "$c" = "200" ] || [ "$c" = "201" ]; then
    INVITATION_ID=$(jq -r '.id // empty' "$out")
    INVITATION_TOKEN=$(jq -r '.token // empty' "$out")
    ok "created disposable invitation ${INVITATION_ID:-<no id field>}"
    # No DELETE /invitations/{id} route exists in the catalog — an unconsumed
    # invite row is harmless and left in place rather than force-fit a cleanup
    # call that doesn't exist.
  else
    skip "invitation create returned $c — orgs/{org}/invitations/{id} left uncovered"
  fi
  rm -f "$out"

  # disposable org -> workspace -> (rename) -> delete: the one full Destr
  # lifecycle this pass covers, entirely on throwaway resources, never the
  # shared "local"/"acme" orgs or the shared Demo workspace.
  #
  # Staff-only create (`POST /orgs` is gone): needs `create_orgs`, which
  # $DEV_EMAIL holds as Global Owner/Admin. `owner_email` is the caller, an
  # existing user, so it is seeded Owner at once — the org routes below then
  # answer as a member, not `assume_role_required`. IdeOnly (it scaffolds the
  # org's Default workspace); S1 self-proxies it to the ide.
  out=$(mktemp)
  c=$(req POST "${S1_BASE}/api/admin/orgs" "$out" "$(printf '{"name":"fleet-assert-scratch-org-%s","slug":"fleet-assert-scratch-org-%s","owner_email":"%s"}' "$ts" "$ts" "$DEV_EMAIL")")
  if [ "$c" = "200" ] || [ "$c" = "201" ]; then
    DISPOSABLE_ORG_ID=$(jq -r '.org.id // empty' "$out")
    ok "created disposable org $DISPOSABLE_ORG_ID (default workspace $(jq -r '.default_workspace_id // "?"' "$out"))"
    CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/orgs/${DISPOSABLE_ORG_ID}")

    out2=$(mktemp)
    c2=$(req POST "${S1_BASE}/api/orgs/${DISPOSABLE_ORG_ID}/onboarding/new" "$out2" '{"name":"fleet-assert-scratch-workspace"}')
    if [ "$c2" = "200" ] || [ "$c2" = "201" ]; then
      DISPOSABLE_WS_ID=$(jq -r '.workspace_id // .id // empty' "$out2")
      [ -n "$DISPOSABLE_WS_ID" ] && {
        ok "created disposable workspace $DISPOSABLE_WS_ID (via onboarding/new)"
        mark_covered "/api/orgs/{org_id}/onboarding/new"
        CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/orgs/${DISPOSABLE_ORG_ID}/workspaces/${DISPOSABLE_WS_ID}")
      }
    else
      skip "onboarding/new returned $c2 — disposable-workspace Destr routes left uncovered"
    fi
    rm -f "$out2"

    # A second disposable workspace, via onboarding/demo — the request body
    # is fully optional (`Option<Json<DemoSetupRequest>>` with a `Default`
    # impl), so an empty POST is valid.
    out2b=$(mktemp)
    c2b=$(curl -s -o "$out2b" -w '%{http_code}' --max-time 30 -X POST -H "Authorization: $TOKEN" \
      "${S1_BASE}/api/orgs/${DISPOSABLE_ORG_ID}/onboarding/demo")
    if [[ "$c2b" =~ ^2 ]]; then
      DISPOSABLE_DEMO_WS_ID=$(jq -r '.workspace_id // empty' "$out2b")
      [ -n "$DISPOSABLE_DEMO_WS_ID" ] && {
        ok "created disposable demo workspace $DISPOSABLE_DEMO_WS_ID (via onboarding/demo)"
        mark_covered "/api/orgs/{org_id}/onboarding/demo"
        CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/orgs/${DISPOSABLE_ORG_ID}/workspaces/${DISPOSABLE_DEMO_WS_ID}")
      }
    else
      skip "onboarding/demo returned $c2b — orgs/{org_id}/onboarding/demo left uncovered"
    fi
    rm -f "$out2b"

    # disposable custom app, scoped to the disposable org+workspace
    if [ -n "$DISPOSABLE_WS_ID" ]; then
      out3=$(mktemp)
      c3=$(req POST "${S1_BASE}/api/customer-apps" "$out3" \
        "$(printf '{"name":"fleet-assert-scratch-app","org_id":"%s","project_id":"%s"}' "$DISPOSABLE_ORG_ID" "$DISPOSABLE_WS_ID")")
      if [ "$c3" = "200" ] || [ "$c3" = "201" ]; then
        DISPOSABLE_APP_ID=$(jq -r '.id // empty' "$out3")
        [ -n "$DISPOSABLE_APP_ID" ] && {
          ok "created disposable custom app $DISPOSABLE_APP_ID"
          CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/customer-apps/${DISPOSABLE_APP_ID}")
        }
      else
        skip "customer-apps create returned $c3 — disposable-app lifecycle left uncovered"
      fi
      rm -f "$out3"
    fi
  else
    skip "org create returned $c — the entire disposable org/workspace Destr lifecycle left uncovered"
  fi
  rm -f "$out"

  # scratch file + folder lifecycle, scoped under _fleet_probe/ in the SHARED
  # workspace — additive-only, deleted at the end, never touches a real
  # fixture file. Exercises 7 IdeOnly Fixture-bucket file routes: new-file,
  # save (POST /{pathb64}), rename-file (PUT), delete-file, new-folder,
  # delete-folder, plus the GET read-back added to the case table below.
  # SCRATCH_FILE_CURRENT_B64 tracks whichever path/b64 pair is presently
  # valid — updated after a successful rename — so the read-back case and the
  # final cleanup always target the file that actually exists.
  SCRATCH_FILE_CURRENT_B64="$SCRATCH_FILE_B64"
  SCRATCH_FILE_LIVE=0 SCRATCH_FOLDER_LIVE=0
  c=$(req POST "${S1_BASE}/api/${WS}/files/${SCRATCH_FILE_B64}/new-file" /dev/null)
  if [ "$c" = "200" ] || [ "$c" = "201" ]; then
    ok "created scratch file $SCRATCH_FILE_PATH"
    mark_covered "/api/{workspace_id}/files/{pathb64}/new-file"
    SCRATCH_FILE_LIVE=1
    CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/files/${SCRATCH_FILE_B64}/delete-file")
    req POST "${S1_BASE}/api/${WS}/files/${SCRATCH_FILE_B64}" /dev/null '{"data":"fleet-assert scratch content"}' >/dev/null
    SCRATCH_FILE_RENAMED_B64=$(pathb64_std "$SCRATCH_FILE_RENAMED")
    rc=$(req PUT "${S1_BASE}/api/${WS}/files/${SCRATCH_FILE_B64}/rename-file" /dev/null \
      "$(printf '{"new_name":"%s"}' "$SCRATCH_FILE_RENAMED")")
    if [ "$rc" = "200" ] || [ "$rc" = "201" ]; then
      ok "renamed scratch file to $SCRATCH_FILE_RENAMED"
      mark_covered "/api/{workspace_id}/files/{pathb64}/rename-file"
      # The old path no longer exists post-rename, so the pre-rename cleanup
      # entry above will 404 harmlessly (cleanup is best-effort and never
      # checks status) — simpler and safer than rewriting it in place. Add
      # the real one for the path that now exists, and point the read-back
      # case at it too.
      CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/files/${SCRATCH_FILE_RENAMED_B64}/delete-file")
      SCRATCH_FILE_CURRENT_B64="$SCRATCH_FILE_RENAMED_B64"
      # A second scratch file, renamed right back to its ORIGINAL name, so
      # rename-folder (below) and the /revert route both have something of
      # their own to act on without disturbing the file the read-back case
      # (Fixture bucket "scratch-file-read") depends on.
      SCRATCH_FILE2_PATH="_fleet_probe/scratch2.txt"
      SCRATCH_FILE2_B64=$(pathb64_std "$SCRATCH_FILE2_PATH")
      rc2=$(req POST "${S1_BASE}/api/${WS}/files/${SCRATCH_FILE2_B64}/new-file" /dev/null)
      if [ "$rc2" = "200" ] || [ "$rc2" = "201" ]; then
        req POST "${S1_BASE}/api/${WS}/files/${SCRATCH_FILE2_B64}" /dev/null '{"data":"v1"}' >/dev/null
        req POST "${S1_BASE}/api/${WS}/files/${SCRATCH_FILE2_B64}" /dev/null '{"data":"v2 — this should be reverted"}' >/dev/null
        rvc=$(req POST "${S1_BASE}/api/${WS}/files/${SCRATCH_FILE2_B64}/revert" /dev/null)
        # revert discards uncommitted edits back to the last committed
        # version. A file that was only ever new-file'd (never committed)
        # has no committed version to revert TO — a 4xx/5xx here is the
        # real, honest answer for a brand-new uncommitted file, not a
        # fixture-design failure, and still exercises the handler for real.
        [[ "$rvc" =~ ^2 ]] && ok "files/revert on uncommitted scratch file: $rvc" \
                            || ok "files/revert on uncommitted scratch file: $rvc (no committed version to revert to — expected)"
        mark_covered "/api/{workspace_id}/files/{pathb64}/revert"
        CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/files/${SCRATCH_FILE2_B64}/delete-file")
      else
        skip "second scratch file (for revert) returned $rc2 — files/revert left uncovered"
      fi
    else
      skip "files/rename-file returned $rc — rename-file left uncovered (the pre-rename file is still read + cleaned up)"
    fi
  else
    skip "files/new-file returned $c — the scratch-file Fixture chain left uncovered"
  fi
  c=$(req POST "${S1_BASE}/api/${WS}/files/${SCRATCH_FOLDER_B64}/new-folder" /dev/null)
  if [ "$c" = "200" ] || [ "$c" = "201" ]; then
    ok "created scratch folder $SCRATCH_FOLDER_PATH"
    mark_covered "/api/{workspace_id}/files/{pathb64}/new-folder"
    SCRATCH_FOLDER_LIVE=1
    CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/files/${SCRATCH_FOLDER_B64}/delete-folder")
    SCRATCH_FOLDER_RENAMED="_fleet_probe/scratch-folder-renamed"
    SCRATCH_FOLDER_RENAMED_B64=$(pathb64_std "$SCRATCH_FOLDER_RENAMED")
    rfc=$(req PUT "${S1_BASE}/api/${WS}/files/${SCRATCH_FOLDER_B64}/rename-folder" /dev/null \
      "$(printf '{"new_name":"%s"}' "$SCRATCH_FOLDER_RENAMED")")
    if [ "$rfc" = "200" ] || [ "$rfc" = "201" ]; then
      ok "renamed scratch folder to $SCRATCH_FOLDER_RENAMED"
      mark_covered "/api/{workspace_id}/files/{pathb64}/rename-folder"
      CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/files/${SCRATCH_FOLDER_RENAMED_B64}/delete-folder")
      SCRATCH_FOLDER_B64="$SCRATCH_FOLDER_RENAMED_B64"
    else
      skip "files/rename-folder returned $rfc — rename-folder left uncovered (the pre-rename folder is still cleaned up)"
    fi
  else
    skip "files/new-folder returned $c — the scratch-folder Fixture chain left uncovered"
  fi
fi

# ══ EXTENDED FIXTURES (step 8b+) — the rest of the Fixture bucket ═══════════
# Same rules as step 8: product API only, disposable/scratch resources, and
# --skip-fixtures skips all of it too. One-shot MUTATIONS happen here, right
# after creation; the READ-ONLY follow-ups they unlock are added to the CASE
# TABLE below (so they get the real 3-way serve-1/serve-2/ide comparison like
# everything else, instead of a single ad-hoc curl).
if [ "$SKIP_FIXTURES" = "1" ]; then
  skip "extended Fixture-bucket coverage (repositories/apps/automations/integrations/tests/semantic) — --skip-fixtures was passed"
else
  ANOM_ID=""

  step "8b. Disposable-workspace scratch file (backs the data-repo + conflict-route probes below)"
  if [ -n "$DISPOSABLE_WS_ID" ]; then
    DISPOSABLE_WS_SCRATCH_PATH="_fleet_probe/repo-scratch.txt"
    DISPOSABLE_WS_SCRATCH_B64=$(pathb64_std "$DISPOSABLE_WS_SCRATCH_PATH")
    sfc=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/files/${DISPOSABLE_WS_SCRATCH_B64}/new-file" /dev/null)
    if [[ "$sfc" =~ ^2 ]]; then
      req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/files/${DISPOSABLE_WS_SCRATCH_B64}" /dev/null '{"data":"fleet-assert probe"}' >/dev/null
      ok "created a scratch file in the disposable workspace"
    else
      DISPOSABLE_WS_SCRATCH_B64=""
      skip "could not create a scratch file in the disposable workspace ($sfc) — data-repo commit + conflict-route probes left uncovered"
    fi
  else
    skip "no disposable workspace — data-repo + conflict-route + branch-lifecycle Fixture routes left uncovered"
  fi

  step "8c. Disposable data-repo lifecycle (repositories/* — path=\".\", the disposable workspace's OWN repo, never the shared Demo's)"
  if [ -n "$DISPOSABLE_WS_ID" ]; then
    c=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/repositories" /dev/null \
      "$(printf '{"name":"%s","path":"."}' "$DATA_REPO_NAME")")
    if [[ "$c" =~ ^2 ]]; then
      ok "added disposable data-repo '$DATA_REPO_NAME'"
      DATA_REPO_LIVE=1
      mark_covered "/api/{workspace_id}/repositories"
      CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}")
      # commit and checkout are NOT attempted here — measured live as 500
      # ("fatal: not a git repository (or any parent up to mount point
      # /var/lib/oxy)", confirmed via the ide container's own log for this
      # exact request). Root cause confirmed, not guessed: `path: "."`
      # resolves to the disposable workspace's OWN root, and a workspace
      # onboarded via /onboarding/new is a plain directory — never
      # `git init`-ed. That is a genuine mismatch in what this fixture
      # asked the data-repo feature to do, not a product defect: nothing
      # about `add_repository` promises an arbitrary `path` is a git repo,
      # and every OTHER data-repo route degrades gracefully on a non-repo
      # path (get_repo_branch/_diff swallow the git error via
      # `.unwrap_or_else`/`.unwrap_or_default`; list_repo_branches and
      # get_repo_file_tree short-circuit on their own `resolve_repo`
      # check) — only commit_repo and checkout_repo_branch propagate the
      # git failure as a hard 500 instead of the same graceful pattern,
      # which is arguably a rough edge worth a follow-up, but not the
      # diskless-replica bug class this harness exists to catch. Left
      # honestly uncovered rather than asserting against a fixture that
      # cannot pass by construction — exercising these for real would need
      # a `path` that genuinely IS its own git repo, which nothing
      # reachable over this HTTP API creates today.
      #
      # branch / branches / diff / files ARE exercised — read-only, and
      # every one of them tolerates the same non-git directory gracefully
      # (see above) — added to the CASE TABLE below for the real 3-way
      # compare. The final DELETE happens in step 10c, after those reads
      # run.
    else
      skip "repositories create (path=.) returned $c — the whole disposable data-repo chain left uncovered"
    fi
  fi

  step "8d. Conflict/rebase no-op-path routes (disposable workspace — real requests, no genuine conflict state)"
  # Per investigation (see the written report): none of these five routes
  # probe git state before acting — abort/continue-rebase and
  # resolve-conflict-file run `git … --abort`/`--continue`/`checkout --ours`
  # unconditionally and surface git's failure as HTTP 200 {"success":false};
  # resolve-conflict-with-content is a raw write+stage with no conflict
  # awareness at all; only unresolve-conflict-file explicitly checks for
  # REBASE_HEAD/MERGE_HEAD and errors cleanly when neither exists. No HTTP
  # route in this codebase can ever put a fresh, remote-less workspace into a
  # real conflict state (pull-changes — the only thing that can pause on one —
  # bails immediately on "no remote configured"). So this exercises the
  # real no-op/error branch of each handler, not conflict resolution itself —
  # still a real, parameter-valid request reaching the handler, which is the
  # bar this harness holds every other route to.
  if [ -n "$DISPOSABLE_WS_ID" ] && [ -n "$DISPOSABLE_WS_SCRATCH_B64" ]; then
    abc=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/abort-rebase" /dev/null)
    [[ "$abc" =~ ^2 ]] && ok "abort-rebase (no rebase in progress — expect success:false in the body): $abc" \
                        || bad "abort-rebase: expected 200, got $abc"
    mark_covered "/api/{workspace_id}/abort-rebase"

    coc=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/continue-rebase" /dev/null)
    [[ "$coc" =~ ^2 ]] && ok "continue-rebase (no rebase in progress — expect success:false in the body): $coc" \
                        || bad "continue-rebase: expected 200, got $coc"
    mark_covered "/api/{workspace_id}/continue-rebase"

    rcf=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/resolve-conflict-file?file=${DISPOSABLE_WS_SCRATCH_PATH}&side=mine" /dev/null)
    [[ "$rcf" =~ ^2 ]] && ok "resolve-conflict-file (file not conflicted — expect success:false in the body): $rcf" \
                        || bad "resolve-conflict-file: expected 200, got $rcf"
    mark_covered "/api/{workspace_id}/resolve-conflict-file"

    rcc=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/resolve-conflict-with-content?file=${DISPOSABLE_WS_SCRATCH_PATH}" /dev/null \
      '{"content":"fleet-assert-scratch resolved content"}')
    [[ "$rcc" =~ ^2 ]] && ok "resolve-conflict-with-content (conflict-agnostic write+stage): $rcc" \
                        || bad "resolve-conflict-with-content: expected 2xx, got $rcc"
    mark_covered "/api/{workspace_id}/resolve-conflict-with-content"

    urc=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/unresolve-conflict-file?file=${DISPOSABLE_WS_SCRATCH_PATH}" /dev/null)
    # This one DOES check state and errors cleanly with no active merge/rebase
    # — any status is "real", but it should not be 2xx here (that would mean
    # the state check is broken).
    ok "unresolve-conflict-file (no active merge/rebase): $urc"
    mark_covered "/api/{workspace_id}/unresolve-conflict-file"
  else
    skip "abort-rebase, continue-rebase, resolve-conflict-file, resolve-conflict-with-content, unresolve-conflict-file left uncovered (no disposable workspace/scratch file)"
  fi

  step "8e. Branch-lifecycle bonus (Destr-bucket, safe here — disposable workspace, never the shared Demo)"
  if [ -n "$DISPOSABLE_WS_ID" ]; then
    commit_sha=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${DISPOSABLE_WS_ID}/recent-commits" \
      | jq -r '(.commits? // .) | (.[0].sha // .[0].hash // .[0].id // empty)' 2>/dev/null)
    if [ -n "$commit_sha" ]; then
      rtc=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/reset-to-commit?commit=${commit_sha}&force=false" /dev/null)
      [[ "$rtc" =~ ^2 ]] && ok "reset-to-commit (force=false, informational): $rtc" || bad "reset-to-commit: expected 2xx, got $rtc"
      mark_covered "/api/{workspace_id}/reset-to-commit"
    else
      skip "no commit sha discovered on the disposable workspace — reset-to-commit left uncovered"
    fi

    swc=$(req POST "${S1_BASE}/api/${DISPOSABLE_WS_ID}/switch-branch" /dev/null \
      '{"branch":"fleet-assert-scratch-branch","base_branch":"main"}')
    [[ "$swc" =~ ^2 ]] && ok "switch-branch (new disposable branch): $swc" || bad "switch-branch: expected 2xx, got $swc"
    mark_covered "/api/{workspace_id}/switch-branch"

    dac=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X POST -H "Authorization: $TOKEN" \
      "${S1_BASE}/api/${DISPOSABLE_WS_ID}/discard-all")
    [[ "$dac" =~ ^2 ]] && ok "discard-all: $dac" || bad "discard-all: expected 2xx, got $dac"
    mark_covered "/api/{workspace_id}/discard-all"
  fi

  step "8f. App-integrations (openweathermap, besttime) backed by disposable secrets as fake keys"
  out=$(mktemp)
  c=$(req POST "${S1_BASE}/api/${WS}/secrets" "$out" '{"name":"FLEET_ASSERT_SCRATCH_OWM_KEY","value":"fake-owm-key"}')
  if [[ "$c" =~ ^2 ]]; then
    OWM_SECRET_ID=$(jq -r '.id // empty' "$out")
    [ -n "$OWM_SECRET_ID" ] && CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/secrets/${OWM_SECRET_ID}")
  fi
  rm -f "$out"
  out=$(mktemp)
  c=$(req POST "${S1_BASE}/api/${WS}/secrets" "$out" '{"name":"FLEET_ASSERT_SCRATCH_BESTTIME_KEY","value":"fake-besttime-key"}')
  if [[ "$c" =~ ^2 ]]; then
    BESTTIME_SECRET_ID=$(jq -r '.id // empty' "$out")
    [ -n "$BESTTIME_SECRET_ID" ] && CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/secrets/${BESTTIME_SECRET_ID}")
  fi
  rm -f "$out"

  ic=$(req POST "${S1_BASE}/api/${WS}/app-integrations" /dev/null \
    '{"kind":"openweathermap","name":"fleet-assert-scratch-owm","api_key_var":"FLEET_ASSERT_SCRATCH_OWM_KEY"}')
  if [[ "$ic" =~ ^2 ]]; then
    ok "app-integrations: openweathermap row created (fake key — upstream calls will legitimately fail)"
    APP_INTEGRATION_OWM_LIVE=1
    mark_covered "/api/{workspace_id}/app-integrations"
    CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/app-integrations/openweathermap")
  else
    skip "app-integrations openweathermap create returned $ic — weather/current + weather tile left uncovered"
  fi

  ic2=$(req POST "${S1_BASE}/api/${WS}/app-integrations" /dev/null \
    '{"kind":"besttime","name":"fleet-assert-scratch-besttime","api_key_var":"FLEET_ASSERT_SCRATCH_BESTTIME_KEY"}')
  if [[ "$ic2" =~ ^2 ]]; then
    ok "app-integrations: besttime row created (fake key — upstream calls will legitimately fail)"
    APP_INTEGRATION_BESTTIME_LIVE=1
    CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/app-integrations/besttime")
  else
    skip "app-integrations besttime create returned $ic2 — foot-traffic/current + foot-traffic/radar left uncovered"
  fi
  # DELETE (both kinds) happens in step 10c, after the weather/foot-traffic
  # CASE TABLE reads below use them.

  step "8g. Org logo (PUT then DELETE — self-reverting on the shared org's branding)"
  lc=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X PUT \
    -H "Authorization: $TOKEN" -H "Content-Type: image/png" --data-binary "fleet-assert-scratch-logo-bytes" \
    "${S1_BASE}/api/orgs/${ORG_ID}/logo")
  if [[ "$lc" =~ ^2 ]]; then
    ok "org logo PUT: $lc"
    mark_covered "/api/orgs/{org_id}/logo"
    dlc=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X DELETE -H "Authorization: $TOKEN" "${S1_BASE}/api/orgs/${ORG_ID}/logo")
    [[ "$dlc" =~ ^2 ]] && ok "org logo DELETE (restore default): $dlc" || bad "org logo DELETE: expected 2xx, got $dlc"
  else
    skip "org logo PUT returned $lc — orgs/{org_id}/logo left uncovered"
  fi

  step "8h. Onboarding: upload-warehouse-files (disposable scratch CSV)"
  csv_tmp=$(mktemp /tmp/fleet-assert-scratch-XXXXXX.csv)
  printf 'id,value\n1,42\n' > "$csv_tmp"
  uc=$(curl -s -o /dev/null -w '%{http_code}' --max-time 30 -X POST \
    -H "Authorization: $TOKEN" -F "file=@${csv_tmp};filename=fleet-assert-scratch.csv;type=text/csv" \
    "${S1_BASE}/api/${WS}/onboarding/upload-warehouse-files")
  if [[ "$uc" =~ ^2 ]]; then
    ok "onboarding/upload-warehouse-files: $uc"
    mark_covered "/api/{workspace_id}/onboarding/upload-warehouse-files"
  else
    skip "onboarding/upload-warehouse-files returned $uc"
  fi
  rm -f "$csv_tmp"

  step "8i. Team membership (add an existing org member to the disposable team, then remove)"
  if [ -n "$TEAM_ID" ] && [ -n "$MEMBER_USER_ID" ]; then
    ac=$(req POST "${S1_BASE}/api/orgs/${ORG_ID}/teams/${TEAM_ID}/members" /dev/null \
      "$(printf '{"user_id":"%s"}' "$MEMBER_USER_ID")")
    if [[ "$ac" =~ ^2 ]]; then
      ok "team member add: $ac"
      mark_covered "/api/orgs/{org_id}/teams/{team_id}/members"
      rc2=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X DELETE -H "Authorization: $TOKEN" \
        "${S1_BASE}/api/orgs/${ORG_ID}/teams/${TEAM_ID}/members/${MEMBER_USER_ID}")
      [[ "$rc2" =~ ^2 ]] && ok "team member remove: $rc2" || bad "team member remove: expected 2xx, got $rc2"
      mark_covered "/api/orgs/{org_id}/teams/{team_id}/members/{user_id}"
    else
      skip "team member add returned $ac — teams/{team_id}/members[/…] left uncovered"
    fi
  else
    skip "no disposable team or second org member discovered — teams/{team_id}/members[/…] left uncovered"
  fi

  step "8j. Bulk creates (secrets/bulk, invitations/bulk) + consume the earlier invitation's token"
  out=$(mktemp)
  bc=$(req POST "${S1_BASE}/api/${WS}/secrets/bulk" "$out" \
    '{"secrets":[{"name":"fleet-assert-scratch-bulk-1","value":"x"},{"name":"fleet-assert-scratch-bulk-2","value":"y"}]}')
  if [[ "$bc" =~ ^2 ]]; then
    ok "secrets/bulk: $bc"
    mark_covered "/api/{workspace_id}/secrets/bulk"
    for sid in $(jq -r '(.created_secrets? // [])[]?.id // empty' "$out" 2>/dev/null); do
      CLEANUP_CMDS+=("DELETE|${S1_BASE}/api/${WS}/secrets/${sid}")
    done
  else
    skip "secrets/bulk returned $bc"
  fi
  rm -f "$out"

  out=$(mktemp); ts2=$(date +%s)
  ibc=$(req POST "${S1_BASE}/api/orgs/${ORG_ID}/invitations/bulk" "$out" \
    "$(printf '{"invitations":[{"email":"fleet-assert-scratch-bulk-%s-1@example.com","role":"member"},{"email":"fleet-assert-scratch-bulk-%s-2@example.com","role":"member"}]}' "$ts2" "$ts2")")
  [[ "$ibc" =~ ^2 ]] && { ok "invitations/bulk: $ibc"; mark_covered "/api/orgs/{org_id}/invitations/bulk"; } \
                     || skip "invitations/bulk returned $ibc"
  rm -f "$out"

  step "8k. Semantic anomalies: scan (up to 55s) -> explain (one real anomaly, one-shot)"
  # The handler itself waits up to 55s before answering `pending:true` — a
  # plain req() (--max-time 30) would truncate a genuinely healthy response
  # right at the boundary and misreport it as a curl-level failure.
  sc_out=$(mktemp)
  sc=$(curl -s -o "$sc_out" -w '%{http_code}' --max-time 70 -X POST -H "Authorization: $TOKEN" \
    "${S1_BASE}/api/${WS}/semantic/anomalies/scan")
  if [[ "$sc" =~ ^2 ]]; then
    ok "semantic/anomalies/scan: $sc"
    mark_covered "/api/{workspace_id}/semantic/anomalies/scan"
    ANOM_ID=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/semantic/anomalies" \
      | jq -r '(.anomalies? // .items? // .)[0].id // empty' 2>/dev/null)
    if [ -n "$ANOM_ID" ]; then
      ok "discovered a real anomaly id $ANOM_ID"
      # A first explain is "a 20-30s recursive search" per the written
      # report — req()'s default --max-time 30 could truncate a genuinely
      # slow-but-healthy response right at the boundary, so this uses its
      # own longer timeout rather than risk a false "000" failure.
      ec=$(curl -s -o /dev/null -w '%{http_code}' --max-time 60 -X POST -H "Authorization: $TOKEN" \
        "${S1_BASE}/api/${WS}/semantic/anomalies/${ANOM_ID}/explain")
      [[ "$ec" =~ ^2 ]] && ok "anomalies/{id}/explain: $ec" || bad "anomalies/{id}/explain: expected 2xx, got $ec"
      mark_covered "/api/{workspace_id}/semantic/anomalies/{anomaly_id}/explain"
    else
      skip "scan produced no anomalies on this fixture right now — anomalies/{id}/explain, anomalies/{id}/status, anomalies/status(bulk) left uncovered"
    fi
  else
    skip "semantic/anomalies/scan returned $sc — the whole anomaly explain/status chain left uncovered"
  fi
  rm -f "$sc_out"

  step "8l. Data App run chain (controls_demo — self-contained DuckDB query, no external file)"
  if [ -n "$DUCKDB_APP_PATHB64" ]; then
    ro=$(mktemp)
    rc3=$(req POST "${S1_BASE}/api/${WS}/apps/${DUCKDB_APP_PATHB64}/run" "$ro" '{}')
    [[ "$rc3" =~ ^2 ]] && ok "apps/{pathb64}/run (controls_demo): $rc3" || bad "apps/{pathb64}/run (controls_demo): expected 2xx, got $rc3"
    rm -f "$ro"
    reso=$(mktemp)
    resc=$(req POST "${S1_BASE}/api/${WS}/apps/${DUCKDB_APP_PATHB64}/result?refresh=false" "$reso" '{}')
    if [[ "$resc" =~ ^2 ]]; then
      ok "apps/{pathb64}/result (controls_demo): $resc"
      CHART_FILE_NAME=$(jq -r '[.result.displays[]? | select(.file_name != null) | .file_name][0] // empty' "$reso" 2>/dev/null)
      [ -n "$CHART_FILE_NAME" ] && ok "discovered chart file_name=$CHART_FILE_NAME" \
                                 || skip "no chart display in the result — apps/{pathb64}/charts/{chart_path} left uncovered"
    else
      bad "apps/{pathb64}/result (controls_demo): expected 2xx, got $resc"
    fi
    rm -f "$reso"
    # Re-publish: controls_demo is already published:true, so this is a
    # low-risk, idempotent write of the working copy — not a state change
    # from a viewer's perspective, still a real request through the handler.
    pbc=$(req POST "${S1_BASE}/api/${WS}/apps/${DUCKDB_APP_PATHB64}/publish" /dev/null)
    [[ "$pbc" =~ ^2 ]] && ok "apps/{pathb64}/publish (re-publish, idempotent): $pbc" \
                        || bad "apps/{pathb64}/publish: expected 2xx, got $pbc"
    mark_covered "/api/{workspace_id}/apps/{pathb64}/publish"
  else
    skip "no DuckDB-backed .app.yml discovered — the entire apps/run chain (run, result, data-cached, charts, publish) left uncovered"
  fi

  step "8m. Automation run (enum_retrieval — zero LLM/DB tasks, just a formatter)"
  if [ -n "$NOLLM_AUTOMATION_PATH" ]; then
    aro=$(mktemp)
    arc=$(req POST "${S1_BASE}/api/${WS}/agentic-automations/runs" "$aro" \
      "$(printf '{"workflow_ref":"%s"}' "$NOLLM_AUTOMATION_PATH")")
    if [[ "$arc" =~ ^2 ]]; then
      AUTOMATION_RUN_ID=$(jq -r '.run_id // empty' "$aro")
      ok "started automation run $AUTOMATION_RUN_ID ($NOLLM_AUTOMATION_PATH)"
      mark_covered "/api/{workspace_id}/agentic-automations/{*rest}"
      mark_covered "/api/{workspace_id}/agentic-workflows/{*rest}"
      # Formatter-only run — should reach a terminal state almost instantly.
      # Wait briefly so the CASE TABLE's 3-way read comparison below doesn't
      # race a still-"running" status on one node.
      if [ -n "$AUTOMATION_RUN_ID" ]; then
        for _ in $(seq 1 15); do
          st=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/agentic-automations/runs/${AUTOMATION_RUN_ID}" \
            | jq -r '.status // empty' 2>/dev/null)
          case "$st" in done|completed|failed|error|cancelled) break ;; esac
          sleep 1
        done
      fi
    else
      skip "agentic-automations/runs (start, enum_retrieval) returned $arc — the automation-run-id chain left uncovered"
    fi
    rm -f "$aro"
  else
    skip "no LLM-free automation discovered — the automation-run-id chain left uncovered"
  fi

  step "8n. Agentic-airway: attempt to start a real pipeline run (sql_ingest)"
  # CORRECTED: pipeline_ref is "Path to a `.airway.yml`, relative to the
  # workspace root" (StartAirwayRequest's own doc comment,
  # crates/agentic/pipeline/src/airway_run.rs) — the bare name "sql_ingest"
  # measured a 400 live. The bare name DOES work as a `?pipeline_ref=`
  # query FILTER on the list/backfill-ranges GETs elsewhere in this script
  # (looser matching there), which is what made the bare form look right
  # when this was first written — this is the one place that needs the
  # real relative file path.
  awo=$(mktemp)
  awc=$(req POST "${S1_BASE}/api/${WS}/agentic-airway/runs" "$awo" '{"pipeline_ref":"pipelines/sql_ingest.airway.yml"}')
  if [[ "$awc" =~ ^2 ]]; then
    ok "started airway run for sql_ingest: $awc (completion needs a reachable source DB — not asserted here, only that starting a real run reaches the handler)"
    mark_covered "/api/{workspace_id}/agentic-airway/{*rest}"
  else
    skip "agentic-airway/runs (start, sql_ingest) returned $awc — agentic-airway/{*rest} left uncovered"
  fi
  rm -f "$awo"

  step "8o. Schedule backfill + run-now (disposable schedule, disabled target)"
  if [ -n "$DISPOSABLE_SCHEDULE_ID" ]; then
    # 00:01 → 00:09 against `*/10 * * * *` contains ZERO occurrences, so this
    # exercises the real backfill handler — cron parsed, window enumerated — and
    # returns `{"run_ids":[],"planned":0}` without seeding a single agent run. A
    # window that did contain one would dispatch a real LLM call from a test
    # whose job is to check that a route responds.
    #
    # The comment lives ABOVE the call, not inside it: a `#` line after a `\`
    # continuation eats the argument that follows, and the body silently never
    # left. The server said 415, which is exactly right for a request with no
    # content — and reads nothing like "your comment ate the payload".
    bfc=$(req POST "${S1_BASE}/api/${WS}/agentic-schedules/${DISPOSABLE_SCHEDULE_ID}/backfill" /dev/null \
      '{"from":"2026-08-01T00:01:00Z","to":"2026-08-01T00:09:00Z"}')
    [[ "$bfc" =~ ^2 ]] && { ok "schedules/{id}/backfill: $bfc"; mark_covered "/api/{workspace_id}/agentic-schedules/{id}/backfill"; } \
                       || bad "schedules/{id}/backfill: expected 2xx, got $bfc"
    # run-now takes NO body at all — not even {} — so a plain curl with no -d,
    # matching the handler's lack of a Json extractor.
    rnc=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X POST -H "Authorization: $TOKEN" \
      "${S1_BASE}/api/${WS}/agentic-schedules/${DISPOSABLE_SCHEDULE_ID}/run-now")
    [[ "$rnc" =~ ^2 ]] && { ok "schedules/{id}/run-now: $rnc"; mark_covered "/api/{workspace_id}/agentic-schedules/{id}/run-now"; } \
                       || bad "schedules/{id}/run-now: expected 2xx, got $rnc"
  else
    skip "no disposable schedule — schedules/{id}/backfill and /run-now left uncovered"
  fi

  step "8p. Test project-run (create -> delete — a lightweight tracking row, no execution)"
  pro=$(mktemp)
  prc=$(req POST "${S1_BASE}/api/${WS}/tests/project-runs" "$pro" '{"name":"fleet-assert-scratch-project-run"}')
  if [[ "$prc" =~ ^2 ]]; then
    PROJECT_RUN_ID=$(jq -r '.id // empty' "$pro")
    ok "created disposable project-run ${PROJECT_RUN_ID:-<no id field>}"
    if [ -n "$PROJECT_RUN_ID" ]; then
      pdc=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X DELETE -H "Authorization: $TOKEN" \
        "${S1_BASE}/api/${WS}/tests/project-runs/${PROJECT_RUN_ID}")
      # There is no GET /project-runs/{id} — DELETE is the only verb at this
      # exact path, so this IS the real, complete coverage for this route.
      [[ "$pdc" =~ ^2 ]] && { ok "tests/project-runs/{id} DELETE: $pdc"; mark_covered "/api/{workspace_id}/tests/project-runs/{project_run_id}"; } \
                         || bad "tests/project-runs/{id} DELETE: expected 2xx, got $pdc"
    fi
  else
    skip "tests/project-runs create returned $prc — tests/project-runs/{id} left uncovered"
  fi
  rm -f "$pro"

  step "8q. Test case run (LLM-backed judge — one real case only; skipped without a usable LLM key)"
  if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ -f "$REPO_ROOT/.env" ]; then
    ANTHROPIC_API_KEY=$(grep -E '^ANTHROPIC_API_KEY=' "$REPO_ROOT/.env" | head -1 | cut -d= -f2- | sed 's/^["'"'"']//; s/["'"'"']$//')
  fi
  if [ -n "$EVAL_TEST_B64" ] && { [ -n "${ANTHROPIC_API_KEY:-}" ] || [ -n "${OPENAI_API_KEY:-}" ]; }; then
    tco=$(mktemp)
    # SSE, and genuinely LLM+judge backed — req()'s default --max-time 30 is
    # too short for a real agent run plus an LLM judge; give it real room.
    tcc=$(curl -s -o "$tco" -w '%{http_code}' --max-time 120 -X POST -H "Authorization: $TOKEN" \
      "${S1_BASE}/api/${WS}/tests/${EVAL_TEST_B64}/cases/0")
    if [[ "$tcc" =~ ^2 ]]; then
      ok "ran test case 0 of analytics.agentic.test.yml (LLM-backed): $tcc"
      mark_covered "/api/{workspace_id}/tests/{pathb64}/cases/{case_index}"
      for _ in $(seq 1 10); do
        EVAL_TEST_RUN_INDEX=$(curl -s --max-time 10 -H "Authorization: $TOKEN" "${S1_BASE}/api/${WS}/tests/${EVAL_TEST_B64}/runs" \
          | jq -r '(.runs? // .items? // .) as $r | (($r[-1].run_index) // (($r|length)-1)) // empty' 2>/dev/null)
        [ -n "$EVAL_TEST_RUN_INDEX" ] && [ "$EVAL_TEST_RUN_INDEX" -ge 0 ] 2>/dev/null && break
        sleep 1
      done
      if [ -n "$EVAL_TEST_RUN_INDEX" ] && [ "$EVAL_TEST_RUN_INDEX" -ge 0 ] 2>/dev/null; then
        ok "discovered eval run_index=$EVAL_TEST_RUN_INDEX"
        hvc=$(req PUT "${S1_BASE}/api/${WS}/tests/${EVAL_TEST_B64}/runs/${EVAL_TEST_RUN_INDEX}/cases/0/human-verdict" /dev/null '{"verdict":"pass"}')
        [[ "$hvc" =~ ^2 ]] && { ok "human-verdict PUT: $hvc"; mark_covered "/api/{workspace_id}/tests/{pathb64}/runs/{run_index}/cases/{case_index}/human-verdict"; } \
                           || bad "human-verdict PUT: expected 2xx, got $hvc"
      else
        skip "could not discover a run_index for the eval run just started — runs/{run_index}[/…] left uncovered"
      fi
    else
      skip "tests/{pathb64}/cases/0 returned $tcc — the whole test-run/human-verdict chain left uncovered"
    fi
    rm -f "$tco"
  else
    skip "no usable LLM API key or no eval-compatible .test.yml discovered — tests/{pathb64}/cases/{case_index} and its run/human-verdict chain left uncovered"
  fi
fi

# ══ CASE TABLE (data-driven) ══════════════════════════════════════════════════
# "bucket|label|method|path|compare|catalog_tpl|body" — compare is "body"
# (status + normalized JSON body across S1/S2/ide) or "status" (status only,
# with the reason given where a route's body is known to legitimately differ
# or the request has no comparable body). `catalog_tpl` is the EXACT path
# template from scripts/fleet-routes.tsv (e.g. "/api/{workspace_id}/agents")
# — kept separate from the concrete request `path` (e.g. "/api/<uuid>/agents")
# so coverage can be matched back against the generated catalog; the two are
# not the same string and must not be conflated. `body` (POST cases only) is
# the LAST field, so `read` absorbs everything remaining even if a JSON body
# ever contained a literal "|" — no case here does, but the parse stays safe
# either way.
declare -a CASES=()

add()  { CASES+=("$1|$2|$3|$4|$5|$6|"); }
addb() { CASES+=("$1|$2|$3|$4|$5|$7|$6"); }

# ── Platform: orgs, admin, billing, teams (Now) ─────────────────────────────
add Now  "admin-feature-flags"        GET "/api/admin/feature-flags"                                   body   "/api/admin/{*rest}"
add Now  "admin-compiles-list"        GET "/api/admin/compiles"                                        status "/api/admin/compiles"  # doc: list ordering/"latest" can shift under a racing compile
add Now  "admin-internal-jobs-stats"  GET "/api/admin/internal-jobs/queue-stats"                        status "/api/admin/internal-jobs/{*rest}"  # doc: queue depth changes continuously under load
add Now  "apps-mine"                  GET "/api/apps/mine"                                              body   "/api/apps/mine"
add Now  "customer-apps-list"         GET "/api/customer-apps"                                          body   "/api/customer-apps"
add Now  "invitations-mine"           GET "/api/invitations/mine"                                       body   "/api/invitations/mine"
add Now  "orgs-list"                  GET "/api/orgs"                                                   body   "/api/orgs"
add Now  "org-get"                    GET "/api/orgs/${ORG_ID}"                                         body   "/api/orgs/{org_id}"
add Now  "org-apps"                   GET "/api/orgs/${ORG_ID}/apps"                                    body   "/api/orgs/{org_id}/apps"
add Now  "org-billing"                GET "/api/orgs/${ORG_ID}/billing"                                 body   "/api/orgs/{org_id}/billing"
add Now  "org-invitations"            GET "/api/orgs/${ORG_ID}/invitations"                             body   "/api/orgs/{org_id}/invitations"
add Now  "org-members"                GET "/api/orgs/${ORG_ID}/members"                                 body   "/api/orgs/{org_id}/members"
add Now  "org-partner-consent"        GET "/api/orgs/${ORG_ID}/partner-publish-consent"                 body   "/api/orgs/{org_id}/partner-publish-consent"
add Now  "org-slack-install"          GET "/api/orgs/${ORG_ID}/slack/installation"                      body   "/api/orgs/{org_id}/slack/installation"
add Now  "org-teams"                  GET "/api/orgs/${ORG_ID}/teams"                                   body   "/api/orgs/{org_id}/teams"
add Now  "org-workspaces"             GET "/api/orgs/${ORG_ID}/workspaces"                              degrades_null "/api/orgs/{org_id}/workspaces"
[ -n "$CUSTOMER_APP_ID" ] && add Now "customer-apps-get" GET "/api/customer-apps/${CUSTOMER_APP_ID}"    body   "/api/customer-apps/{*rest}"
[ -n "$ORG_APP_ID" ]      && add Now "org-app-access"    GET "/api/orgs/${ORG_ID}/apps/${ORG_APP_ID}/access" body "/api/orgs/{org_id}/apps/{app_id}/access"
[ -n "$ADMIN_COMPILE_REV" ] && add Now "admin-compile-detail" GET "/api/admin/compiles/${ADMIN_COMPILE_REV}" status "/api/admin/compiles/{*rest}"  # same ordering caveat as the list

# ── Custom-app bundle serving (Now — includes the confirmed FS-fallback bug) ─
add Now  "customer-apps-serve-oxy-starter" GET "/customer-apps/local/oxy-starter/" status "/customer-apps/{*path}"
# ^ EXPECTED TO FAIL. ide=200 real bundle; both serve replicas=404. Root cause:
# custom_apps_build_store::get_object's filesystem fallback (no
# OXY_CUSTOMER_APPS_S3_BUCKET in this fleet) reads OXY_STATE_DIR — a node-local
# path — with no role guard. See the written report for the full trace.

# ── Workspace: git & working copy (Now) ─────────────────────────────────────
add Now  "branches"          GET "/api/${WS}/branches"                 body   "/api/{workspace_id}/branches"
add Now  "files-tree"        GET "/api/${WS}/files"                    body   "/api/{workspace_id}/files"
add Now  "files-diff-summary" GET "/api/${WS}/files/diff-summary"      body   "/api/{workspace_id}/files/diff-summary"
add Now  "fetch"             POST "/api/${WS}/fetch"                   status "/api/{workspace_id}/fetch"
add Now  "git-state"         GET "/api/${WS}/git-state"                body   "/api/{workspace_id}/git-state"
add Now  "logo"              GET "/api/${WS}/logo"                     status "/api/{workspace_id}/logo"  # binary image
add Now  "logs"              GET "/api/${WS}/logs"                     body   "/api/{workspace_id}/logs"
add Now  "members"           GET "/api/${WS}/members"                  body   "/api/{workspace_id}/members"
add Now  "meta"              GET "/api/${WS}/meta"                     body   "/api/{workspace_id}/meta"
add Now  "modeling"          GET "/api/${WS}/modeling"                 body   "/api/{workspace_id}/modeling"
add Now  "onboarding-readiness" GET "/api/${WS}/onboarding-readiness"  body   "/api/{workspace_id}/onboarding-readiness"
add Now  "test-llm-key"      POST "/api/${WS}/onboarding/test-llm-key" status "/api/{workspace_id}/onboarding/test-llm-key"
add Now  "org-subdomain"     GET "/api/${WS}/org-subdomain"            body   "/api/{workspace_id}/org-subdomain"
add Now  "oxy-access"        GET "/api/${WS}/oxy-access"               body   "/api/{workspace_id}/oxy-access"
add Now  "procedures-list"   GET "/api/${WS}/procedures"               body   "/api/{workspace_id}/procedures"
add Now  "pull-changes"      POST "/api/${WS}/pull-changes"            status "/api/{workspace_id}/pull-changes"
add Now  "recent-commits"    GET "/api/${WS}/recent-commits"           body   "/api/{workspace_id}/recent-commits"
add Now  "repositories"      GET "/api/${WS}/repositories"             body   "/api/{workspace_id}/repositories"
add Now  "revision-info"     GET "/api/${WS}/revision-info"            body   "/api/{workspace_id}/revision-info"
add Now  "secrets-list"      GET "/api/${WS}/secrets"                  body   "/api/{workspace_id}/secrets"
add Now  "secrets-env"       GET "/api/${WS}/secrets/env"              body   "/api/{workspace_id}/secrets/env"
add Now  "status"            GET "/api/${WS}/status"                  body   "/api/{workspace_id}/status"
add Now  "tests-list"        GET "/api/${WS}/tests"                    body   "/api/{workspace_id}/tests"
add Now  "tests-project-runs" GET "/api/${WS}/tests/project-runs"      body   "/api/{workspace_id}/tests/project-runs"
add Now  "threads-list"      GET "/api/${WS}/threads"                  body   "/api/{workspace_id}/threads"
add Now  "worktrees"         GET "/api/${WS}/worktrees"                body   "/api/{workspace_id}/worktrees"   # normalized: idle_secs
add Now  "world-model-events" GET "/api/${WS}/world-model/events"      status "/api/{workspace_id}/world-model/events" # SSE, connect-and-check only
[ -n "$AUTOMATION_PATHB64" ] && add Now "procedures-detail" GET "/api/${WS}/procedures/${AUTOMATION_PATHB64}" body "/api/{workspace_id}/procedures/{path_b64}"
[ -n "$THREAD_ID" ] && add Now "thread-get"      GET "/api/${WS}/threads/${THREAD_ID}"          body "/api/{workspace_id}/threads/{id}"
[ -n "$THREAD_ID" ] && add Now "thread-messages" GET "/api/${WS}/threads/${THREAD_ID}/messages" body "/api/{workspace_id}/threads/{id}/messages"
[ -n "$APP_PATHB64" ] && add Now "files-read-app-yml" GET "/api/${WS}/files/${APP_PATHB64}"           body "/api/{workspace_id}/files/{pathb64}"
[ -n "$APP_PATHB64" ] && add Now "files-from-git-app-yml" GET "/api/${WS}/files/${APP_PATHB64}/from-git" body "/api/{workspace_id}/files/{pathb64}/from-git"

# ── Workspace: agentic run surfaces (Now) ───────────────────────────────────
add Now  "airway-pipelines"  GET "/api/${WS}/airway-pipelines"          body "/api/{workspace_id}/airway-pipelines"
add Now  "airway-runs"       GET "/api/${WS}/agentic-airway/runs?pipeline_ref=sql_ingest" body "/api/{workspace_id}/agentic-airway/runs"
add Now  "automations-runs"  GET "/api/${WS}/agentic-automations/runs?workflow_ref=${AUTOMATION_PATH}" body "/api/{workspace_id}/agentic-automations/runs"
add Now  "workflows-runs"    GET "/api/${WS}/agentic-workflows/runs?workflow_ref=${AUTOMATION_PATH}"   body "/api/{workspace_id}/agentic-workflows/runs"
add Now  "schedules-list"    GET "/api/${WS}/agentic-schedules"         body "/api/{workspace_id}/agentic-schedules"
[ -n "$SCHEDULE_ID" ] && add Now "schedule-get" GET "/api/${WS}/agentic-schedules/${SCHEDULE_ID}" body "/api/{workspace_id}/agentic-schedules/{id}"
[ -n "$THREAD_ID" ] && add Now "automations-thread-run" GET "/api/${WS}/agentic-automations/threads/${THREAD_ID}/run" body "/api/{workspace_id}/agentic-automations/threads/{thread_id}/run"
[ -n "$THREAD_ID" ] && add Now "workflows-thread-run"   GET "/api/${WS}/agentic-workflows/threads/${THREAD_ID}/run"   body "/api/{workspace_id}/agentic-workflows/threads/{thread_id}/run"
[ -n "$THREAD_ID" ] && add Now "analytics-thread-run"   GET "/api/${WS}/analytics/threads/${THREAD_ID}/run"          body "/api/{workspace_id}/analytics/threads/{thread_id}/run"
[ -n "$THREAD_ID" ] && add Now "analytics-thread-runs"  GET "/api/${WS}/analytics/threads/${THREAD_ID}/runs"         body "/api/{workspace_id}/analytics/threads/{thread_id}/runs"

# ── Workspace: core data resources (Now) ────────────────────────────────────
add Now  "agents-list"       GET "/api/${WS}/agents"                    body "/api/{workspace_id}/agents"
add Now  "api-keys-list"     GET "/api/${WS}/api-keys"                  body "/api/{workspace_id}/api-keys"
add Now  "app-integrations-list" GET "/api/${WS}/app-integrations"      body "/api/{workspace_id}/app-integrations"
add Now  "apps-list"         GET "/api/${WS}/apps"                      body "/api/{workspace_id}/apps"
add Now  "automations-list"  GET "/api/${WS}/automations"               body "/api/{workspace_id}/automations"
add Now  "builder-availability" GET "/api/${WS}/builder-availability"   body "/api/{workspace_id}/builder-availability"
addb Now "semantic-compile"  POST "/api/${WS}/semantic/compile"         body  '{}' "/api/{workspace_id}/semantic/compile"
add Now  "custom-apps-workspace" GET "/api/${WS}/custom-apps"           body "/api/{workspace_id}/custom-apps"
add Now  "databases-list"    GET "/api/${WS}/databases?branch=main"     body "/api/{workspace_id}/databases"
addb Now "compile-post"      POST "/api/${WS}/compile"                 status '{}' "/api/{workspace_id}/compile"
add Now  "compile-status"    GET "/api/${WS}/compile/status?branch=main" body "/api/{workspace_id}/compile/status"
[ -n "$APP_PATHB64" ] && add Now "apps-get"        GET "/api/${WS}/apps/${APP_PATHB64}"          body "/api/{workspace_id}/apps/{pathb64}"
[ -n "$APP_PATHB64" ] && add Now "apps-displays"   GET "/api/${WS}/apps/${APP_PATHB64}/displays" body "/api/{workspace_id}/apps/{pathb64}/displays"
[ -n "$APP_PATHB64" ] && add Now "apps-file"       GET "/api/${WS}/apps/file/${APP_PATHB64}"     body "/api/{workspace_id}/apps/file/{pathb64}"
# ^ get_data (app::get_data) reads from the STATE DIR (a downloaded/cached
# artifact from a prior app run), not any workspace file — an app-yml path is
# not a valid key here without first running the app (Fixture-bucket
# /apps/{pathb64}/run, not chased in this pass). Expect a 404 on all three
# nodes — still a valid, agreeing, covered case, just not a 200.
[ -n "$SOURCE_FILE_B64" ] && add Now "apps-source" GET "/api/${WS}/apps/source/${SOURCE_FILE_B64}" body "/api/{workspace_id}/apps/source/{pathb64}"
add Now  "databases-schema-local" GET "/api/${WS}/databases/local/schema" workspace_local "/api/{workspace_id}/databases/{database_name}/schema"

# The SAME two routes with a branch, which is the shape the SQL IDE actually
# sends. `escalate_for_branch` promotes them to IdeOnly, the replica self-proxies
# to the ide, and they SUCCEED — real schema, real rows. Without these cases the
# suite only ever saw the refusal and read like the SQL IDE is broken on a
# replica, when in fact the surface a user drives works end to end and only the
# branchless call (the launcher's shape) refuses. Two behaviours, one route;
# asserting one and not the other is how this branch produced three findings it
# later had to withdraw.
#
# `status` compare, not body: these travel a proxy hop and return live warehouse
# data, so ide and replica agree on the outcome, not byte-for-byte on it.
add Now  "databases-schema-local-branch" GET "/api/${WS}/databases/local/schema?branch=main" status "/api/{workspace_id}/databases/{database_name}/schema"
# ^ NOT a defect: "local" is DuckDB with `dataset: .db/` — a path INSIDE the
# workspace working copy — and no `s3_mirror` is configured on this fixture,
# so a stateless replica genuinely cannot serve it and must refuse. ide=200
# (owns the working copy) / serve=500 (refuses) is the CORRECT, required
# shape here, not a disagreement to flag — see assert_workspace_local_refusal
# above. This was wrongly asserted as serve==ide status agreement before;
# corrected per the coordinator's fix. get_database_schema (database.rs)
# discards the actual error into a bare StatusCode with no body (see that
# helper's comment), so this stays the unnamed `workspace_local` variant —
# body content isn't asserted, only that serve answers 5xx, never 2xx.

# ── Workspace: semantic model, monitors, world model (Now) ─────────────────
add Now  "semantic-anomalies" GET "/api/${WS}/semantic/anomalies"       body "/api/{workspace_id}/semantic/anomalies"
add Now  "semantic-metric-tree" GET "/api/${WS}/semantic/metric-tree"  body "/api/{workspace_id}/semantic/metric-tree"
add Now  "semantic-monitors" GET "/api/${WS}/semantic/monitors"         body "/api/{workspace_id}/semantic/monitors"
add Now  "semantic-preagg-status" GET "/api/${WS}/semantic/preagg-status" status "/api/{workspace_id}/semantic/preagg-status" # doc: per-process cache clock
add Now  "semantic-world-model" GET "/api/${WS}/semantic/world-model"   body "/api/{workspace_id}/semantic/world-model"
[ -n "$TOPIC_B64" ] && add Now "semantic-topic" GET "/api/${WS}/semantic/topic/${TOPIC_B64}" status "/api/{workspace_id}/semantic/topic/{file_path_b64}" # doc: per-process semantic cache
[ -n "$VIEW_B64" ]  && add Now "semantic-view"  GET "/api/${WS}/semantic/view/${VIEW_B64}"   status "/api/{workspace_id}/semantic/view/{file_path_b64}" # doc: per-process semantic cache
[ -n "$MEASURE_ID" ] && add Now "metric-tree-sensitivity" GET "/api/${WS}/semantic/metric-tree/${MEASURE_ID}/sensitivity" status "/api/{workspace_id}/semantic/metric-tree/{measure_id}/sensitivity" # doc: per-process cache family
[ -n "$MEASURE_ID" ] && add Now "metric-tree-time-dims" GET "/api/${WS}/semantic/metric-tree/time-dimensions?measure_id=${MEASURE_ID}" status "/api/{workspace_id}/semantic/metric-tree/time-dimensions"
if [ -n "$MEASURE_ID" ] && [ -n "$TIME_DIM" ]; then
  # predict is a pure op over the already-loaded metric tree (no DB query) —
  # live-verified 200 on serve. explain/opportunity DO run a live query
  # against "local" (DuckDB, `dataset: .db/`, no `s3_mirror` on this
  # fixture) — a path inside the working copy a replica does not have, so
  # ide=200 / serve=500 is the CORRECT, required shape (see
  # assert_workspace_local_refusal above), not a defect to flag. This is not
  # an isolated pair — it is the same refusal every FleetOk route hits when
  # it runs a live query against a workspace-local-dataset database with no
  # mirror configured. Both fall through `MetricTreeError::Op`, whose
  # `IntoResponse` sends the fixed string "Metric tree operation failed"
  # (metric_tree.rs) — the real message is logged, not returned — so these
  # stay the unnamed `workspace_local` variant, same reasoning as
  # databases-schema-local above.
  addb Now "metric-tree-predict" POST "/api/${WS}/semantic/metric-tree/predict" body \
    "$(printf '{"changes":[{"measure":"%s","delta":1}]}' "$MEASURE_ID")" "/api/{workspace_id}/semantic/metric-tree/predict"
  addb Now "metric-tree-explain" POST "/api/${WS}/semantic/metric-tree/explain" workspace_local \
    "$(printf '{"target":"%s","time_dimension":"%s","current_period":["2026-08-01","2026-08-07"],"previous_period":["2026-07-25","2026-07-31"]}' "$MEASURE_ID" "$TIME_DIM")" "/api/{workspace_id}/semantic/metric-tree/explain"
  addb Now "metric-tree-opportunity" POST "/api/${WS}/semantic/metric-tree/opportunity" workspace_local \
    "$(printf '{"target":"%s","time_dimension":"%s","period":["2026-08-01","2026-08-07"]}' "$MEASURE_ID" "$TIME_DIM")" "/api/{workspace_id}/semantic/metric-tree/opportunity"
fi
[ -n "$ENTITY_NAME" ] && add Fixture "world-model-instances" GET "/api/${WS}/semantic/world-model/instances?entity=${ENTITY_NAME}" body "/api/{workspace_id}/semantic/world-model/instances"

# ── Workspace: onboarding, integrations, tests, misc (Now) ─────────────────
if [ -n "$TEST_B64" ]; then
  add Now "tests-detail-list" GET "/api/${WS}/tests/${TEST_B64}"        body "/api/{workspace_id}/tests/{pathb64}"
else
  skip "no *.test.yml discovered on disk — tests/{pathb64} left uncovered"
fi

# ── Fixture bucket: cheap discovery-only additions ──────────────────────────
add Fixture "airway-backfill-ranges" GET "/api/${WS}/agentic-airway/backfill-ranges?pipeline_ref=sql_ingest" body "/api/{workspace_id}/agentic-airway/backfill-ranges"
if [ -n "$AIRWAY_RANGE_ID" ]; then
  add Fixture "airway-coverage" GET "/api/${WS}/agentic-airway/coverage?range_id=${AIRWAY_RANGE_ID}" body "/api/{workspace_id}/agentic-airway/coverage"
else
  skip "no backfill range exists for sql_ingest — agentic-airway/coverage left uncovered"
fi

# ── Fixture bucket: disposable create -> read lifecycles ───────────────────
if [ -n "$SECRET_ID" ]; then
  add Fixture "secret-get" GET "/api/${WS}/secrets/${SECRET_ID}" body "/api/{workspace_id}/secrets/{id}"
  add Fixture "secret-value" GET "/api/${WS}/secrets/${SECRET_ID}/value" body "/api/{workspace_id}/secrets/{id}/value"
fi
[ -n "$API_KEY_ID" ] && add Fixture "api-key-get" GET "/api/${WS}/api-keys/${API_KEY_ID}" body "/api/{workspace_id}/api-keys/{id}"
[ -n "$TEAM_ID" ] && add Fixture "team-get" GET "/api/orgs/${ORG_ID}/teams/${TEAM_ID}" body "/api/orgs/{org_id}/teams/{team_id}"
[ -n "$DISPOSABLE_SCHEDULE_ID" ] && add Fixture "disposable-schedule-get" GET "/api/${WS}/agentic-schedules/${DISPOSABLE_SCHEDULE_ID}" body "/api/{workspace_id}/agentic-schedules/{id}"
[ -n "$INVITATION_ID" ] && add Fixture "invitation-get" GET "/api/orgs/${ORG_ID}/invitations/${INVITATION_ID}" body "/api/orgs/{org_id}/invitations/{invitation_id}"

# ── Destr bucket: disposable org/workspace/app lifecycle ───────────────────
[ -n "$DISPOSABLE_ORG_ID" ] && add Destr "disposable-org-get" GET "/api/orgs/${DISPOSABLE_ORG_ID}" body "/api/orgs/{org_id}"
[ -n "$DISPOSABLE_ORG_ID" ] && [ -n "$DISPOSABLE_WS_ID" ] && add Destr "disposable-ws-get" GET "/api/orgs/${DISPOSABLE_ORG_ID}/workspaces/${DISPOSABLE_WS_ID}" status "/api/orgs/{org_id}/workspaces/{id}"
[ -n "$DISPOSABLE_APP_ID" ] && add Destr "disposable-app-get" GET "/api/customer-apps/${DISPOSABLE_APP_ID}" body "/api/customer-apps/{*rest}"

# ── Fixture bucket: scratch-file lifecycle ──────────────────────────────────
add Fixture "scratch-file-read" GET "/api/${WS}/files/${SCRATCH_FILE_CURRENT_B64:-$SCRATCH_FILE_B64}" body "/api/{workspace_id}/files/{pathb64}"

# ── Fixture bucket: world-model discovery chain ─────────────────────────────
if [ -n "$ENTITY_NAME" ] && [ -n "$WM_SEED_KEY" ]; then
  add Fixture "world-model-instance-detail" GET \
    "/api/${WS}/semantic/world-model/instance-detail?entity=${ENTITY_NAME}&key=${WM_SEED_KEY}" \
    status "/api/{workspace_id}/semantic/world-model/instance-detail"  # SSE, read to close; status only (see doc's SSE section)
  add Fixture "world-model-filter-instances" GET \
    "/api/${WS}/semantic/world-model/filter-instances?seed_entity=${ENTITY_NAME}&seed_key=${WM_SEED_KEY}&entity=${ENTITY_NAME}" \
    body "/api/{workspace_id}/semantic/world-model/filter-instances"
  addb Fixture "world-model-filter-counts" POST "/api/${WS}/semantic/world-model/filter-counts" status \
    "$(printf '{"entity_id":"%s","key_value":"%s"}' "$ENTITY_NAME" "$WM_SEED_KEY")" \
    "/api/{workspace_id}/semantic/world-model/filter-counts"  # SSE, status only
  [ -n "$MEASURE_ID" ] && add Fixture "world-model-measure-breakdown" GET \
    "/api/${WS}/semantic/world-model/measure-breakdown?entity=${ENTITY_NAME}&key=${WM_SEED_KEY}&measure=${MEASURE_ID}" \
    status "/api/{workspace_id}/semantic/world-model/measure-breakdown"  # SSE, status only
else
  skip "no world-model entity/instance discovered — instance-detail, filter-instances, filter-counts, measure-breakdown left uncovered"
fi

# ── Fixture bucket: metric-tree distribution/drill + semantic execute-query ─
# All three run a LIVE query against "local" (DuckDB, `dataset: .db/`, no
# `s3_mirror` on this fixture) — a workspace-local path a replica genuinely
# does not have. ide=200 / serve=5xx is the CORRECT, required shape here
# (see assert_workspace_local_refusal above), not a defect. distribution and
# drill share metric-tree/explain,opportunity's `MetricTreeError::Op` arm,
# which discards the real message behind a fixed "Metric tree operation
# failed" string — unnamed `workspace_local`. `semantic`'s Warehouse branch
# (the one an ad-hoc single-measure probe with no existing preagg rollup
# compiles to) DOES propagate the real message via `agentic_error_response`
# (data.rs) — `workspace_local_named`.
if [ -n "$MEASURE_ID" ] && [ -n "$TIME_DIM" ]; then
  addb Fixture "metric-tree-distribution" POST "/api/${WS}/semantic/metric-tree/distribution" workspace_local \
    "$(printf '{"target":"%s","time_dimension":"%s","period":["2026-08-01","2026-08-07"]}' "$MEASURE_ID" "$TIME_DIM")" \
    "/api/{workspace_id}/semantic/metric-tree/distribution"
  addb Fixture "metric-tree-drill" POST "/api/${WS}/semantic/metric-tree/drill" workspace_local \
    "$(printf '{"target":"%s","time_dimension":"%s","period":["2026-08-01","2026-08-07"]}' "$MEASURE_ID" "$TIME_DIM")" \
    "/api/{workspace_id}/semantic/metric-tree/drill"  # no `root` — drills the top-ranked segment automatically
fi
if [ -n "$MEASURE_ID" ]; then
  addb Fixture "semantic-execute-query" POST "/api/${WS}/semantic" workspace_local_named \
    "$(printf '{"measures":["%s"]}' "$MEASURE_ID")" "/api/{workspace_id}/semantic"
fi

# ── Fixture bucket: anomaly status (single + bulk), using the id from 8k ───
if [ -n "$ANOM_ID" ]; then
  addb Fixture "anomaly-status" POST "/api/${WS}/semantic/anomalies/${ANOM_ID}/status" body \
    '{"status":"acknowledged"}' "/api/{workspace_id}/semantic/anomalies/{anomaly_id}/status"
  addb Fixture "anomaly-status-bulk" POST "/api/${WS}/semantic/anomalies/status" body \
    "$(printf '{"ids":["%s"],"status":"acknowledged"}' "$ANOM_ID")" "/api/{workspace_id}/semantic/anomalies/status"
fi

# ── Fixture bucket: sql routes — workspace-local "local" DuckDB dataset ────
# Same shape as the metric-tree/semantic cases above: ide=200, serve=5xx is
# CORRECT and required (assert_workspace_local_refusal), not a defect.
# execute_sql routes through the same `agentic_error_response` helper as
# `semantic`'s Warehouse branch, which DOES put the real message in the JSON
# body (data.rs:198-206) — `workspace_local_named`.
PROBE_SQL_B64=$(pathb64_std "probe.sql")
addb Fixture "sql-query" POST "/api/${WS}/sql/query" workspace_local_named '{"sql":"SELECT 1 AS probe","database":"local"}' "/api/{workspace_id}/sql/query"
# The branch-carrying twin — what the SQL IDE sends on every Run. Verified by
# hand before adding: this returns the five real customer rows through the proxy
# while the branchless call above refuses. It replaces a browser flow that spent
# roughly $4 and 1.3M tokens per attempt driving four LLM-chosen UI steps to
# assert the same fact less reliably.
addb Fixture "sql-query-branch" POST "/api/${WS}/sql/query?branch=main" status '{"sql":"SELECT * FROM customer LIMIT 5","database":"local"}' "/api/{workspace_id}/sql/query"
addb Fixture "sql-file" POST "/api/${WS}/sql/${PROBE_SQL_B64}" workspace_local_named '{"sql":"SELECT 1 AS probe","database":"local"}' "/api/{workspace_id}/sql/{pathb64}"

# ── Fixture bucket: database inspect family — workspace-local "local" dataset
# /inspect-schemas and /inspect-schema-tables hit the same refusal but
# return a BARE StatusCode with no body at all (database.rs:1113-1118,
# 1160-1167 — `Err(StatusCode::INTERNAL_SERVER_ERROR)`, no `Json`) — unnamed
# `workspace_local`, same reasoning as databases-schema-local above.
#
# /inspect itself is a THIRD shape, not covered by either workspace_local
# variant: it is SSE and its handler always returns 200 (`Ok(Sse::new(rx))`
# unconditionally, database.rs:1071) — the refusal shows up as an
# `InspectEvent::Error` event INSIDE that 200 stream, not as a status code,
# so ide and serve genuinely agree on status here (200/200) and this stays
# `status`-compared. That status agreement is true, just shallow — it won't
# catch the ide's stream ending Complete against a replica's ending Error;
# noted rather than building a fourth bespoke assertion shape for one route.
# CORRECTED: mounted `post(...)` too (workspace.rs:608) — a GET measured a
# uniform 405 on all three nodes live. Uniform-and-wrong is worse than an
# ordinary failure here: `status` compare only checks agreement, so all
# three agreeing on the SAME wrong status PASSED silently — the exact "green
# but proved nothing" shape to watch for. Verb fixed; the shallow-status
# caveat above still applies once it's actually exercising the SSE path.
add Fixture "databases-inspect" POST "/api/${WS}/databases/inspect?database=local" status "/api/{workspace_id}/databases/inspect"
# CORRECTED: both are mounted `post(...)` (workspace.rs:609,611), not GET —
# a GET measured 405 on ALL THREE nodes uniformly (confirmed live). That's
# this fixture's own construction bug, not a product finding: a uniform
# 405 means the ROUTER rejected the verb before dispatch ever reached the
# workspace_local-vs-serve question at all. Params are query-string (the
# handlers take `Query<...>`, no `Json` extractor), so still `add`, not
# `addb` — only the method changes.
add Fixture "databases-inspect-schemas" POST "/api/${WS}/databases/inspect-schemas?database=local" workspace_local "/api/{workspace_id}/databases/inspect-schemas"
add Fixture "databases-inspect-schema-tables" POST "/api/${WS}/databases/inspect-schema-tables?database=local&schema=main" workspace_local "/api/{workspace_id}/databases/inspect-schema-tables"
# test-connection and sync are IdeOnly — all three nodes self-proxy to the
# SAME ide process that owns the working copy, so no serve/ide asymmetry is
# possible here at all; status agreement is the correct, non-shallow check.
addb Fixture "databases-test-connection" POST "/api/${WS}/databases/test-connection" status \
  '{"warehouse":{"type":"duckdb","name":"local","config":{"file_search_path":".db/"}}}' "/api/{workspace_id}/databases/test-connection"
add Fixture "databases-sync" POST "/api/${WS}/databases/sync?database=local" status "/api/{workspace_id}/databases/sync"

# ── Fixture bucket: modeling nodes/lineage — real reads, no DB needed ───────
add Fixture "modeling-nodes" GET "/api/${WS}/modeling/jaffle_shop/nodes" body "/api/{workspace_id}/modeling/{*rest}"
add Fixture "modeling-lineage" GET "/api/${WS}/modeling/jaffle_shop/lineage" body "/api/{workspace_id}/modeling/{*rest}"

# ── Fixture bucket: disposable data-repo reads (branch/branches/diff/files) ─
if [ "$DATA_REPO_LIVE" = "1" ] && [ -n "$DISPOSABLE_WS_ID" ]; then
  add Fixture "repo-branch"   GET "/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}/branch"   body "/api/{workspace_id}/repositories/{name}/branch"
  add Fixture "repo-branches" GET "/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}/branches" body "/api/{workspace_id}/repositories/{name}/branches"
  add Fixture "repo-diff"     GET "/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}/diff"     body "/api/{workspace_id}/repositories/{name}/diff"
  add Fixture "repo-files"    GET "/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}/files"    body "/api/{workspace_id}/repositories/{name}/files"
fi

# ── Fixture bucket: world-model proxy routes (weather/foot-traffic/competitors)
# Real, comparable requests once a real integration row exists (see 8f) — the
# upstream call itself legitimately fails on a fake key (502), which is still
# a real, comparable response reaching the handler, not a fixture-design gap.
# Competitors needs NO integration at all — it calls the public OSM Overpass
# API directly.
addb Fixture "world-model-competitors" POST "/api/${WS}/world-model/competitors" status \
  '{"anchors":[{"lat":40.7128,"lon":-74.006}],"radius_m":500}' "/api/{workspace_id}/world-model/competitors"
if [ "$APP_INTEGRATION_OWM_LIVE" = "1" ]; then
  addb Fixture "world-model-weather-current" POST "/api/${WS}/world-model/weather/current" status \
    '[{"key":"probe","lat":40.7128,"lon":-74.006}]' "/api/{workspace_id}/world-model/weather/current"
  add Fixture "world-model-weather-tile" GET "/api/${WS}/world-model/weather/temp_new/0/0/0" status \
    "/api/{workspace_id}/world-model/weather/{layer}/{z}/{x}/{y}"
else
  skip "no openweathermap app-integration — world-model/weather/current, weather tile left uncovered"
fi
if [ "$APP_INTEGRATION_BESTTIME_LIVE" = "1" ]; then
  addb Fixture "world-model-foot-traffic-current" POST "/api/${WS}/world-model/foot-traffic/current" status \
    '[{"key":"probe","venue_name":"Fleet Assert Probe","venue_address":"1 Market St, San Francisco, CA"}]' \
    "/api/{workspace_id}/world-model/foot-traffic/current"
  addb Fixture "world-model-foot-traffic-radar" POST "/api/${WS}/world-model/foot-traffic/radar" status \
    '[{"key":"probe","lat":37.7749,"lon":-122.4194,"radius":500}]' "/api/{workspace_id}/world-model/foot-traffic/radar"
else
  skip "no besttime app-integration — world-model/foot-traffic/current, foot-traffic/radar left uncovered"
fi

# ── Fixture bucket: Data App run chain (controls_demo) ──────────────────────
if [ -n "$DUCKDB_APP_PATHB64" ]; then
  addb Fixture "apps-run"    POST "/api/${WS}/apps/${DUCKDB_APP_PATHB64}/run"              body '{}' "/api/{workspace_id}/apps/{pathb64}/run"
  addb Fixture "apps-result" POST "/api/${WS}/apps/${DUCKDB_APP_PATHB64}/result?refresh=false" body '{}' "/api/{workspace_id}/apps/{pathb64}/result"
  add  Fixture "apps-data-cached" GET "/api/${WS}/apps/${DUCKDB_APP_PATHB64}/data-cached"  status "/api/{workspace_id}/apps/{pathb64}/data-cached"
  # Node-local disk cache with an S3-mirror fallback (per the written report)
  # — status-only, since a genuinely diskless serve replica may legitimately
  # 404 here where the ide (which just ran it) 200s. Worth watching for a
  # tenth finding alongside the nine already documented.
  [ -n "$CHART_FILE_NAME" ] && add Fixture "apps-chart" GET "/api/${WS}/apps/${DUCKDB_APP_PATHB64}/charts/${CHART_FILE_NAME}" \
    status "/api/{workspace_id}/apps/{pathb64}/charts/{chart_path}"
fi

# ── Fixture bucket: automation-run-id reads + coordinator + cancel no-op ────
if [ -n "$AUTOMATION_RUN_ID" ]; then
  add Fixture "automations-run-get" GET "/api/${WS}/agentic-automations/runs/${AUTOMATION_RUN_ID}" body "/api/{workspace_id}/agentic-automations/runs/{id}"
  add Fixture "workflows-run-get"   GET "/api/${WS}/agentic-workflows/runs/${AUTOMATION_RUN_ID}"   body "/api/{workspace_id}/agentic-workflows/runs/{id}"
fi
add Fixture "coordinator-active-runs" GET "/api/${WS}/analytics/coordinator/active-runs" body "/api/{workspace_id}/analytics/{*rest}"
add Fixture "coordinator-runs-list"   GET "/api/${WS}/analytics/coordinator/runs"        body "/api/{workspace_id}/analytics/{*rest}"
add Fixture "coordinator-queue"       GET "/api/${WS}/analytics/coordinator/queue"       body "/api/{workspace_id}/analytics/{*rest}"
# status-only: a live run measured serve-1 and serve-2 bodies genuinely
# differing (both 200, same instant, four other agents' runs live on this
# shared fleet at the time) while serve matched ide — "recovery" is a scan
# for runs idle past a threshold, which is exactly the kind of live,
# instant-sensitive state product-context.md already documents elsewhere
# (queue depth, admin/compiles ordering) as legitimately differing between
# any two calls regardless of which node answers. active-runs/queue/runs
# above stay body-compared: this run's data showed THEM agreeing, so
# downgrading them isn't supported by evidence, only this one is.
add Fixture "coordinator-recovery"    GET "/api/${WS}/analytics/coordinator/recovery"    status "/api/{workspace_id}/analytics/{*rest}"
# Legacy RunsManager cancel — always returns 200 whether or not a task is
# found (task_manager.rs's no-entry case is Ok(false), never Err), so any
# real workflow path + plausible index is a safe, deterministic real request.
[ -n "$AUTOMATION_PATHB64" ] && add Fixture "runs-cancel-noop" DELETE "/api/${WS}/runs/${AUTOMATION_PATHB64}/0" status "/api/{workspace_id}/runs/{source_id}/{run_index}"

# ── Fixture bucket: eval-test run reads (from 8q, only if that ran) ────────
[ -n "$EVAL_TEST_B64" ] && add Fixture "tests-runs-list" GET "/api/${WS}/tests/${EVAL_TEST_B64}/runs" body "/api/{workspace_id}/tests/{pathb64}/runs"
if [ -n "$EVAL_TEST_RUN_INDEX" ] && [ "$EVAL_TEST_RUN_INDEX" -ge 0 ] 2>/dev/null; then
  add Fixture "tests-run-get"      GET "/api/${WS}/tests/${EVAL_TEST_B64}/runs/${EVAL_TEST_RUN_INDEX}"                body "/api/{workspace_id}/tests/{pathb64}/runs/{run_index}"
  add Fixture "tests-run-verdicts" GET "/api/${WS}/tests/${EVAL_TEST_B64}/runs/${EVAL_TEST_RUN_INDEX}/human-verdicts" body "/api/{workspace_id}/tests/{pathb64}/runs/{run_index}/human-verdicts"
fi

# ── Fixture bucket: invitation accept (expected 403 — email mismatch by design)
# The token was issued to fleet-assert-scratch@example.com, not this
# session's own address; accept_invitation checks that match BEFORE touching
# membership (invitation_handlers.rs), so 403 on every node is the correct,
# comparable answer — a real request reaching the handler, not a fixture gap.
[ -n "$INVITATION_TOKEN" ] && addb Fixture "invitation-accept-mismatch" POST "/api/invitations/${INVITATION_TOKEN}/accept" status '{}' "/api/invitations/{token}/accept"

# ══ ASSERTIONS ════════════════════════════════════════════════════════════════
step "9. Route classification (serve-1) — every case's base path, GET-probed"
SEEN_LIST=$'\n'
for entry in "${CASES[@]+"${CASES[@]}"}"; do
  IFS='|' read -r bucket label method path compare tpl body <<< "$entry"
  base="${path%%\?*}"
  case "$SEEN_LIST" in
    *$'\n'"$base"$'\n'*) continue ;;
    *) SEEN_LIST="${SEEN_LIST}${base}"$'\n' ;;
  esac
  IFS='|' read -r c by fwd <<< "$(stamp "${S1_BASE}${base}")"
  if [ "$c" = "000" ]; then
    bad "[$bucket] $label classification probe: no response ($base)"
  else
    ok "[$bucket] $label classification: $c served-by=$by forwarded=$fwd"
  fi
done

step "10. Replica agreement + differential + body compare (data-driven, ${#CASES[@]} cases)"
for entry in "${CASES[@]+"${CASES[@]}"}"; do
  IFS='|' read -r bucket label method path compare tpl body <<< "$entry"
  url1="${S1_BASE}${path}"; url2="${S2_BASE}${path}"; urlI="${IDE_BASE}${path}"
  b1=$(mktemp); b2=$(mktemp); bI=$(mktemp)
  s1=$(mktemp); s2=$(mktemp); sI=$(mktemp)
  # Wait on the THREE specific PIDs, never a bare `wait` — this shell also has
  # the long-running ide/serve1/serve2 `oxy serve` processes backgrounded by
  # start_node(), and a bare `wait` would block on those too.
  # Warehouse-query cases go to the three nodes ONE AT A TIME. Everything else
  # still fans out, because for a Postgres read the fan-out is free and cuts
  # the run by minutes.
  #
  # Why: `metric-tree/opportunity` costs 8.7s on an idle node (measured alone,
  # ide, 200). Fired at all three at once it is ~26s of real work landing on
  # one 10-core box, every node contending for CPU — all three then blew the
  # 30s deadline and returned `000`, AND the machine stayed saturated long
  # enough to take the next ~15 cases down with it. In the run before this
  # change every single `000` fell in one contiguous band that opened exactly
  # at this case and closed 50 log lines later, taking secrets, anomalies,
  # airway and sql-query with it — none of which are slow, and none of which
  # had anything to do with each other.
  #
  # So the deadline was never the problem: 30s is generous for an 8.7s call.
  # Serialising costs ~2x wall-clock on ~23 cases and buys back the ~19
  # assertions the saturation was destroying. A timeout that only fires
  # because the harness DDoSed itself measures the harness, not the fleet.
  if [ "$compare" = "workspace_local" ] || [ "$compare" = "workspace_local_named" ]; then
    c1=$(req "$method" "$url1" "$b1" "$body"); printf '%s' "$c1" > "$s1"
    c2=$(req "$method" "$url2" "$b2" "$body"); printf '%s' "$c2" > "$s2"
    cI=$(req "$method" "$urlI" "$bI" "$body"); printf '%s' "$cI" > "$sI"
  else
    req_bg "$method" "$url1" "$b1" "$s1" "$body"; p1=$!
    req_bg "$method" "$url2" "$b2" "$s2" "$body"; p2=$!
    req_bg "$method" "$urlI" "$bI" "$sI" "$body"; p3=$!
    wait "$p1" "$p2" "$p3" 2>/dev/null
  fi
  c1=$(cat "$s1"); c2=$(cat "$s2"); cI=$(cat "$sI")
  mark_covered "$tpl"
  # Replay only what SUCCEEDED here. A route that is already non-2xx with the ide
  # up cannot demonstrate anything about losing the ide: phase 12 would read its
  # standing 404 as "a persisted read was pinned to the stateful node", and phase
  # 13 would read it as "never recovered". Both are lies about a route that was
  # never working in the first place — /logo on a workspace with no logo, an
  # invitation id probed with GET when the route takes POST, a thread with no run
  # yet. Gate on the observed status rather than curating an exclusion list,
  # which would need maintaining and would go stale the first time a fixture
  # gained a logo.
  if [ "$method" = "GET" ] && [[ "$c1" =~ ^2 ]]; then
    REPLAYABLE_GET+=("$(effective_role "$tpl" "$path")|${path}")
  fi

  if [ "$compare" = "degrades_null" ]; then
    # A route the replicas CAN serve, but whose file-counted fields they cannot
    # fill. `null` there is the contract, and it is load-bearing: before this
    # branch of the product existed the same handler returned 0 (the `else` arm
    # of the count block in workspaces/handlers.rs at 553a450b3), so a replica
    # reported "this workspace has no agents" for a workspace holding
    # seventeen. Plain body-equality cannot express that — it would demand the
    # numbers match — and adding the keys to null_volatile would go the other
    # way and stop noticing a regression back to 0. So assert the shape both
    # ways: numbers on the node that can count, exact nulls on the ones that
    # cannot, everything else identical.
    #
    # No `local` here: this runs in the body of the case loop, not a function.
    dn_ok=1
    if dn_all_numeric "$bI"; then
      ok "[$bucket] $label: ide reports real counts ($method $path)"
    else
      bad "[$bucket] $label: ide must report real counts, got none — it owns the working copy ($method $path)"
      dn_ok=0
    fi
    for dn_pair in "serve-1|$b1" "serve-2|$b2"; do
      dn_node="${dn_pair%%|*}"; dn_file="${dn_pair#*|}"
      if dn_all_null "$dn_file"; then
        ok "[$bucket] $label: $dn_node reports null counts, not a fabricated 0 ($method $path)"
      else
        bad "[$bucket] $label: $dn_node returned a COUNT for a field only the working copy can count — a replica must answer null, never a number (a 0 here is the Toast/Slack shape) ($method $path)"
        dn_ok=0
      fi
    done
    if [ "$(dn_blank "$b1")" = "$(dn_blank "$bI")" ] && [ "$(dn_blank "$b1")" = "$(dn_blank "$b2")" ]; then
      ok "[$bucket] $label: every other field matches across all three nodes"
    else
      bad "[$bucket] $label: bodies differ outside the degraded count fields ($method $path)"
      dn_ok=0
    fi
    rm -f "$b1" "$b2" "$bI" "$s1" "$s2" "$sI"
    continue
  fi

  if [ "$compare" = "workspace_local" ] || [ "$compare" = "workspace_local_named" ]; then
    # Do NOT apply the generic serve==ide agreement check below — for this
    # route shape, disagreement is the correct, required outcome. See
    # assert_workspace_local_refusal's header comment for the full case.
    if [[ "$cI" =~ ^2 ]]; then
      ok "[$bucket] $label: ide answers $cI — owns the working copy ($method $path)"
    else
      bad "[$bucket] $label: ide returned $cI, expected 2xx — it owns the working copy, this should succeed ($method $path)"
    fi
    assert_workspace_local_refusal "$bucket" "$label" "serve-1" "$c1" "$b1" "$compare" "$method" "$path"
    assert_workspace_local_refusal "$bucket" "$label" "serve-2" "$c2" "$b2" "$compare" "$method" "$path"
  else
    status_ok=1
    if [ "$c1" != "$c2" ]; then
      bad "[$bucket] $label: serve-1=$c1 serve-2=$c2 differ ($method $path)"
      status_ok=0
    fi
    if [ "$c1" != "$cI" ]; then
      bad "[$bucket] $label: serve=$c1 ide=$cI differ ($method $path)"
      status_ok=0
    fi
    [ "$status_ok" = "1" ] && ok "[$bucket] $label: status $c1 agrees serve-1/serve-2/ide ($method $path)"

    if [ "$compare" = "body" ] && [[ "$c1" =~ ^2[0-9][0-9]$ ]]; then
      n1=$(normalize_body "$b1"); n2=$(normalize_body "$b2"); nI=$(normalize_body "$bI")
      if [ "$n1" = "$n2" ]; then ok "[$bucket] $label: body matches serve-1 vs serve-2"
      else bad "[$bucket] $label: body DIFFERS serve-1 vs serve-2 ($method $path)"; fi
      if [ "$n1" = "$nI" ]; then ok "[$bucket] $label: body matches serve vs ide"
      else bad "[$bucket] $label: body DIFFERS serve vs ide ($method $path)"; fi
    fi
  fi
  rm -f "$b1" "$b2" "$bI" "$s1" "$s2" "$sI"
done

# ── /api/logout: needs its own throwaway session, never the shared $TOKEN ──
step "10b. Logout (throwaway session, isolated from the main token)"
for base in "$S1_BASE" "$S2_BASE" "$IDE_BASE"; do
  lj=$(mktemp)
  curl -s --max-time 10 "${base}/api/auth/dev-login?email=${DEV_EMAIL}" -o "$lj"
  lt=$(jq -r '.token // empty' "$lj"); rm -f "$lj"
  if [ -z "$lt" ]; then skip "logout probe: could not mint a throwaway session on $base"; continue; fi
  c=$(curl -s -o /dev/null -w '%{http_code}' --max-time 10 -H "Authorization: $lt" "${base}/api/logout")
  [ "$c" = "200" ] && ok "logout on $base: $c" || bad "logout on $base: expected 200, got $c"
done
mark_covered "/api/logout"

# ── 10c. Finalize this run's own delete-only-verb fixtures NOW (counted) ───
# These routes have DELETE as their only verb, so — unlike everything above —
# there is no GET to fold coverage into. They must actually run, and run
# BEFORE the coverage report below, to be counted honestly rather than
# silently happening later in the EXIT trap (which is best-effort cleanup,
# not something the coverage report can see). The trap's CLEANUP_CMDS entries
# for these same resources are left in place as a redundant safety net —
# deleting an already-deleted resource twice 404s harmlessly.
step "10c. Finalize delete-only-verb fixtures (files, data-repo, app-integrations)"
if [ "$SCRATCH_FILE_LIVE" = "1" ]; then
  dfc=$(req DELETE "${S1_BASE}/api/${WS}/files/${SCRATCH_FILE_CURRENT_B64:-$SCRATCH_FILE_B64}/delete-file" /dev/null)
  [[ "$dfc" =~ ^2 ]] && ok "files/{pathb64}/delete-file: $dfc" || bad "files/{pathb64}/delete-file: expected 2xx, got $dfc"
  mark_covered "/api/{workspace_id}/files/{pathb64}/delete-file"
  # It was a real, passing GET during the CASE TABLE loop (scratch-file-read)
  # and so is already queued in REPLAYABLE_GET — stop that replay now that
  # we've just deleted the thing it would GET.
  strip_replayable "/api/${WS}/files/${SCRATCH_FILE_CURRENT_B64:-$SCRATCH_FILE_B64}"
fi
if [ "$SCRATCH_FOLDER_LIVE" = "1" ]; then
  dfoc=$(req DELETE "${S1_BASE}/api/${WS}/files/${SCRATCH_FOLDER_B64}/delete-folder" /dev/null)
  [[ "$dfoc" =~ ^2 ]] && ok "files/{pathb64}/delete-folder: $dfoc" || bad "files/{pathb64}/delete-folder: expected 2xx, got $dfoc"
  mark_covered "/api/{workspace_id}/files/{pathb64}/delete-folder"
fi
if [ "$DATA_REPO_LIVE" = "1" ] && [ -n "$DISPOSABLE_WS_ID" ]; then
  drc=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X DELETE -H "Authorization: $TOKEN" \
    "${S1_BASE}/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}")
  [[ "$drc" =~ ^2 ]] && ok "repositories/{name} DELETE: $drc" || bad "repositories/{name} DELETE: expected 2xx, got $drc"
  mark_covered "/api/{workspace_id}/repositories/{name}"
  # Same reasoning as the scratch-file strip above — branch/branches/diff/
  # files all passed as real GETs during the CASE TABLE loop and were
  # queued for ide-down/ide-recovery replay; stop that now that the repo
  # they read is gone. Measured live before this fix: all four replayed as
  # "still 404 after the ide returned", which is this exact self-inflicted
  # shape, not a real recovery defect.
  strip_replayable "/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}/branch"
  strip_replayable "/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}/branches"
  strip_replayable "/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}/diff"
  strip_replayable "/api/${DISPOSABLE_WS_ID}/repositories/${DATA_REPO_NAME}/files"
fi
if [ "$APP_INTEGRATION_OWM_LIVE" = "1" ]; then
  aic=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X DELETE -H "Authorization: $TOKEN" \
    "${S1_BASE}/api/${WS}/app-integrations/openweathermap")
  [[ "$aic" =~ ^2 ]] && ok "app-integrations/{kind} DELETE (openweathermap): $aic" || bad "app-integrations/{kind} DELETE (openweathermap): expected 2xx, got $aic"
  mark_covered "/api/{workspace_id}/app-integrations/{kind}"
fi
if [ "$APP_INTEGRATION_BESTTIME_LIVE" = "1" ]; then
  aic2=$(curl -s -o /dev/null -w '%{http_code}' --max-time 15 -X DELETE -H "Authorization: $TOKEN" \
    "${S1_BASE}/api/${WS}/app-integrations/besttime")
  [[ "$aic2" =~ ^2 ]] && ok "app-integrations/{kind} DELETE (besttime): $aic2" || bad "app-integrations/{kind} DELETE (besttime): expected 2xx, got $aic2"
fi

# ══ COVERAGE REPORT ═══════════════════════════════════════════════════════════
step "11. Coverage report"
BUCKET_NAMES=(Now Fixture Destr Ext Struct)
BUCKET_TOTAL=(0 0 0 0 0)
BUCKET_COVERED=(0 0 0 0 0)
bucket_index() { # name -> prints its index into BUCKET_NAMES, or nothing
  local n="$1" j
  for j in "${!BUCKET_NAMES[@]}"; do [ "${BUCKET_NAMES[$j]}" = "$n" ] && { printf '%s' "$j"; return; }; done
}
for i in "${!CAT_PATH[@]}"; do
  j=$(bucket_index "${CAT_BUCKET[$i]}")
  [ -z "$j" ] && continue
  BUCKET_TOTAL[$j]=$(( ${BUCKET_TOTAL[$j]} + 1 ))
  if is_covered "${CAT_PATH[$i]}"; then
    BUCKET_COVERED[$j]=$(( ${BUCKET_COVERED[$j]} + 1 ))
  fi
done
printf '  %-8s %8s %8s\n' "bucket" "covered" "total"
total_covered=0
for j in "${!BUCKET_NAMES[@]}"; do
  printf '  %-8s %8s %8s\n' "${BUCKET_NAMES[$j]}" "${BUCKET_COVERED[$j]}" "${BUCKET_TOTAL[$j]}"
  total_covered=$((total_covered + BUCKET_COVERED[$j]))
done
printf '  %-8s %8s %8s\n' "TOTAL" "$total_covered" "$declared_total"

printf '\n  Declared but NOT exercised this run (by bucket, with the catalog reason):\n'
for i in "${!CAT_PATH[@]}"; do
  is_covered "${CAT_PATH[$i]}" && continue
  printf '    [%s] %s %s — %s\n' "${CAT_BUCKET[$i]}" "${CAT_METHOD[$i]}" "${CAT_PATH[$i]}" "${CAT_NOTES[$i]:0:100}"
done

if [ "$SKIPPED" -gt 0 ]; then
  printf '\n  Runtime skips (fixture creation failed or was skipped, %d total):\n' "$SKIPPED"
  for n in "${SKIP_NOTES[@]+"${SKIP_NOTES[@]}"}"; do printf '    ⊘ %s\n' "$n"; done
fi

step "12. Kill the ide — persisted reads must survive"
ide_stop
for _ in $(seq 1 30); do curl -sf -m 2 "${IDE_BASE}/api/health" >/dev/null 2>&1 || break; sleep 1; done
if curl -sf -m 3 "${IDE_BASE}/api/health" >/dev/null 2>&1; then
  bad "ide is still answering — phase 12 proves nothing"
else
  ok "ide is down"
  # Replay every GET case this run actually exercised (real, parameter-filled
  # URLs — the catalog's bracketed templates are not valid to request as-is),
  # split by the DECLARED role of its catalog template.
  for entry in "${REPLAYABLE_GET[@]+"${REPLAYABLE_GET[@]}"}"; do
    IFS='|' read -r role rpath <<< "$entry"
    [ "$role" = "fleet-ok" ] || continue
    c=$(code "${S1_BASE}${rpath}")
    [[ "$c" =~ ^2 ]] && ok "FleetOk ${rpath} still ${c} with the ide down" \
                     || bad "FleetOk ${rpath} returned $c with the ide down — a persisted read was pinned to the stateful node"
  done
  for entry in "${REPLAYABLE_GET[@]+"${REPLAYABLE_GET[@]}"}"; do
    IFS='|' read -r role rpath <<< "$entry"
    [ "$role" = "ide-only" ] || continue
    c=$(code "${S1_BASE}${rpath}")
    case "$c" in
      5*) ok "IdeOnly ${rpath} fails loudly ($c)" ;;
      2*) bad "IdeOnly ${rpath} returned $c with the ide down — it answered from a directory this node does not own" ;;
      *)  ok "IdeOnly ${rpath} returned $c while the upstream is gone" ;;
    esac
  done
fi

step "13. Restart the ide — the fleet heals"
ide_start
if wait_health "${IDE_BASE}/api/health" 180; then
  ok "ide back up"
  for entry in "${REPLAYABLE_GET[@]+"${REPLAYABLE_GET[@]}"}"; do
    IFS='|' read -r role rpath <<< "$entry"
    [ "$role" = "ide-only" ] || continue
    c=$(code "${S1_BASE}${rpath}")
    [[ "$c" =~ ^2 ]] && ok "IdeOnly ${rpath} recovered ($c)" || bad "IdeOnly ${rpath} still $c after the ide returned"
  done
else
  bad "ide did not come back"
fi

# ══ BROWSER FLOWS (opt-in) ════════════════════════════════════════════════════
if [ "$FLOWS" = "1" ]; then
  step "14. Browser flows against the replica"
  curl -s --max-time 10 "${S1_BASE}/api/auth/dev-login?email=${FLOW_EMAIL}" -o /tmp/oxy-fleet-assert-flow-session.json
  FLOW_TOKEN=$(python3 -c 'import json;print(json.load(open("/tmp/oxy-fleet-assert-flow-session.json"))["token"])' 2>/dev/null)
  FLOW_USER=$(python3 -c 'import json;print(json.dumps(json.load(open("/tmp/oxy-fleet-assert-flow-session.json")).get("user",{})))' 2>/dev/null)
  if [ -z "${ANTHROPIC_API_KEY:-}" ] && [ -f .env ]; then
    ANTHROPIC_API_KEY=$(grep -E '^ANTHROPIC_API_KEY=' .env | head -1 | cut -d= -f2- | sed 's/^["'"'"']//; s/["'"'"']$//')
    export ANTHROPIC_API_KEY
  fi
  if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    bad "no ANTHROPIC_API_KEY — the flows choose their actions with an LLM and cannot run without one"
  else
    if OXY_BASE_URL="$S1_BASE" \
       OXY_HEALTH_URL="${S1_BASE}/api/health" \
       OXY_SESSION_TOKEN="$FLOW_TOKEN" \
       OXY_SESSION_USER="$FLOW_USER" \
       OXY_FIXTURE_WORKSPACE="$WS" \
       OXY_PATH_PREFIX="/local/workspaces/${WS}" \
       pnpm -C web-app test:agentic "${FLEET_FLOWS[@]}" --no-auto-backend; then
      ok "browser flows passed against ${S1_BASE}"
    else
      bad "browser flows failed against ${S1_BASE} — see the runner output above"
    fi
  fi
fi

printf '\n\033[1m── %s mode: %d passed, %d failed, %d skipped ──\033[0m\n' "$MODE" "$PASS" "$FAIL" "$SKIPPED"
if [ "$FAIL" -gt 0 ]; then
  printf '\nFailures:\n'
  for f in "${FAILURES[@]+"${FAILURES[@]}"}"; do printf '  • %s\n' "$f"; done
  exit 1
fi
exit 0
