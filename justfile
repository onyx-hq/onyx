# Default: list available recipes
default:
    @just --list

# ── Setup ─────────────────────────────────────────────────────────────────────

# Full initial setup (install all dependencies)
setup: install

# Install Rust + Node dependencies and tools
install:
    @echo "==> Checking Rust toolchain..."
    rustup show
    @echo "==> Fetching Rust crate dependencies..."
    cargo fetch
    @echo "==> Installing cargo-nextest..."
    @cargo nextest --version >/dev/null 2>&1 || cargo install cargo-nextest --locked
    @echo "==> Installing Node dependencies..."
    pnpm install
    @just install-hooks
    @echo "Done. Run 'just dev' to start the development servers."

# Point git at the tracked hook dir, so a new worktree seeds its own target/.
#
# .githooks owns core.hooksPath outright: package.json's `prepare` sets that
# config directly rather than running `husky`, because husky v9 claims
# core.hooksPath for .husky/_ and would silently take it back on the next
# `pnpm install` — which is exactly what happened, leaving post-checkout dead and
# every new worktree unseeded. Setting it from `prepare` keeps every install
# route self-wiring (a plain `pnpm install` on a fresh clone still gets hooks,
# which is how commitlint and lint-staged reach anyone who never runs
# `just install`) without letting husky reclaim the path. We need a *tracked*
# dir anyway: .husky/_ is generated, so it does not exist in a brand-new
# worktree — the exact case post-checkout is for.
#
# husky stays a devDependency only to keep .husky/ meaningful; nothing invokes
# it. .githooks carries shims that forward commit-msg / pre-commit / pre-push to
# the scripts in .husky/, so commitlint and lint-staged keep running. Add a shim
# whenever a hook is added to .husky/.
install-hooks:
    @git config core.hooksPath .githooks
    @echo "==> git hooks enabled (.githooks, forwarding to .husky)"

# It removes the dependency graph for good (a cold build is 88% registry deps),
# but NOT the edit loop: rustc's incremental cache cannot be shared, so your
# first edit to each crate still compiles it from scratch — ~61s for oxy-app on
# `cargo check`, and `cargo build` is several times that, against 418s cold.
#
# Auto-picks the closest warm sibling; pass one to override. Runs automatically
# on `git worktree add` once hooks are installed. The win tracks third-party
# dependency drift, so glance at the "N differ" line it prints. See
# internal-docs/rust-build-performance.md.
#
# Single-arg and quoted: a source path with a space used to split into two argv
# entries and die in argparse as `unrecognized arguments`.
#
# Seed this checkout's target/ from a warm checkout (~9s, ~1.3G of copied build/).
seed-target source="":
    src={{ quote(source) }}; python3 scripts/seed-target.py ${src:+"$src"}

# Seed for the DEV-DYNAMIC loop (`just build-backend-dyn`): prefers a source that
# already built the dylib, so this checkout reuses the seeded ~1.4 GB
# liboxy_app_dylib.dylib (cold start ~3 min) instead of the ~20 min link. Build
# dynamic once in a warm "golden" worktree first (`just build-backend-dyn`), then
# `just seed-dyn` (auto-picks it) — or `just seed-dyn <golden>` to name it. Warns
# if no dynamic-capable source is found.
seed-dyn source="":
    src={{ quote(source) }}; python3 scripts/seed-target.py --dynamic ${src:+"$src"}

# ── Build ──────────────────────────────────────────────────────────────────────

# Build everything (debug)
build: build-backend build-frontend

# Build the Rust backend (debug)
build-backend:
    cargo build 2>&1 | grep -E "^(error|warning\[)" || true

# Build the backend FAST: full debug, but skips V8/Functions (deno_core) — a much
# smaller binary to link. Opt back in with `just build-backend` when editing Functions.
build-backend-fast:
    cargo bf 2>&1 | grep -E "^(error|warning\[)" || true

# Build the frontend
build-frontend:
    pnpm build

# ── Check / Lint ───────────────────────────────────────────────────────────────

# Run cargo check (fast type-check)
check:
    cargo check 2>&1 | grep -E "^(error|warning\[)" || true

# Type-check FAST: skips V8/Functions (deno_core).
check-fast:
    cargo cf 2>&1 | grep -E "^(error|warning\[)" || true

# Lint everything
lint: lint-backend lint-frontend

# Run clippy
lint-backend:
    cargo clippy --workspace

# Run ESLint / Biome
lint-frontend:
    pnpm lint

# Validate inter-crate dependency rules (see internal-docs/backend-architecture.md)
check-deps:
    python3 scripts/check-deps.py

# DRY up workspace Cargo.toml manifests by inheriting shared deps from workspace root
autoinherit:
    @cargo autoinherit --version >/dev/null 2>&1 || cargo install cargo-autoinherit
    cargo autoinherit

# Format all code (clippy auto-fix + rustfmt + frontend)
fmt:
    cargo clippy --fix --allow-dirty --allow-staged --broken-code --workspace --lib && cargo fmt --all
    pnpm --filter oxy-web run format

# Check formatting without writing
fmt-check:
    cargo fmt --check
    pnpm --filter oxy-web run format:check

# ── Test ───────────────────────────────────────────────────────────────────────
#
# Pick the SMALLEST recipe that covers what you changed. The cost here is
# dominated by linking, not by running: every integration-test target is its own
# binary that statically links the whole dep graph (DuckDB + DataFusion + Arrow +
# AWS SDK), so each one costs a multi-hundred-MB link before a single assertion
# runs. Unit tests all share ONE binary per crate, which is why `unit` is an
# order of magnitude cheaper than anything that builds `tests/`.
#
#   just unit oxy-app     <- default inner loop; ~1.3k tests, one link
#   just test-crate oxy-app
#   just test             <- whole workspace; CI does this, you rarely need to

# Run all tests with nextest (whole workspace — slow; prefer `unit` / `test-crate`)
test:
    cargo nextest run

# Unit tests only (`src/**` `#[cfg(test)]`) for one crate — ONE binary, one link.
# This is the right verification loop for almost every change.
unit crate="oxy-app":
    cargo nextest run -p {{crate}} --lib

# Every test for one crate, unit + integration binaries.
test-crate crate="oxy-app":
    cargo nextest run -p {{crate}}

# Run tests matching a nextest filterset, e.g.
#   just test-filter 'test(authz)'
#   just test-filter 'binary(custom_apps)'
test-filter expr:
    cargo nextest run -E '{{expr}}'

# Compile every test target without running any — catches breakage cheaply.
test-build:
    cargo nextest list --workspace >/dev/null

# Remove dangling test containers (Postgres/ClickHouse/MySQL) left by test runs
clean-test-containers:
    @docker ps -aq --filter "label=org.testcontainers.managed-by=testcontainers" | xargs -r docker rm -f

