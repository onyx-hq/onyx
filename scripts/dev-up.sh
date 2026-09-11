#!/usr/bin/env bash
# `just up` / `just down` / `just status` — the whole local stack, one idempotent command.
#
#   up       `oxy start --enterprise` (Docker Postgres, API :3000, no-auth internal :3001),
#            `oxy seed` (compiles + promotes every seeded workspace), Vite on 127.0.0.1:5173.
#            Reuses whatever is ours and healthy; restarts the backend only when the binary
#            changed. Writes .oxy-dev/state.json — the file an agent reads next.
#   down     Stops the backend + Vite this script started. --db also stops the containers.
#   status   Recorded state + live health. Exit 1 when the stack is not serving.
#
#   up flags   --no-build     use the existing binary ($OXY_BIN, else target/debug/oxy)
#              --no-seed      skip `oxy seed`
#              --no-frontend  API only, no Vite
#              --restart      restart backend + Vite even when healthy
#              --clean        `oxy start --clean`: wipe the Postgres/ClickHouse volumes first
#
# Cloud/enterprise mode only — never `--local`. Sign in with /dev-login?as=<persona>.
# Guide: .claude/skills/oxy-run-and-verify/SKILL.md
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
DEV_DIR="$REPO/.oxy-dev"
RUN_DIR="$DEV_DIR/run"
LOG_DIR="$DEV_DIR/logs"
STATE_JSON="$DEV_DIR/state.json"
STATE_WRITER="$REPO/scripts/dev-up-state.py"

BACKEND_PORT=3000
INTERNAL_PORT=3001
FRONTEND_PORT=5173
PG_PORT=15432
BACKEND_URL="http://127.0.0.1:$BACKEND_PORT"
FRONTEND_URL="http://127.0.0.1:$FRONTEND_PORT"
DATABASE_URL="postgresql://postgres:postgres@localhost:$PG_PORT/oxy"
BIN="${OXY_BIN:-$REPO/target/debug/oxy}"
# Recorded pids are verified against these before anything is signalled.
BACKEND_MATCH="start --enterprise"
FRONTEND_MATCH="--strictPort"

if [ -t 1 ]; then B=$'\033[1m' Y=$'\033[33m' R=$'\033[31m' Z=$'\033[0m'; else B='' Y='' R='' Z=''; fi
step() { printf '\n%s==> %s%s\n' "$B" "$*" "$Z"; }
note() { printf '    %s\n' "$*"; }
warn() { printf '%swarn:%s %s\n' "$Y" "$Z" "$*" >&2; }
die() {
  printf '%serror:%s %s\n' "$R" "$Z" "$*" >&2
  exit 1
}
rel() { printf '%s' "${1#"$REPO"/}"; }

rget() { if [ -f "$RUN_DIR/$1" ]; then cat "$RUN_DIR/$1"; fi; }
rset() { printf '%s\n' "$2" >"$RUN_DIR/$1"; }

# A value from the environment, else from the repo .env (dotenv never overrides the env).
env_or_dotenv() {
  local v="${!1:-}"
  if [ -z "$v" ] && [ -f "$REPO/.env" ]; then
    v=$(grep -E "^$1=" "$REPO/.env" | head -1 | cut -d= -f2- |
      sed -E -e 's/[[:space:]]+#.*$//' -e "s/^[\"']//" -e "s/[\"']$//" || true)
  fi
  printf '%s' "$v"
}

alive() { [ -n "${1:-}" ] && kill -0 "$1" 2>/dev/null; }
group_alive() { [ -n "${1:-}" ] && kill -0 -- "-$1" 2>/dev/null; }
http_ok() { curl -sf --noproxy '*' -o /dev/null --max-time 3 "$1" 2>/dev/null; }

# The recorded pid's process group, if it is still ours. spawn() makes every child a session
# leader, so a live leader must lead its own group and run what we started (a pid recycled
# after a reboot does neither); a group whose leader died but members linger is ours too.
our_group() { # run-file-name match
  local pid pgid cmd
  pid=$(rget "$1.pid")
  [ -n "$pid" ] || return 0
  if alive "$pid"; then
    pgid=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ' || true)
    cmd=$(ps -o command= -p "$pid" 2>/dev/null || true)
    case "$cmd" in *"$2"*) [ "$pgid" = "$pid" ] && printf '%s' "$pid" ;; esac
  elif group_alive "$pid"; then
    printf '%s' "$pid"
  fi
  return 0
}

fingerprint() {
  if [ ! -e "$1" ]; then echo missing; elif [ "$(uname -s)" = Darwin ]; then
    stat -f '%m:%z' "$1"
  else stat -c '%Y:%s' "$1"; fi
}

