#!/usr/bin/env bash
# Does this repo publish its custom apps from CI, or does a human do it?
#
# OFF by default, and the default is the whole point. `customer-apps` — the
# monorepo Oxy ships its own custom apps from — deleted its publish CI outright
# and its engineers publish from their machines with `oxyc login` and
# `oxyc publish`; its own docs say "there is no GitHub Actions / CI". A repo
# scaffolded from this template therefore behaves the same way unless somebody
# asks for CI publishing, rather than arriving demanding a long-lived token the
# rest of the org has already decided it does not want.
#
# In its own file for the same reason publish-env.sh is: a `run:` block cannot
# be executed anywhere but a runner, so anything left inline is unexecuted code
# until the first real push. This is the switch the whole opt-in rests on, so it
# is the last thing that should be unexecuted.
#
# THE SWITCH IS EXPLICIT, AND IT IS NOT THE TOKEN. Skipping the publish whenever
# no OXY_TOKEN reached the job would have been one line, and it would have
# destroyed the property publish.yaml is built around: that a publish into a
# misconfigured environment is a loud credential failure rather than a quiet
# success. Inferred from the token, "we do not publish from CI" and "somebody
# forgot production's OXY_TOKEN" are the same observation — and the second one
# then ships nothing, silently, for as long as nobody looks. Two causes need two
# states, which is what this file is.
#
# A VARIABLE rather than a secret, for two reasons that both matter. It is read
# in the `build` job — the only job that runs when publishing is off, and so the
# only one that can say anything about it — and that job must reference no
# `secrets.` at all: it executes the bundles' own postinstall scripts, and the
# credential boundary in publish.yaml is enforced by a test that fails on any
# secret reference there. The second reason is plainer: a switch whose entire
# job is to tell "off on purpose" apart from "misconfigured" has to be legible,
# in Settings and in a run log. A secret is neither.
#
# Contract:
#   $1        the raw value, defaulting to $OXY_CI_PUBLISH — which publish.yaml
#             fills from `vars.OXY_CI_PUBLISH`. Passed explicitly only by the
#             tests.
#   stdout    `on` or `off`, one line. Nothing else, ever: the workflow captures
#             stdout into a job output that gates the publish job.
#   stderr    when off, the ::notice:: that names the self-serve command and
#             says how to turn CI publishing on.
#   exit 0    for a value this file recognises, an absent one included.
#   exit 1    for a value it does not.
#
# That last refusal is deliberate. `OXY_CI_PUBLISH=ture` cannot be read as
# either answer, and both guesses are wrong in a way nobody would see: read as
# off, the repo quietly stops publishing; read as on, it publishes when nobody
# asked. It fails in the `build` job, which holds no credential, before a single
# dependency is installed.

set -euo pipefail

raw="${1:-${OXY_CI_PUBLISH:-}}"

# Case-folded through `tr` because bash 3.2 — which is what macOS ships and what
# this repo's suite runs on — has no ${var,,}.
value="$(printf '%s' "$raw" | tr '[:upper:]' '[:lower:]')"

case "$value" in
  # Absent is the default and the default is off. The explicit negatives are
  # listed beside it for the person who writes `false` meaning it: a rule of
  # "any non-empty value is on" would read that as a publish, which is the one
  # misreading of a switch that costs something.
  ''|false|no|off|0)
    printf 'off\n'
    printf '::notice::CI publishing is off in this repo, which is the default. This run built the app bundles and published nothing. Publishing here is self-serve: from an app directory under apps/, run "oxyc login --env dev" once and then "oxyc publish --env dev" (or --env production). To publish from CI instead, add a repository VARIABLE named OXY_CI_PUBLISH with the value true — Settings, then Secrets and variables, then Actions, then the Variables tab — and give each GitHub environment its own OXY_TOKEN.\n' >&2
    ;;
  true|yes|on|1)
    printf 'on\n'
    ;;
  *)
    printf '::error::OXY_CI_PUBLISH is set to "%s", which is neither on (true, yes, on, 1) nor off (false, no, off, 0, or the variable removed). Refusing to guess, because both guesses are silent: read as off this repo would stop publishing with nobody told, and read as on it would publish when nobody asked. Fix the value or delete the variable.\n' \
      "$raw" >&2
    exit 1
    ;;
esac