# ── Dev servers ────────────────────────────────────────────────────────────────

# Print instructions for starting backend + frontend dev servers
dev:
    @echo "Run in separate terminals:"
    @echo "  just dev-backend"
    @echo "  just dev-frontend"

# Flags: --no-build --no-seed --no-frontend --restart --clean. Guide: the oxy-run-and-verify skill.
# Whole stack, detached + idempotent: build, `oxy start --enterprise`, seed, Vite -> .oxy-dev/state.json
up *FLAGS:
    @bash scripts/dev-up.sh up {{ FLAGS }}

# Stop what `just up` started; --db also stops the oxy-postgres / oxy-clickhouse containers.
down *FLAGS:
    @bash scripts/dev-up.sh down {{ FLAGS }}

# Health of the `just up` stack; exits 1 when it is not serving.
status:
    @bash scripts/dev-up.sh status

# Start the Rust API server (http://localhost:3000)
dev-backend:
    cargo run start

# Start the API server FAST: skips V8/Functions (deno_core) for a quicker
# build+run loop. Full debug info; Functions endpoints are inert.
dev-backend-fast:
    cargo rf -- start

# Start the API server with DYNAMIC LINKING (Phase 4, macOS). The ~1.4 GB of
# heavy deps live in liboxy_app_dylib.dylib, linked ONCE (~20 min the first time,
# a separate fingerprint from the static build). After that each surface edit
# relinks in ~17s instead of ~52s. Baked rpaths (rustlib + @executable_path) let
# ./target/debug/oxy find its dylibs with no DYLD_LIBRARY_PATH. `dev-dynamic`
# is dev-only — CI/release never build the dylib. `just build-backend-dyn` to
# build without running (e.g. to time it).
_dyn-rustflags:
    #!/usr/bin/env bash
    set -euo pipefail
    SYSROOT="$(rustc --print sysroot)"; HOST="$(rustc -vV | awk '/host:/{print $2}')"
    printf -- '-C prefer-dynamic -C link-arg=-Wl,-rpath,%s/lib/rustlib/%s/lib -C link-arg=-Wl,-rpath,@executable_path -C link-arg=-Wl,-rpath,@executable_path/deps' "$SYSROOT" "$HOST"

build-backend-dyn:
    # Full output (no grep filter / `|| true`): this is the recipe most likely to
    # fail at LINK, where the actionable part is the `ld: Undefined symbols …` notes
    # under `error: linking with cc failed` — a grep for `^error` drops exactly those.
    RUSTFLAGS="$(just _dyn-rustflags)" cargo build -p oxy-server --features dev-dynamic

dev-backend-dyn *ARGS="start":
    RUSTFLAGS="$(just _dyn-rustflags)" cargo run -p oxy-server --features dev-dynamic -- {{ ARGS }}

# `oxy start` runs its OWN oxy-clickhouse container — don't also `just clickhouse-up` (both bind :8123).
# Start the API server with ClickHouse observability + enterprise UI
dev-backend-obs:
    #!/usr/bin/env bash
    set -euo pipefail
    set -a; source .env.clickhouse; set +a
    cargo run -p oxy-server -- start --enterprise

# Start the Vite dev server (http://localhost:5173)
dev-frontend:
    pnpm run dev

# Start the OAuth bounce proxy (http://localhost:8429). Lets several local
# dev instances share one registered redirect URI per provider (Google + GitHub);
# it forwards each callback back to the instance that started the flow.
# See scripts/oauth-bounce.mjs.
oauth-proxy:
    node scripts/oauth-bounce.mjs

# ── Database / Seed ────────────────────────────────────────────────────────────

# Seed the demo project (guest user + Local org + nil-UUID workspace at ./examples).
seed:
    cargo run -- seed

# Drop the demo workspace row.
seed-clear:
    cargo run -- seed --clear

# Run database migrations manually
migrate:
    cargo run --bin migration

# ── Customer-apps publish smoke test ───────────────────────────────────────────
#
# End-to-end smoke for the self-serve publish pipeline against a local
# `oxy serve` using the FILESYSTEM build store (no S3/MinIO). Publishes a
# pre-built bundle via `oxyc publish`, then fetches the served URL and checks
# HTML came back. Exit 0 = green; any failure aborts immediately.
#
# Usage:
#     just test-customer-apps-publish <org> <slug> <bundle-dir> <project-id>
#
# Example:
#     just test-customer-apps-publish test hello-oxy \
#         /Users/luong/oxy-hq/customer-apps/examples/hello-oxy/out \
#         cdba75a2-c074-4dfa-a77c-a505b2845944
#
# Prerequisites (one-time):
#   - `cargo build -p oxy-server`  (provides ./target/debug/oxy, for `oxy serve`)
#   - `pnpm --dir sdk/cli build`   (provides sdk/cli/dist/main.mjs, the oxyc that publishes)
#   - A local `oxy serve` running WITHOUT OXY_CUSTOMER_APPS_S3_BUCKET set, so
#     builds land in the state dir, e.g.:
#         OXY_STATE_DIR="$HOME/.local/share/oxy" ./target/debug/oxy serve
#   - Logged in as an app-admin: `node sdk/cli/dist/main.mjs login --env local`
#     (or export OXY_TOKEN=<app-admin api key>)
#   - A real project UUID in your local oxy DB (passed as <project-id> — the
#     app may be new, so build-config can't resolve it yet)
#
# Full background: internal-docs/customer-apps.md §3d + the self-serve design.
test-customer-apps-publish org slug bundle project_id:
    #!/usr/bin/env bash
    set -euo pipefail

    TARGET="${OXY_TARGET:-http://localhost:3000}"
    OXYC="node sdk/cli/dist/main.mjs"

    if [ ! -f "{{ bundle }}/index.html" ]; then
        echo "ERROR: {{ bundle }}/index.html missing — every bundle must have one" >&2
        exit 1
    fi
    if [ ! -f "sdk/cli/dist/main.mjs" ]; then
        echo "ERROR: sdk/cli/dist/main.mjs missing — run 'pnpm --dir sdk/cli build' first" >&2
        exit 1
    fi

    # Token: OXY_TOKEN, else the `oxyc login` cache for this host — asked of
    # oxyc, which knows where that file lives on this OS (not ~/.config on macOS).
    TOKEN="${OXY_TOKEN:-$($OXYC token --target "$TARGET" 2>/dev/null || true)}"
    if [ -z "$TOKEN" ]; then
        echo "ERROR: not authenticated for $TARGET." >&2
        echo "       Run: node sdk/cli/dist/main.mjs login --env local   (or export OXY_TOKEN=<app-admin key>)" >&2
        exit 1
    fi

    # Serve must be up (FS backend = OXY_CUSTOMER_APPS_S3_BUCKET unset in serve's env).
    if ! curl -sf "$TARGET/api/health" >/dev/null 2>&1 && ! curl -sf "$TARGET/health" >/dev/null 2>&1; then
        echo "ERROR: no oxy serve responding at $TARGET." >&2
        echo "       Start it: OXY_STATE_DIR=\"\$HOME/.local/share/oxy\" ./target/debug/oxy serve" >&2
        exit 1
    fi

    echo "==> [1/2] Publishing {{ org }}/{{ slug }} → $TARGET (filesystem build store, live channel)…"
    $OXYC publish \
        --target "$TARGET" \
        --org {{ org }} \
        --app {{ slug }} \
        --project {{ project_id }} \
        --dir "{{ bundle }}" \
        --promote

    echo "==> [2/2] Fetching the served bundle…"
    URL="$TARGET/customer-apps/{{ org }}/{{ slug }}/"
    BODY="$(curl -sf -H "Authorization: Bearer $TOKEN" "$URL")"
    if ! echo "$BODY" | grep -qiE "<html|<!doctype|__OXY_APP__"; then
        echo "ERROR: $URL did not return recognizable HTML" >&2
        exit 1
    fi

    echo
    echo "✓ Smoke OK — published to the state dir and served from $URL"

