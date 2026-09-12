#!/usr/bin/env bash
# Which oxy environment does this run publish to?
#
# One line of logic, in its own file for the same reason discover-apps.sh is:
# a `run:` block cannot be executed anywhere but a runner, so anything left
# inline is unexecuted code until the first real push. This is the mapping the
# whole dev/prod split rests on, so it is the last thing that should be
# unexecuted.
#
# It also has exactly one caller and one answer ON PURPOSE. The name printed
# here is used TWICE by publish.yaml — as `oxyc publish --env <name>` and as the
# GitHub Actions `environment:` whose secrets that publish authenticates with —
# and those two must never disagree. Two ternaries in the YAML would have been
# shorter and would have drifted the first time one of them was edited; one
# value, resolved once and read twice, cannot.
#
# Contract:
#   $1        the trigger, defaulting to $GITHUB_EVENT_NAME. Passed explicitly
#             only by the tests.
#   stdout    the environment name on one line: `dev` or `production`.
#   exit 0    only for a trigger this repo has a mapping for.
#   exit 1    for anything else, INCLUDING an empty trigger.
#
# The refusal is the point. `oxyc publish` defaults `--env` to **production**,
# so a workflow that omits the flag ships a customer's live environment while
# looking like it did nothing in particular — which is exactly what this repo's
# CI did before this file existed. Every publish now names its environment out
# loud, and a trigger nobody has thought about fails here rather than falling
# through to the most dangerous of the two.

set -euo pipefail

trigger="${1:-${GITHUB_EVENT_NAME:-}}"

case "$trigger" in
  # A merge to main. Draft, to the dev workspace: main moving is not a decision
  # to ship anything to a customer.
  push)
    printf 'dev\n'
    ;;
  # Someone stood at the Actions tab, picked a ref and pressed the button. That
  # is the deliberate act, so that is the one that reaches production.
  workflow_dispatch)
    printf 'production\n'
    ;;
  '')
    printf '::error::no trigger to map to an oxy environment: GITHUB_EVENT_NAME is empty and no argument was given.\n' >&2
    exit 1
    ;;
  *)
    printf '::error::this repo publishes on "push" and "workflow_dispatch" only, and has no oxy environment mapped for "%s". Add one to .github/scripts/publish-env.sh — deliberately, because the alternative is a trigger nobody reviewed publishing to production, which is what "oxyc publish" does when nothing names an environment.\n' \
      "$trigger" >&2
    exit 1
    ;;
esac