fresh_log() { # name -> a log path that did not exist before (never reuse or append)
  local base path n=1
  base="$LOG_DIR/$1-$(date +%Y%m%d-%H%M%S)"
  path="$base.log"
  while [ -e "$path" ]; do
    n=$((n + 1))
    path="$base-$n.log"
  done
  : >"$path"
  printf '%s' "$path"
}

# Detached in a new session: survives the calling shell (and an agent's tool call ending),
# and `down` can signal the whole tree (pnpm -> node) as one process group.
spawn() { # log argv... -> pid
  python3 - "$REPO" "$@" <<'PY'
import subprocess, sys
cwd, log, argv = sys.argv[1], sys.argv[2], sys.argv[3:]
with open(log, "ab") as out:
    p = subprocess.Popen(argv, cwd=cwd, stdin=subprocess.DEVNULL, stdout=out,
                         stderr=subprocess.STDOUT, start_new_session=True)
print(p.pid)
PY
}

stop() { # run-file-name match grace-seconds
  local group i=0
  group=$(our_group "$1" "$2")
  if [ -n "$group" ]; then
    note "stopping $1 (process group $group)"
    kill -TERM -- "-$group" 2>/dev/null || true
    while group_alive "$group" && [ "$i" -lt "$3" ]; do
      sleep 1
      i=$((i + 1))
    done
    if group_alive "$group"; then
      warn "$1 still running ${3}s after SIGTERM — SIGKILL"
      kill -KILL -- "-$group" 2>/dev/null || true
    fi
  fi
  rm -f "$RUN_DIR/$1.pid"
}

wait_http() { # url seconds pid log label
  local i=0
  while [ "$i" -lt "$2" ]; do
    if http_ok "$1"; then return 0; fi
    if ! alive "$3"; then
      tail -n 40 "$4" >&2
      die "$5 exited before answering $1 — log: $4"
    fi
    sleep 2
    i=$((i + 2))
  done
  tail -n 40 "$4" >&2
  die "$5 not answering $1 after ${2}s (still running as pid $3; \`just down\` stops it) — log: $4"
}

lock() {
  local holder
  if ! mkdir "$DEV_DIR/lock" 2>/dev/null; then
    holder=$(cat "$DEV_DIR/lock/pid" 2>/dev/null || true)
    alive "$holder" && die "another dev-up.sh (pid $holder) is already running"
    rm -f "$DEV_DIR/lock/pid"
    rmdir "$DEV_DIR/lock" 2>/dev/null || true
    mkdir "$DEV_DIR/lock"
  fi
  echo $$ >"$DEV_DIR/lock/pid"
  trap 'rm -f "$DEV_DIR/lock/pid"; rmdir "$DEV_DIR/lock" 2>/dev/null || true' EXIT
}

# ── preflight ────────────────────────────────────────────────────────────────
need() { command -v "$1" >/dev/null 2>&1 || die "$1 not found on PATH — $2"; }

preflight_tools() {
  need python3 "used for process spawning and state.json"
  need curl "used for health checks"
  need docker "install Docker Desktop or OrbStack"
  docker info >/dev/null 2>&1 ||
    die "Docker daemon not reachable (\`docker info\` failed). Start Docker; from Claude Code, run this outside the Bash sandbox (it needs the Docker socket)."
  [ "$NO_BUILD" = 1 ] || [ -n "${OXY_BIN:-}" ] || need cargo "install rustup"
  [ "$NO_FRONTEND" = 1 ] || need pnpm "see DEVELOPMENT.md prerequisites"
  if [ -n "${OXY_ROLE:-}" ]; then note "OXY_ROLE=$OXY_ROLE is dropped for this stack (oxy start is one all-roles process)"; fi
  if [ -n "$(env_or_dotenv OXY_DEV_LOGIN_EMAILS)" ]; then
    warn "OXY_DEV_LOGIN_EMAILS is set — it replaces the persona roster, so /dev-login?as=<persona> may refuse"
  fi
  if [ -z "$(env_or_dotenv OXY_GLOBAL_ADMINS)" ]; then
    warn "OXY_GLOBAL_ADMINS is not in .env (or the env) — dev-login stays disabled, so no persona can sign in. Add e.g. OXY_GLOBAL_ADMINS=you@oxy.tech to .env"
  fi
}