# ── Release ────────────────────────────────────────────────────────────────────

# Preview the next release version and unreleased changelog (no side effects).
release-preview:
    @echo "==> Next version:"
    @uv run scripts/release/bump-version.py --dry-run
    @echo ""
    @echo "==> Unreleased changelog:"
    @git cliff --unreleased

# Dry-run: generate a combined changelog draft for one or more past releases.
# Example: just release-changelog-preview 0.5.34
# Example: just release-changelog-preview 0.5.33 0.5.34 0.5.35
release-changelog-preview +VERSIONS:
    uv run scripts/release/update-content-changelog.py --dry-run {{ VERSIONS }}

# Manually trigger the release PR workflow on GitHub (requires gh CLI + auth).
release-trigger:
    gh workflow run prepare-release.yaml --ref main

# ── Airhouse local stack ───────────────────────────────────────────────────────

# Boot the local airhouse stack.
airhouse-up:
    docker compose -f docker-compose.airhouse.yml up -d
    @echo
    @echo "Next — streamlined (recommended), two commands:"
    @echo "  just airhouse-precompile   # migrate + seed + compile+PROMOTE the demo workspace"
    @echo "  just airhouse-fleet        # build + run ide :3000 + serve :3002 (env auto-set)"
    @echo "  # then: just routing-check 3002"
    @echo
    @echo "Or step-by-step:"
    @echo "  set -a; source .env.airhouse; set +a"
    @echo "  cargo run -p migration --bin migration                                     # one-shot"
    @echo "  cargo run -p oxy-server -- seed                                               # guest user + Local org + workspace at ./examples + OXY_GLOBAL_ADMINS as Owners"
    @echo "  # Compile+PROMOTE the seeded workspace. ABSOLUTE path: a RELATIVE --workspace-path"
    @echo "  # silently under-compiles (the discover globs miss → only config.yml). --promote sets"
    @echo "  # current_revision_id so the serve fleet can actually read it."
    @echo "  cargo run -p oxy-server -- compile --workspace-path $PWD/examples --workspace-id 70787bb2-e11b-5488-b2c3-02e60d5fc7d3 --enterprise --promote"
    @echo "  # Split fleet — run BOTH (the serve node self-proxies IdeOnly → the ide node)."
    @echo "  # The serve node's --internal-port 0 turns OFF its internal API (which also"
    @echo "  # defaults to 3001) so it can't clash with the ide node's internal API on :3001."
    @echo "  # OXY_INPROC_GLOBAL_WORKER=1 on the IDE node is REQUIRED: it runs the global"
    @echo "  # driver that DRAINS the compile queue. Without it, admin/auto compiles sit"
    @echo "  # 'queued' forever, no revision is produced, and the serve node 503s every"
    @echo "  # compiled read with needs_recompile. The serve node must NOT have it (it has"
    @echo "  # no working copy; --no-workers keeps it a pure reader). Mirrors oxy-dev, where"
    @echo "  # the StatefulSet sets OXY_INPROC_GLOBAL_WORKER=1 and the serve fleet strips it."
    @echo "  OXY_ROLE=ide   OXY_INPROC_GLOBAL_WORKER=1 cargo run -p oxy-server -- serve --enterprise  # ide node (full FS + drains compiles; main :3000, internal :3001)"
    @echo "  OXY_ROLE=serve OXY_IDE_UPSTREAM=http://localhost:3000 cargo run -p oxy-server -- serve --enterprise --no-workers --port 3002 --internal-port 0   # stateless serve node"
    @echo "  just routing-check 3002                                                   # probe serve → IdeOnly shows Forwarded-Via: serve + Served-By: ide"

# One-shot precompile: migrate + seed + compile+PROMOTE the demo workspace into
# Postgres so the stateless serve fleet can read it. Idempotent — re-run freely.
# Run after `just airhouse-up`. The workspace UUID is deterministic:
# Uuid::new_v5(NAMESPACE_DNS, "demo.oxy.local") = 70787bb2-… (seed.rs).
airhouse-precompile:
    #!/usr/bin/env bash
    set -euo pipefail
    set -a; source .env.airhouse; set +a
    echo "→ waiting for postgres"
    for _ in $(seq 1 60); do
      docker compose -f docker-compose.airhouse.yml exec -T airhouse-postgres \
        pg_isready -U airhouse -d oxydb >/dev/null 2>&1 && break || sleep 1
    done
    echo "→ migrate"; cargo run -p migration --bin migration
    echo "→ seed";    cargo run -p oxy-server -- seed
    echo "→ compile + promote (ABSOLUTE path — a relative --workspace-path under-compiles)"
    cargo run -p oxy-server -- compile --workspace-path "$PWD/examples" \
      --workspace-id 70787bb2-e11b-5488-b2c3-02e60d5fc7d3 --enterprise --promote --skip-migrations
    echo "✓ demo workspace compiled + promoted. Next: just airhouse-fleet"

