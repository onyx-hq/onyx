#!/usr/bin/env bash
# Which custom apps does this repo hold, and where does each one's build land?
#
# Split out of publish.yaml on purpose. GitHub Actions cannot be executed
# locally, so anything left inline in a `run:` block is unexecuted code until
# the first real push. This file is the part that CAN be run here, and it is
# run — the same bytes, not a transcription of them.
#
# Contract:
#   stdout  one line per app, "<app-dir><TAB><out-dir>", relative to the repo
#           root, sorted. Empty when the repo has no apps.
#   exit 0  including for zero apps. A freshly scaffolded repo has none, and
#           its CI must be green on day one — a template that ships a red
#           check teaches the team to stop reading checks.
#   exit 1  only for a repo that HAS apps and cannot publish them: a manifest
#           that will not parse, an app with no resolvable identity, or a
#           missing lockfile.
#   $GITHUB_OUTPUT gains `count=<n>` when that variable is set, so the
#           workflow's skip condition and this script's answer are one value
#           rather than two that can disagree.

set -euo pipefail

root="${1:-.}"

emit_count() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    printf 'count=%s\n' "$1" >> "$GITHUB_OUTPUT"
  fi
}

# `find` prints nothing and exits 0 when nothing matches, which is exactly
# what a day-one repo produces. That is why discovery is a `find`, not a glob:
# `for d in apps/*/` under `set -e` yields the literal string `apps/*/`, and a
# legitimately-empty match then reads as either a phantom app or a hard
# failure depending on what runs next. This project has been bitten by that
# twice.
#
# -maxdepth 3 covers both layouts pnpm-workspace.yaml declares:
#   apps/<app>/oxy-app.json         (depth 2)
#   apps/<org>/<app>/oxy-app.json   (depth 3)
# The prunes keep a dependency's own manifest, or a copy left inside a
# previous build output, from being published as if it were one of ours.
manifests=""
unpruned=""
if [ -d "$root/apps" ]; then
  manifests="$(cd "$root" && find apps -maxdepth 3 \
    \( -name node_modules -o -name out -o -name dist -o -name build \
       -o -name .git -o -name .turbo \) -prune \
    -o -type f -name oxy-app.json -print | LC_ALL=C sort)"
  unpruned="$(cd "$root" && find apps -maxdepth 3 \
    -type f -name oxy-app.json -print | LC_ALL=C sort)"
fi

# The prune above is right, but it is also SILENT, and silence is the wrong
# half of it: an app whose directory is legitimately named `dist`, `out` or
# `build` is dropped and the workflow then succeeds having published nothing.
#
# Warn about exactly that case and no other. A manifest under a pruned
# directory that sits INSIDE a directory which is itself an app —
# `apps/dashboard/out/oxy-app.json`, `apps/dashboard/node_modules/…` — is a
# build artifact or a dependency's own manifest, correctly ignored, and
# warning about it would be the noise that gets warnings ignored.
if [ -n "$unpruned" ]; then
  while IFS= read -r candidate; do
    [ -n "$candidate" ] || continue
    if printf '%s\n' "$manifests" | grep -qxF "$candidate"; then
      continue
    fi
    skipped_dir="${candidate%/oxy-app.json}"
    parent="${skipped_dir%/*}"
    # Nested inside a real app: intended, stays quiet.
    if printf '%s\n' "$manifests" | grep -qxF "$parent/oxy-app.json"; then
      continue
    fi
    printf '::warning file=%s::%s was skipped: a directory named "%s" is treated as build output, not an app. Rename it if this really is an app — nothing here will be published.\n' \
      "$candidate" "$skipped_dir" "${skipped_dir##*/}" >&2
  done <<EOF
$unpruned
EOF
fi

if [ -z "$manifests" ]; then
  printf 'publish: no custom apps under apps/ — nothing to build, nothing to publish.\n' >&2
  emit_count 0
  exit 0
fi

if ! command -v jq >/dev/null 2>&1; then
  printf '::error::jq is not on this runner, so oxy-app.json cannot be read. GitHub'"'"'s ubuntu-latest image ships it; a self-hosted runner may not.\n' >&2
  exit 1
fi

buffer="$(mktemp)"
trap 'rm -f "$buffer"' EXIT

bad=0
count=0

while IFS= read -r manifest; do
  app_dir="${manifest%/oxy-app.json}"

  if ! jq -e 'type == "object"' "$root/$manifest" >/dev/null 2>&1; then
    printf '::error file=%s::%s is not a JSON object. oxyc publish reads this file for the app identity and will not get past it.\n' \
      "$manifest" "$manifest" >&2
    bad=1
    continue
  fi

  # `oxyc publish` resolves the app slug as `--app`, then OXY_APP, then the
  # manifest, then the `<app>` segment of an `apps/<org>/<app>/` path — its
  # `--help` says the same. This workflow sets neither the flag nor the env
  # var, so `manifest, then path` are the only two links this check depends on.
  slug="$(jq -r '.slug // ""' "$root/$manifest")"
  if [ -z "$slug" ] || [ "$slug" = "null" ]; then
    printf '::error file=%s::%s has no "slug". That is the app'"'"'s identity on the platform and there is nowhere else to get it.\n' \
      "$manifest" "$manifest" >&2
    bad=1
    continue
  fi

  # Same resolution chain for the org, with one
  # difference that matters: the path fallback only fires for the nested
  # `apps/<org>/<app>/` layout. In the flat `apps/<app>/` layout there is no
  # `<org>` segment, so a manifest with no `orgSlug` leaves the org
  # unresolvable — and `oxyc publish` needs it to look the project up on the
  # build-config endpoint.
  org="$(jq -r '.orgSlug // ""' "$root/$manifest")"
  nested=0
  case "$app_dir" in
    apps/*/*) nested=1 ;;
  esac
  if { [ -z "$org" ] || [ "$org" = "null" ]; } && [ "$nested" -eq 0 ]; then
    printf '::error file=%s::%s has no "orgSlug", and %s is not an apps/<org>/<app>/ path for oxyc publish to infer one from. Add "orgSlug" to the manifest.\n' \
      "$manifest" "$manifest" "$app_dir" >&2
    bad=1
    continue
  fi

  # `build.outDir`, defaulting to `out` — the same default `oxyc publish`
  # applies. Guarded with a type check because a manifest whose `build` is a
  # string (or a number) would make plain `.build.outDir` a jq error, and a jq
  # error under `set -e` would abort the whole scan on one malformed file
  # instead of reporting it.
  out="$(jq -r 'if (.build | type) == "object" then (.build.outDir // "out") else "out" end' "$root/$manifest")"
  if [ -z "$out" ] || [ "$out" = "null" ]; then
    out="out"
  fi

  printf '%s\t%s\n' "$app_dir" "$out" >> "$buffer"
  count=$((count + 1))
done <<EOF
$manifests
EOF

if [ "$bad" -ne 0 ]; then
  exit 1
fi

# Reached only when the repo has at least one app, which is the only state in
# which a lockfile is required. `pnpm install --frozen-lockfile` fails without
# one, and it fails with a message about frozen lockfiles rather than about
# the thing to do, so say the thing to do.
if [ ! -f "$root/pnpm-lock.yaml" ]; then
  printf '::error::this repo has %s custom app(s) but no pnpm-lock.yaml. "pnpm install --frozen-lockfile" cannot run without one: run "pnpm install" locally and commit the lockfile it writes.\n' \
    "$count" >&2
  exit 1
fi

emit_count "$count"
cat "$buffer"