port_ok() { # port our-group allowed-containers(space-separated) label
  local pids pid pg names c
  pids=$(lsof -nP -iTCP:"$1" -sTCP:LISTEN -t 2>/dev/null | sort -u || true)
  [ -n "$pids" ] || return 0
  if [ -n "$2" ]; then
    for pid in $pids; do
      pg=$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ' || true)
      if [ "$pg" = "$2" ]; then return 0; fi
    done
  fi
  if [ -n "$3" ]; then
    names=" $(docker ps --filter "publish=$1" --format '{{.Names}}' 2>/dev/null | tr '\n' ' ' || true) "
    for c in $3; do
      case "$names" in *" $c "*) return 0 ;; esac
    done
  fi
  printf '%sport %s (%s) is held by a process this stack did not start:%s\n' "$R" "$1" "$4" "$Z" >&2
  lsof -nP -iTCP:"$1" -sTCP:LISTEN 2>/dev/null | sed 's/^/    /' >&2 || true
  return 1
}

preflight_ports() {
  if ! command -v lsof >/dev/null 2>&1; then
    warn "lsof not found — skipping the port-ownership check"
    return 0
  fi
  local bg fg bad=0
  bg=$(our_group backend "$BACKEND_MATCH")
  fg=$(our_group frontend "$FRONTEND_MATCH")
  port_ok "$BACKEND_PORT" "$bg" "" "oxy API" || bad=1
  port_ok "$INTERNAL_PORT" "$bg" "" "oxy internal API" || bad=1
  port_ok "$PG_PORT" "" "oxy-postgres" "Postgres" || bad=1
  if [ "$(env_or_dotenv OXY_OBSERVABILITY_BACKEND)" = clickhouse ]; then
    port_ok 8123 "" "oxy-clickhouse" "ClickHouse HTTP" || bad=1
    port_ok 9000 "" "oxy-clickhouse" "ClickHouse native" || bad=1
  fi
  if [ "$NO_FRONTEND" != 1 ]; then port_ok "$FRONTEND_PORT" "$fg" "" "Vite" || bad=1; fi
  [ "$bad" = 0 ] || die "free the ports above (a stack you started by hand? stop it), then re-run"
}

# ── steps ────────────────────────────────────────────────────────────────────
# The shell may export RUSTC_WRAPPER=sccache: .cargo/config.toml forbids it (sccache hard-errors
# under incremental), and a wrapper that is not installed fails every rustc invocation.
cargo_build() {
  local var val drop=""
  for var in RUSTC_WRAPPER CARGO_BUILD_RUSTC_WRAPPER; do
    val="${!var:-}"
    [ -n "$val" ] || continue
    case "$(basename "$val")" in
      sccache*) note "ignoring $var=$val for this build (.cargo/config.toml: sccache breaks incremental)" ;;
      *)
        if command -v "$val" >/dev/null 2>&1; then continue; fi
        note "ignoring $var=$val for this build (not on PATH)"
        ;;
    esac
    drop="$drop $var"
  done
  (
    for var in $drop; do unset "$var"; done
    cd "$REPO" && cargo build -p oxy-server
  )
}

build() {
  step "build"
  if [ -n "${OXY_BIN:-}" ]; then
    note "OXY_BIN=$OXY_BIN — not building"
    return 0
  fi
  if [ ! -e "$BIN" ]; then
    note "no $(rel "$BIN") yet: a cold build is ~7 min (\`just seed-target\` copies a warm checkout's target/ in ~9s)"
  fi
  local log
  log=$(fresh_log build)
  rset build.log "$log"
  note "cargo build -p oxy-server  (log: $(rel "$log"))"
  if ! cargo_build >"$log" 2>&1; then
    grep -E '^error' "$log" | head -20 >&2 || true
    tail -n 40 "$log" >&2
    die "cargo build failed — log: $log"
  fi
}

backend() {
  step "backend  $BACKEND_URL"
  [ -x "$BIN" ] || die "no oxy binary at $BIN — drop --no-build, or set OXY_BIN"
  local group log pid
  group=$(our_group backend "$BACKEND_MATCH")
  if [ -n "$group" ] && [ "$RESTART" != 1 ] && [ "$CLEAN" != 1 ] &&
    [ "$(fingerprint "$BIN")" = "$(rget backend.fingerprint)" ] && http_ok "$BACKEND_URL/api/ready"; then
    note "reusing pid $group (ready, binary unchanged)"
    return 0
  fi
  if [ -n "$group" ]; then
    if [ "$(fingerprint "$BIN")" != "$(rget backend.fingerprint)" ]; then note "binary changed since pid $group started"; fi
    stop backend "$BACKEND_MATCH" 30
  fi
  log=$(fresh_log backend)
  local fp
  fp=$(fingerprint "$BIN")
  # Auth stays on (MAGIC_LINK_LOCAL_TEST mounts /login + /dev-login); app email previews
  # instead of hitting real SES. Both defer to a value already in the environment.
  pid=$(spawn "$log" env -u OXY_ROLE \
    MAGIC_LINK_LOCAL_TEST="${MAGIC_LINK_LOCAL_TEST:-true}" \
    OXY_APP_EMAIL_LOCAL_TEST="${OXY_APP_EMAIL_LOCAL_TEST:-1}" \
    "$BIN" start --enterprise --port "$BACKEND_PORT" --internal-port "$INTERNAL_PORT" \
    ${CLEAN_FLAG:+"$CLEAN_FLAG"}) || die "could not launch $BIN"
  rset backend.pid "$pid"
  rset backend.fingerprint "$fp"
  rset backend.log "$log"
  note "oxy start --enterprise  pid $pid  (log: $(rel "$log"))"
  # First run also pulls the Postgres (and ClickHouse) images before the server binds.
  wait_http "$BACKEND_URL/api/ready" "${OXY_DEV_UP_READY_SECS:-240}" "$pid" "$log" "backend"
  note "ready"
}