# Build once, then run the split fleet (ide :3000 + serve :3002). The ide starts
# first and finishes migrating before serve starts (avoids a concurrent-migration
# race). OXY_INPROC_GLOBAL_WORKER on the ide node drains the compile queue; the
# serve node serves the latest compiled revision regardless of branch (no per-node
# default-branch config needed). Both nodes' logs stream here, prefixed
# [ide]/[serve] (also saved raw to /tmp/oxy-*.log). Ctrl-C stops both.
airhouse-fleet:
    #!/usr/bin/env bash
    set -euo pipefail
    set -a; source .env.airhouse; set +a
    echo "→ build"; cargo build -p oxy-server
    bin=./target/debug/oxy

    # Each node streams to THIS terminal (prefixed [ide]/[serve]) AND to a raw
    # /tmp log — so a startup crash (e.g. a failed migration) is visible here, not
    # hidden in a file you have to tail. The ide starts FIRST and we block until
    # it has migrated + bound :3000 before starting serve: launching both at once
    # ran DB migrations concurrently, and the CREATE TYPE enum migrations collided
    # (duplicate key on pg_type), crashing whichever node lost the race.
    echo "→ ide :3000 (owns migrations)"
    OXY_ROLE=ide OXY_INPROC_GLOBAL_WORKER=1 "$bin" serve --enterprise \
        > >(tee /tmp/oxy-ide.log | awk '{ print "[ide]   " $0; fflush() }') 2>&1 &
    ide=$!
    trap 'echo; echo stopping; kill $ide ${serve:-} 2>/dev/null || true' INT TERM EXIT

    echo "→ waiting for ide :3000 (migrations + bind)…"
    until curl -sf -m2 http://localhost:3000/health >/dev/null 2>&1; do
        kill -0 "$ide" 2>/dev/null || { echo "[fleet] ide exited during startup — see [ide] log above"; exit 1; }
        sleep 1
    done

    echo "→ serve :3002 (migrations already applied by ide)"
    OXY_ROLE=serve OXY_IDE_UPSTREAM=http://localhost:3000 "$bin" serve --enterprise --no-workers --port 3002 --internal-port 0 \
        > >(tee /tmp/oxy-serve.log | awk '{ print "[serve] " $0; fflush() }') 2>&1 &
    serve=$!

    echo "fleet up | logs streaming here (+ /tmp/oxy-{ide,serve}.log) | just routing-check 3002"
    wait

# Tear it down (drops volumes; pass --keep-data to retain).
airhouse-down *FLAGS:
    docker compose -f docker-compose.airhouse.yml down {{ if FLAGS =~ "--keep-data" { "" } else { "-v" } }}

# Tail logs; pass a service name to focus.
airhouse-logs *SERVICE:
    docker compose -f docker-compose.airhouse.yml logs -f {{ SERVICE }}

# Show services, buckets, and DBs.
airhouse-status:
    @docker compose -f docker-compose.airhouse.yml ps
    @echo
    @echo "==> Buckets on MinIO:"
    @docker compose -f docker-compose.airhouse.yml run --rm --no-deps -T --entrypoint sh airhouse-createbucket \
        -c 'mc alias set m http://airhouse-minio:9000 minioadmin minioadmin >/dev/null && mc ls m' \
        2>/dev/null || echo "  (minio not reachable yet)"
    @echo
    @echo "==> Databases on airhouse-postgres:"
    @docker compose -f docker-compose.airhouse.yml exec -T airhouse-postgres \
        psql -U airhouse -d airhouse -tc \
        "SELECT datname FROM pg_database WHERE datname IN ('airhouse','airhouse_cp','oxydb') ORDER BY 1" \
        2>/dev/null || echo "  (postgres not reachable yet)"

# psql shell on oxydb.
airhouse-psql:
    docker compose -f docker-compose.airhouse.yml exec airhouse-postgres \
        psql -U airhouse -d oxydb

# Print blob keys from PG + objects from MinIO.
airhouse-verify-blobs:
    @echo "==> semantic_views with blob keys:"
    @set -a; . ./.env.airhouse; set +a; \
        psql "$OXY_DATABASE_URL" -c \
        "SELECT name, substring(compiled_sql_blob_key, 1, 80) AS key FROM semantic_views WHERE compiled_sql_blob_key IS NOT NULL LIMIT 10;"
    @echo
    @echo "==> Objects in s3://oxy-compile-blobs/workspaces/:"
    @set -a; . ./.env.airhouse; set +a; \
        AWS_PAGER='' aws --endpoint-url "$AWS_ENDPOINT_URL" \
        s3 ls "s3://${OXY_COMPILE_BLOB_S3_BUCKET}/workspaces/" --recursive

# ── ClickHouse local observability stack ────────────────────────────────────────

# ClickStack = HyperDX UI + ClickHouse + OTel collector, the store for the
# PLATFORM telemetry oxy exports (traces + logs of the oxy process itself). It is
# the operator store — unrelated to the oxy-clickhouse container below, which is
# the product observability store the in-app Traces console reads.
# Ports: 8080 UI, 4318 OTLP/HTTP (what oxy speaks), 4317 OTLP/gRPC.

# Boot a local ClickStack (HyperDX on :8080, OTLP on :4318) for platform telemetry.
clickstack-up:
    #!/usr/bin/env bash
    set -euo pipefail
    if docker ps -a --format '{{{{.Names}}}}' | grep -qx oxy-clickstack; then
      docker start oxy-clickstack >/dev/null
    else
      docker run -d --name oxy-clickstack \
        -p 8080:8080 -p 4317:4317 -p 4318:4318 \
        -v oxy-clickstack-db:/data/db \
        -v oxy-clickstack-ch:/var/lib/clickhouse \
        docker.hyperdx.io/hyperdx/hyperdx-all-in-one >/dev/null
    fi
    echo
    echo "ClickStack booting: HyperDX on http://localhost:8080 (create the first user there)."
    echo
    echo "Point oxy at its collector, then run a server-shaped command (serve / start / worker):"
    echo "  export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318"
    echo "  export OTEL_LOGS_EXPORTER=otlp        # nothing tails stdout locally, so ship logs too"
    echo "  export OTEL_RESOURCE_ATTRIBUTES=deployment.environment=local"
    echo "  cargo run -p oxy-server -- serve"
    echo
    echo "The boot log line 'OTLP export enabled' confirms the exporters are up; every"
    echo "HTTP request is then a 'GET /api/…' span in HyperDX, with its logs attached."
    echo "Operator guide: internal-docs/platform-telemetry.md"

# Tear the local ClickStack down (drops its volumes; pass --keep-data to retain).
clickstack-down *FLAGS:
    docker rm -f oxy-clickstack >/dev/null 2>&1 || true
    {{ if FLAGS =~ "--keep-data" { "@true" } else { "docker volume rm oxy-clickstack-db oxy-clickstack-ch >/dev/null 2>&1 || true" } }}

# Boot the local ClickHouse observability server.
clickhouse-up:
    docker compose -f docker-compose.clickhouse.yml up -d
    @echo
    @echo "ClickHouse booting on http://localhost:8123 (database: observability)."
    @echo "Check readiness:  just clickhouse-status"
    @echo
    @echo "Point oxy at it (boot creates the obs schema — tables + rollup MV + backfill):"
    @echo "  set -a; source .env.clickhouse; set +a"
    @echo "  cargo run -p oxy-server -- serve --enterprise"
    @echo
    @echo "Then verify — no LLM/agent run needed:"
    @echo "  just clickhouse-status        # tables + row counts"
    @echo "  just clickhouse-obs-verify    # synthetic-span smoke test: rollup MV + percentile/histogram/cost SQL"
    @echo "  just clickhouse-client        # interactive SQL shell"

# Tear it down (drops the data volume; pass --keep-data to retain).
clickhouse-down *FLAGS:
    docker compose -f docker-compose.clickhouse.yml down {{ if FLAGS =~ "--keep-data" { "" } else { "-v" } }}

# Tail ClickHouse logs.
clickhouse-logs:
    docker compose -f docker-compose.clickhouse.yml logs -f

# Show status: service + observability tables + row counts.
clickhouse-status:
    @docker compose -f docker-compose.clickhouse.yml ps
    @echo
    @echo "==> Tables in observability:"
    @docker compose -f docker-compose.clickhouse.yml exec -T clickhouse \
        clickhouse-client --query "SHOW TABLES FROM observability" 2>/dev/null \
        || echo "  (not ready, or oxy hasn't created the schema yet — run oxy serve with .env.clickhouse)"
    @echo
    @echo "==> Row counts:"
    @docker compose -f docker-compose.clickhouse.yml exec -T clickhouse clickhouse-client --query \
        "SELECT 'spans' t, count() rows FROM observability.observability_spans \
         UNION ALL SELECT 'executions', count() FROM observability.observability_executions \
         ORDER BY t FORMAT PrettyCompact" 2>/dev/null \
        || echo "  (tables not created yet — run oxy serve with .env.clickhouse first)"

# Interactive clickhouse-client shell on the observability database.
clickhouse-client:
    docker compose -f docker-compose.clickhouse.yml exec clickhouse clickhouse-client --database observability

# Smoke-test the observability rollup + analytics SQL WITHOUT an LLM/agent run:
# insert synthetic spans, confirm the observability_executions MV flattened them
# (agent_ref denormalization, is_success, error extraction), then run the
# p50/p95/p99 + histogram + cost queries. Requires the schema (tables + MV) to
# exist — create it by booting oxy once against this ClickHouse (see
# clickhouse-up). Does NOT duplicate the DDL; schema.rs stays canonical.
clickhouse-obs-verify:
    #!/usr/bin/env bash
    set -euo pipefail
    ch() { docker compose -f docker-compose.clickhouse.yml exec -T clickhouse clickhouse-client "$@"; }

    if ! ch --query "EXISTS TABLE observability.observability_executions" | grep -q 1; then
      echo "✗ observability.observability_executions not found — create the schema first:"
      echo "    set -a; source .env.clickhouse; set +a; cargo run -p oxy-server -- serve --enterprise"
      exit 1
    fi
    if ! ch --query "EXISTS TABLE observability.observability_executions_mv" | grep -q 1; then
      echo "✗ rollup MV observability_executions_mv missing (oxy boot should create it)."; exit 1
    fi

    echo "→ inserting synthetic spans (2 tool_call + 1 llm)…"
    ch --query "INSERT INTO observability.observability_spans (trace_id,span_id,span_name,span_attributes,event_data,duration_ns,status_code,timestamp) VALUES ('smoke-1','smoke-tool-ok','analytics.tool_call','{\"oxy.span_type\":\"tool_call\",\"oxy.execution_type\":\"semantic_query\",\"oxy.is_verified\":\"true\",\"oxy.agent.ref\":\"smoke_agent\",\"agent.prompt\":\"what is MRR\",\"oxy.database\":\"demo\"}','[{\"name\":\"tool_call.output\",\"attributes\":{\"status\":\"success\",\"output\":\"42\"}}]',1500000000,'OK',now64(9))"
    ch --query "INSERT INTO observability.observability_spans (trace_id,span_id,span_name,span_attributes,event_data,duration_ns,status_code,timestamp) VALUES ('smoke-1','smoke-tool-err','analytics.tool_call','{\"oxy.span_type\":\"tool_call\",\"oxy.execution_type\":\"sql_generated\",\"oxy.is_verified\":\"false\",\"oxy.agent.ref\":\"smoke_agent\",\"agent.prompt\":\"bad query\"}','[{\"name\":\"tool_call.output\",\"attributes\":{\"status\":\"error\",\"error.message\":\"boom\"}}]',420000000,'ERROR',now64(9))"
    ch --query "INSERT INTO observability.observability_spans (trace_id,span_id,span_name,span_attributes,event_data,duration_ns,status_code,timestamp) VALUES ('smoke-1','smoke-llm','llm.call','{\"oxy.span_type\":\"llm\",\"gen_ai.request.model\":\"claude-sonnet-5\"}','[{\"name\":\"llm.usage\",\"attributes\":{\"prompt_tokens\":\"1000\",\"completion_tokens\":\"500\"}}]',800000000,'OK',now64(9))"

    echo; echo "→ [1/4] rollup MV flattened the tool_call spans (expect ok→is_success=1 err='', err→is_success=0 err='boom', agent_ref=smoke_agent on both):"
    ch --query "SELECT span_id,agent_ref,execution_type,is_verified,is_success,error_message,user_question FROM observability.observability_executions WHERE trace_id='smoke-1' ORDER BY span_id FORMAT PrettyCompact"

    echo; echo "→ [2/4] latency percentiles (quantile over duration_ns):"
    ch --query "SELECT round(quantile(0.5)(duration_ns)/1e6,1) p50_ms, round(quantile(0.95)(duration_ns)/1e6,1) p95_ms, round(quantile(0.99)(duration_ns)/1e6,1) p99_ms FROM observability.observability_executions WHERE trace_id='smoke-1' FORMAT PrettyCompact"

    echo; echo "→ [3/4] latency histogram buckets:"
    ch --query "SELECT toUInt16(least(15,greatest(0,toInt32(floor(log2(greatest(duration_ns/1e6,1.0))))))) bucket, count() c FROM observability.observability_executions WHERE trace_id='smoke-1' GROUP BY bucket ORDER BY bucket FORMAT PrettyCompact"

    echo; echo "→ [4/4] cost/model aggregation over llm spans (expect claude-sonnet-5, 1000 in / 500 out):"
    ch --query "SELECT model, count() calls, sum(input_tokens) in_tok, sum(output_tokens) out_tok FROM (SELECT JSONExtractString(span_attributes,'gen_ai.request.model') model, toInt64OrZero(JSONExtractString(arrayFirst(x -> JSONExtractString(x,'name')='llm.usage', JSONExtractArrayRaw(event_data)),'attributes','prompt_tokens')) input_tokens, toInt64OrZero(JSONExtractString(arrayFirst(x -> JSONExtractString(x,'name')='llm.usage', JSONExtractArrayRaw(event_data)),'attributes','completion_tokens')) output_tokens FROM observability.observability_spans WHERE JSONExtractString(span_attributes,'oxy.span_type')='llm' AND trace_id='smoke-1') WHERE model!='' GROUP BY model FORMAT PrettyCompact"

    echo; echo "→ cleanup synthetic rows"
    ch --query "DELETE FROM observability.observability_spans WHERE trace_id='smoke-1'"
    ch --query "DELETE FROM observability.observability_executions WHERE trace_id='smoke-1'"
    echo "✓ smoke complete — if the four blocks above look right, the rollup + analytics SQL work on real ClickHouse."