seed() {
  step "seed"
  local log
  log=$(fresh_log seed)
  rset seed.log "$log"
  # --llm-keys: DATABASE_URL is the `oxy start` container, so this box's keys stay on it.
  note "oxy seed --workspace-path ./examples --llm-keys  (log: $(rel "$log"))"
  if ! (cd "$REPO" && env -u OXY_ROLE OXY_DATABASE_URL="$DATABASE_URL" "$BIN" seed --workspace-path ./examples --llm-keys) >"$log" 2>&1; then
    tail -n 40 "$log" >&2
    die "oxy seed failed — log: $log"
  fi
}

# node_modules/.pnpm/lock.yaml is pnpm's copy of the lockfile it last installed. Behind the
# real one, a later `pnpm run`/`exec` may install implicitly (or run against stale deps), so
# install explicitly, frozen, where the failure is visible.
deps_stale() {
  local marker="$REPO/node_modules/.pnpm/lock.yaml"
  [ -d "$REPO/web-app/node_modules" ] || return 0
  [ -f "$marker" ] || return 0
  [ "$REPO/pnpm-lock.yaml" -nt "$marker" ] && ! cmp -s "$REPO/pnpm-lock.yaml" "$marker"
}

frontend() {
  step "frontend  $FRONTEND_URL"
  local group log pid installed=0
  group=$(our_group frontend "$FRONTEND_MATCH")
  if deps_stale; then
    if [ -n "$group" ]; then stop frontend "$FRONTEND_MATCH" 10; fi
    group=""
    log=$(fresh_log pnpm-install)
    note "node_modules missing or behind pnpm-lock.yaml — pnpm install --frozen-lockfile  (log: $(rel "$log"))"
    (cd "$REPO" && pnpm install --frozen-lockfile --config.confirmModulesPurge=false) >"$log" 2>&1 || {
      tail -n 40 "$log" >&2
      die "pnpm install failed — log: $log"
    }
    installed=1
  fi
  if [ -n "$group" ] && [ "$RESTART" != 1 ] && [ "$installed" = 0 ] && http_ok "$FRONTEND_URL/"; then
    note "reusing pid $group"
  else
    if [ -n "$group" ]; then stop frontend "$FRONTEND_MATCH" 10; fi
    log=$(fresh_log frontend)
    pid=$(spawn "$log" env OXY_DEV_HOST=127.0.0.1 OXY_DEV_PORT="$FRONTEND_PORT" \
      OXY_DEV_PROXY_TARGET="http://localhost:$BACKEND_PORT" \
      pnpm --dir web-app run dev "$FRONTEND_MATCH") || die "could not launch pnpm"
    rset frontend.pid "$pid"
    rset frontend.log "$log"
    note "vite  pid $pid  (log: $(rel "$log"))"
    wait_http "$FRONTEND_URL/" 90 "$pid" "$log" "Vite"
  fi
  http_ok "$FRONTEND_URL/api/health" || warn "Vite is up but its /api proxy does not reach the backend"
}

write_state() {
  local fe_log="" fe_pid=""
  if [ -n "$(our_group frontend "$FRONTEND_MATCH")" ]; then
    fe_pid=$(rget frontend.pid)
    fe_log=$(rget frontend.log)
  fi
  STATE_JSON="$STATE_JSON" REPO="$REPO" BIN="$BIN" DATABASE_URL="$DATABASE_URL" \
    BACKEND_URL="$BACKEND_URL" INTERNAL_URL="http://127.0.0.1:$INTERNAL_PORT" FRONTEND_URL="$FRONTEND_URL" \
    BACKEND_PID="$(rget backend.pid)" FRONTEND_PID="$fe_pid" \
    BACKEND_LOG="$(rget backend.log)" FRONTEND_LOG="$fe_log" SEED_LOG="$(rget seed.log)" \
    BUILD_LOG="$(rget build.log)" STAFF_EMAILS="$(env_or_dotenv OXY_GLOBAL_ADMINS)" \
    python3 "$STATE_WRITER"
}