# ── Routing boundary check ─────────────────────────────────────────────────────

# Probe a running node and show how each route ROLE is handled, by dumping the
# x-oxy-* headers (enforce_role stamps them before the handler, so even an
# unauth 401/421 reveals the routing). Run against:
#   the IDE node   (OXY_ROLE=ide,   :3000)  → everything Served-By: ide
#   the SERVE node (OXY_ROLE=serve, :3002)  → FleetOk Served-By: serve, and on
#       IdeOnly routes: Forwarded-Via: serve + Served-By: ide  (OXY_IDE_UPSTREAM set)
#       or 421 + Required-Role: ide                            (OXY_IDE_UPSTREAM unset)
# Default ws is the deterministic `oxy seed` demo workspace; any uuid works
# (classification is path-based, so the headers show even on a 404).
routing-check port="3000" ws="70787bb2-e11b-5488-b2c3-02e60d5fc7d3":
    #!/usr/bin/env bash
    set -eu
    base="http://localhost:{{ port }}"
    tally=$(mktemp)
    probe() {  # METHOD PATH
      local code by fwd
      code=$(curl -s -o /dev/null -w '%{http_code}' -D /tmp/_rch --max-time 5 -X "$1" "$base$2" || echo "---")
      by=$(grep -i '^x-oxy-served-by:' /tmp/_rch | sed 's/.*: *//; s/@.*//; s/\r//')
      grep -iq '^x-oxy-forwarded-via:' /tmp/_rch && fwd="  (serve→ide)" || fwd=""
      printf '  %-4s %-42s %-5s %s%s\n' "$1" "$2" "$code" "${by:-?}" "$fwd"
      echo "${by:-?}" >> "$tally"
    }
    echo "Routing check → $base  (ws={{ ws }})"
    echo "  401/200 is fine — x-oxy-served-by is stamped before auth; we're checking ROUTING."
    echo
    echo "FleetOk — no filesystem → the stateless serve fleet answers directly:"
    probe GET  /api/health
    probe GET  /api/{{ ws }}/threads
    probe GET  /api/{{ ws }}/apps
    probe GET  /api/{{ ws }}/agents
    probe GET  /api/{{ ws }}/databases
    probe GET  /api/{{ ws }}/tests
    probe GET  /api/{{ ws }}/traces
    probe GET  /api/{{ ws }}/semantic/monitors
    probe GET  /api/{{ ws }}/analytics/runs/r1/events
    probe GET  /api/{{ ws }}/blocks
    probe GET  /api/{{ ws }}/world-model/cameras
    probe GET  /api/{{ ws }}/results/files/abc.parquet
    # The IDE page is only the SPA shell — the same embedded index.html on every
    # pod. Its ACTIONS are IdeOnly and proxy individually; the page itself is not.
    probe GET  /ide
    echo
    echo "IdeOnly — genuinely needs the working copy / .git / node-local state → ide:"
    probe GET  /api/{{ ws }}/files
    probe POST /api/{{ ws }}/compile
    probe GET  /api/{{ ws }}/branches
    probe GET  /api/{{ ws }}/details
    probe GET  /api/{{ ws }}/status
    probe GET  /api/{{ ws }}/events
    probe GET  /api/{{ ws }}/world-model/events
    probe GET  /api/{{ ws }}/charts/x.json
    echo
    echo "── tally (this sample) ──  served-by serve: $(grep -c '^serve$' "$tally" || true)   ide: $(grep -c '^ide$' "$tally" || true)"
    echo "  (the FleetOk set is the BULK of the real API — apps/agents/semantic/analytics/orgs/users/billing/… all serve)"
    rm -f "$tally" /tmp/_rch

# Both halves of the suite in one command: every declared route across a real
# three-node fleet, then all 35 browser flows with the backings and identities
# each group needs. Handles the session plumbing, the cloud ordering, and the
# ClickHouse seed itself — getting any of those wrong fails as a locator
# timeout that reads exactly like a product bug.
#   just verify              # everything, against what is already built
#   just verify --build      # rebuild the fleet image first
#   just verify --group a    # HTTP only    /  --group b  # flows only
#   just verify --keep       # leave the stacks up
#   just verify --expensive  # add ide-pipeline-quickbooks (~$3.20/run on a cold cache)
#   just verify --dry-run    # print every flow invocation, run none of them
verify *FLAGS:
    ./scripts/verify-all.sh {{ FLAGS }}

# Assert the split-fleet CONTRACT over HTTP — no browser, no LLM, CI-safe.
# `routing-check` above prints a table for a human to read; this decides.
# Brings up its own postgres + ide + two serve replicas, seeds and promotes a
# revision, then asserts: FleetOk routes are answered by the replica itself,
# IdeOnly routes are proxied to the ide, both replicas agree, and killing the
# ide leaves persisted reads at 200 while IdeOnly fails loudly. Exits non-zero
# on any violation. `--keep` leaves the fleet up.
#   just fleet-assert --build          after a code change — build, then assert
#   just fleet-assert docker --build   same, against the compose fleet whose
#                                      replicas genuinely have no working copy
#   just fleet-assert                  assert whatever is already built
#   just fleet-assert --keep           leave the fleet running to poke at
fleet-assert *FLAGS:
    ./scripts/fleet-assert.sh {{ FLAGS }}
# ── Per-org OLTP POC ──────────────────────────────────────────────────────────
# Docs: scripts/oltp/README.md · Design:
# internal-docs/2026-08-04-per-org-oltp-postgres-design.md

# ROLE is one of: analyst (read-only) | app | pipeline | owner.
# Open psql against the demo database as one of the provisioned roles.
oltp-psql ROLE="analyst":
    #!/usr/bin/env bash
    set -euo pipefail
    ctl="postgresql://postgres:postgres@localhost:15432/oxy"
    db="$(psql "$ctl" -tAc "SELECT database_name FROM oltp_tenants LIMIT 1")"
    case "{{ ROLE }}" in
      analyst)  dsn="$(./target/debug/oxy oltp dsn --org ${OLTP_ORG:-luong@oxy.tech} --role analyst)" ;;
      app)      dsn="$(./target/debug/oxy oltp dsn --org ${OLTP_ORG:-luong@oxy.tech} --role app:bookings)" ;;
      pipeline) dsn="$(./target/debug/oxy oltp dsn --org ${OLTP_ORG:-luong@oxy.tech} --role pipeline:toast)" ;;
      owner)    dsn="$(./target/debug/oxy oltp dsn --org ${OLTP_ORG:-luong@oxy.tech} --role owner)" ;;
      *) echo "unknown role '{{ ROLE }}' — try: analyst | app | pipeline | owner" >&2; exit 1 ;;
    esac
    psql "$dsn"

# Deliberately NOT `oxy start`'s :15432: that cluster holds the control plane
# and, since `oxy start` defaults per-org OLTP to local, tenant databases too —
# and the suite refuses any cluster with tenants. Its own container, --rm, on a
# port nothing else claims.
#
# Disposable Postgres on :15433 for the OLTP integration suite.
oltp-test-db:
    #!/usr/bin/env bash
    set -euo pipefail
    docker rm -f oxy-oltp-test >/dev/null 2>&1 || true
    docker run -d --rm --name oxy-oltp-test \
      -e POSTGRES_PASSWORD=postgres -p 15433:5432 postgres:18-alpine >/dev/null
    for _ in $(seq 1 40); do
      docker exec oxy-oltp-test pg_isready -U postgres >/dev/null 2>&1 && break
      sleep 1
    done
    if ! docker exec oxy-oltp-test pg_isready -U postgres >/dev/null 2>&1; then
      echo "oxy-oltp-test did not become ready on :15433" >&2
      exit 1
    fi
    echo "oxy-oltp-test up on :15433 — run: cargo nextest run -p oxy-oltp --test integration"

# In-place per-object, NOT a volume wipe: the demo uses the shared dev Postgres
# (`oxy start`'s :15432), so this must not touch the volume — `oxy start --clean`
# would take workspaces, threads and apps with it. This is the dev cluster, not
# the test one: `just oltp-test-db` on :15433 is throwaway and needs no cleanup
# (its container is `--rm`).
#
# Keeps `neon` rows on purpose. Deleting one drops the row and its sealed
# owner/analyst passwords while the billed Neon project keeps running — an
# orphaned project nobody can reach; delete those in the Neon console. It also
# drops the cluster-global roles the local tenants used, which are LOGIN roles
# whose passwords this discards — re-provisioning re-mints them.
#
# Clear LOCAL OLTP state on the shared dev cluster (keeps neon rows; volume untouched).
oltp-down:
    #!/usr/bin/env bash
    set -euo pipefail
    # The LEDGER (oltp_tenants/oltp_roles) lives on the control plane; the tenant
    # DATABASES and their cluster-global roles live on OXY_OLTP_ADMIN_URL, which
    # `oxy start`'s three-state gate keeps distinct when a developer set it. When
    # unset they are the same cluster. Conflating them deleted the ledger rows —
    # sealed passwords and all — while the databases and roles sat untouched on
    # the other cluster.
    ctl="${OXY_DATABASE_URL:-postgresql://postgres:postgres@localhost:15432/oxy}"
    admin="${OXY_OLTP_ADMIN_URL:-$ctl}"
    # host:port of the tenant cluster, the same string `host_from_dsn` records in
    # tenant.host: strip scheme, drop any userinfo, take up to the first '/'.
    tmp="${admin#*://}"; tmp="${tmp##*@}"; tmp="${tmp%%/*}"; admin_host="${tmp%%\?*}"

    if [ "$(psql "$ctl" -tAc "SELECT to_regclass('public.oltp_tenants') IS NOT NULL")" != "t" ]; then
      echo "no OLTP state on this cluster (oltp_tenants absent) — nothing to clear"
      exit 0
    fi

    neon="$(psql "$ctl" -tAc "SELECT count(*) FROM oltp_tenants WHERE provider = 'neon'")"
    if [ "$neon" -gt 0 ]; then
      echo "keeping $neon neon tenant row(s) — delete those projects in the Neon console"
    fi
    # Tenants provisioned on a DIFFERENT cluster than the one we can reach: their
    # databases are not here, so clearing their rows would orphan those. Skip.
    elsewhere="$(psql "$ctl" -tAc "SELECT count(*) FROM oltp_tenants WHERE provider <> 'neon' AND host <> '$admin_host'")"
    if [ "$elsewhere" -gt 0 ]; then
      echo "keeping $elsewhere tenant(s) on other clusters (not $admin_host) — run with OXY_OLTP_ADMIN_URL pointed there"
    fi

    # Writer + owner names from the LEDGER (control plane), for tenants on this
    # admin cluster only. Collected BEFORE the delete cascades oltp_roles away.
    roles="$(psql "$ctl" -tAc "
      SELECT r.role_name FROM oltp_roles r JOIN oltp_tenants t ON t.id = r.tenant_row_id
        WHERE t.provider <> 'neon' AND t.host = '$admin_host'
      UNION SELECT owner_role FROM oltp_tenants
        WHERE provider <> 'neon' AND host = '$admin_host'")"
    # Analyst roles are catalog-based (hashed tag, not in the ledger) and live on
    # the ADMIN cluster — query THERE, not $ctl. On $ctl (the control plane) a
    # split setup holds no `oxy_analyst_ro*` roles, so they would have survived
    # the sweep with the passwords the DELETE below discards.
    #
    # Catalog-wide on $admin, unlike the host-scoped arms above: every
    # `oxy_analyst_ro*` on this cluster is dropped even if all its tenants were
    # skipped as "elsewhere". Correct — by then those are orphans on THIS
    # cluster with passwords no ledger row still holds.
    analysts="$(psql "$admin" -tAc "SELECT rolname FROM pg_roles WHERE rolname LIKE 'oxy\_analyst\_ro%'")"

    psql "$ctl" -qc "DELETE FROM oltp_tenants WHERE provider <> 'neon' AND host = '$admin_host';"

    # Databases + roles on the ADMIN cluster (where they live), not $ctl.
    # Databases first: a role owning objects cannot be dropped, and writers own
    # the tables inside these.
    for db in $(psql "$admin" -tAc "SELECT datname FROM pg_database WHERE datname LIKE 'oxy\_org\_%'"); do
      psql "$admin" -qc "DROP DATABASE IF EXISTS \"$db\" WITH (FORCE)" || true
      echo "dropped database $db"
    done

    for role in $roles $analysts; do
      if psql "$admin" -qc "DROP ROLE IF EXISTS \"$role\"" >/dev/null 2>&1; then
        echo "dropped role $role"
      else
        echo "kept role $role (still owns objects, or not on $admin_host)"
      fi
    done

    echo "local OLTP state on $admin_host cleared. Neon projects are untouched — delete those in the console."