status() {
  local bpid fpid code=0 fp
  bpid=$(our_group backend "$BACKEND_MATCH")
  fpid=$(our_group frontend "$FRONTEND_MATCH")
  printf '%sbackend%s   ' "$B" "$Z"
  if [ -z "$bpid" ]; then
    echo "not running (nothing recorded by just up)"
    code=1
  elif http_ok "$BACKEND_URL/api/ready"; then
    fp="binary unchanged"
    [ "$(fingerprint "$BIN")" = "$(rget backend.fingerprint)" ] || fp="binary REBUILT since start — \`just up\` restarts it"
    echo "pid $bpid  $BACKEND_URL/api/ready ok  ($fp)"
  else
    echo "pid $bpid  $BACKEND_URL/api/ready NOT ready"
    code=1
  fi
  printf '%sfrontend%s  ' "$B" "$Z"
  if [ -z "$fpid" ]; then
    echo "not running"
    code=1
  elif http_ok "$FRONTEND_URL/"; then
    if http_ok "$FRONTEND_URL/api/health"; then echo "pid $fpid  $FRONTEND_URL ok (proxy ok)"; else echo "pid $fpid  $FRONTEND_URL ok, /api proxy FAILING"; fi
  else
    echo "pid $fpid  $FRONTEND_URL NOT answering"
    code=1
  fi
  printf '%scontainers%s ' "$B" "$Z"
  docker ps --format '{{.Names}}: {{.Status}}' 2>/dev/null | grep -E '^oxy-(postgres|clickhouse):' | tr '\n' ' ' || printf 'none running'
  echo
  local name path
  for name in build backend seed frontend; do
    path=$(rget "$name.log")
    if [ -n "$path" ]; then printf '%slog%s       %-8s %s\n' "$B" "$Z" "$name" "$(rel "$path")"; fi
  done
  if [ -f "$STATE_JSON" ]; then printf '%sstate%s     %s\n' "$B" "$Z" "$(rel "$STATE_JSON")"; fi
  return "$code"
}

usage() { sed -n '2,18p' "$0" | sed -E 's/^# ?//'; }

# ── main ─────────────────────────────────────────────────────────────────────
CMD=up
case "${1:-}" in
  up | down | status | help) CMD="$1" && shift ;;
esac
NO_BUILD=0 NO_SEED=0 NO_FRONTEND=0 RESTART=0 CLEAN=0 DB=0 CLEAN_FLAG=""
for arg in "$@"; do
  case "$arg" in
    --no-build) NO_BUILD=1 ;;
    --no-seed) NO_SEED=1 ;;
    --no-frontend) NO_FRONTEND=1 ;;
    --restart) RESTART=1 ;;
    --clean) CLEAN=1 CLEAN_FLAG="--clean" ;;
    --db) DB=1 ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown argument: $arg (commands: up | down | status; flags: see --help)" ;;
  esac
done
mkdir -p "$RUN_DIR" "$LOG_DIR"
# Without ps every recorded pid reads as foreign, and `up` would fight its own stack for ports.
ps -o pid= -p $$ >/dev/null 2>&1 ||
  die "\`ps\` is not permitted in this shell (a sandbox?) — dev-up.sh inspects and signals processes; run it unsandboxed"

case "$CMD" in
  up)
    lock
    preflight_tools
    preflight_ports
    [ "$NO_BUILD" = 1 ] || build
    backend
    [ "$NO_SEED" = 1 ] || seed
    [ "$NO_FRONTEND" = 1 ] || frontend
    step "state"
    write_state
    ;;
  down)
    lock
    stop frontend "$FRONTEND_MATCH" 10
    stop backend "$BACKEND_MATCH" 30
    rm -f "$STATE_JSON"
    if [ "$DB" = 1 ]; then
      running=" $(docker ps --format '{{.Names}}' 2>/dev/null | tr '\n' ' ' || true) "
      for c in oxy-postgres oxy-clickhouse; do
        case "$running" in
          *" $c "*)
            note "docker stop $c"
            docker stop "$c" >/dev/null
            ;;
        esac
      done
    fi
    note "stopped (logs kept in $(rel "$LOG_DIR"))"
    ;;
  status) status ;;
  help) usage ;;
esac