oltp-status:
    @docker ps --filter name=oxy- --format "table {{{{.Names}}	{{{{.Status}}	{{{{.Ports}}"

# Free the ports and stop the containers the demo started. Destroys nothing.
#
# `oxy start --enterprise` plus `pnpm dev` is the normal local stack, and a
# Ctrl-C that races the shutdown, a closed terminal or a `kill -9` leaves the
# API holding :3000 and the whole turbo/vite tree holding :5173. The next run
# then fails on a bound port, which reads as a broken build rather than as
# leftovers.
#
# Ports are freed by asking WHO IS LISTENING, not by pattern-matching process
# names: `pkill -f oxy` on a machine that also runs the real thing is how you
# kill someone's actual work.
#
# Listeners are identified before they are killed: a process whose argv does not
# point at this checkout is reported and left running, and `FORCE=1` overrides.
#
# **Postgres is stopped through Docker, never through its port.** :15432 is
# bound by the Docker daemon's proxy — on OrbStack that PID *is* OrbStack, so an
# lsof-and-kill there takes down every container on the machine.
#
# Free :3000 and :5173 and stop the oxy containers. Deletes nothing.
oltp-stop:
    #!/usr/bin/env bash
    set -uo pipefail

    # Only kill what looks like THIS repo's dev stack, unless told otherwise.
    #
    # The header above rejects `pkill -f oxy` for killing someone's real work,
    # and "whatever is listening on :3000" has the same exposure with a wider
    # blast radius — :3000 is the most-squatted dev port on any machine that
    # also runs Node, and the kill escalates to the whole process group. So each
    # listener is identified first, and anything whose argv does not point at
    # this checkout is left alone with its command line printed.
    owns_port() {
      local argv="$1"
      [[ "$argv" == *"$PWD"* ]] || [[ "$argv" == *oxy-web* ]] || [[ "$argv" == *"target/debug/oxy"* ]]
    }

    free_port() {
      local port="$1" label="$2" pids pgid argv
      pids="$(lsof -ti "tcp:$port" -sTCP:LISTEN 2>/dev/null || true)"
      if [[ -z "$pids" ]]; then echo "  :$port already free ($label)"; return; fi
      if [[ "${FORCE:-0}" != "1" ]]; then
        for pid in $pids; do
          argv="$(ps -o args= -p "$pid" 2>/dev/null || true)"
          if ! owns_port "$argv"; then
            echo "  :$port SKIPPED — not this repo's:" >&2
            echo "      $pid  ${argv:0:120}" >&2
            echo "      re-run with FORCE=1 to kill it anyway" >&2
            return
          fi
        done
      fi
      for pid in $pids; do
        # The whole process group: `pnpm run dev` is turbo → vite → esbuild, and
        # killing only the listener leaves the parents to respawn or linger.
        pgid="$(ps -o pgid= -p "$pid" 2>/dev/null | tr -d ' ')"
        if [[ -n "$pgid" ]]; then kill -TERM -"$pgid" 2>/dev/null || true; fi
        kill -TERM "$pid" 2>/dev/null || true
      done
      for _ in $(seq 1 20); do
        lsof -ti "tcp:$port" -sTCP:LISTEN >/dev/null 2>&1 || break
        sleep 0.25
      done
      pids="$(lsof -ti "tcp:$port" -sTCP:LISTEN 2>/dev/null || true)"
      if [[ -n "$pids" ]]; then
        # SIGTERM was ignored or the handler hung; nothing here owns unflushed
        # state worth waiting longer for.
        for pid in $pids; do kill -9 "$pid" 2>/dev/null || true; done
        sleep 0.5
      fi
      if lsof -ti "tcp:$port" -sTCP:LISTEN >/dev/null 2>&1; then
        echo "  :$port STILL BOUND — inspect with: lsof -nP -iTCP:$port -sTCP:LISTEN" >&2
      else
        echo "  :$port freed ($label)"
      fi
    }

    echo "Stopping the Oxy demo…"

    # Snapshot the containers BEFORE freeing the ports.
    #
    # `oxy start` owns its Postgres container through bollard and stops it on
    # shutdown, so killing the API usually takes the container with it — and a
    # `docker ps` afterwards then finds nothing and reports "no containers",
    # which reads as "there was nothing to clean" rather than "already handled".
    # Taking the list first lets the summary say what actually happened.
    # Four opening braces, TWO closing, in every docker --format below. just
    # escapes a doubled opening brace but passes a doubled closing one straight
    # through, so the symmetric-looking spelling reaches docker with two extra
    # characters and prints the value with a stray brace pair glued on. That had
    # silently broken oltp-status's table, and here it corrupts the very name
    # this recipe then looks for.
    # Excludes oxy-oltp-test: it is `--rm`, so `docker stop` would REMOVE it, and
    # this recipe's whole promise is "stopped, not deleted". The test cluster is
    # `just oltp-test-db`'s to manage.
    before="$(docker ps --format '{{{{.Names}}' --filter 'name=oxy-' 2>/dev/null | grep -v '^oxy-oltp-test$' || true)"

    # Honour the same env var `oxy start` reads, so a non-default port is freed
    # rather than silently skipped.
    free_port "${OXY_HTTP_PORT:-3000}" "oxy api"
    free_port 5173 "vite dev server"

    if [[ -z "$before" ]]; then
      echo "  no oxy-* containers were running"
    else
      for name in $before; do
        if docker ps -q --filter "name=^${name}$" | grep -q .; then
          # `stop`, not `rm`: the container and its volume both survive, so the
          # next `oxy start` comes back in seconds with every workspace, thread
          # and app still there.
          docker stop "$name" >/dev/null 2>&1 && echo "  stopped $name"
        else
          echo "  stopped $name (with the api that owns it)"
        fi
      done
    fi

    echo
    # Scoped to what THIS recipe did: `oltp-clean` runs `oltp-down` first, so a
    # flat "nothing was deleted" would be a lie about the run the reader just
    # watched drop a tenant database.
    echo "This deleted nothing — the Docker volume is untouched."
    echo "  restart:      oxy start --enterprise  (+ pnpm dev)"
    echo "  also drop OLTP state:  just oltp-down     (tenant rows, roles, oxy_org_* databases)"
    echo "  both at once:          just oltp-clean"

# Stop everything AND clear the OLTP state — the full reset.
#
# Still leaves the shared dev database's own contents alone: `oltp-down` drops
# the tenant databases this POC created, not the workspaces and threads beside
# them, and a Neon project is a real billed resource that only the console
# deletes.
#
# Order matters: `oltp-down` talks to Postgres, so it has to run BEFORE
# `oltp-stop` takes the container down. The other way round fails on a refused
# connection, having already stopped everything — the worst of both.
#
# Stop everything and clear the OLTP state — the full reset.
oltp-clean: oltp-down oltp-stop
