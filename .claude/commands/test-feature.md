---
name: test-feature
description: Generate an agentic browser test (.flow.test.yml) from a free-form description, skipping Q&A
activeForm: Generating agentic browser test from description
argument-hint: "<description of feature to test>"
allowed-tools:
  - Bash
  - Read
  - Write
  - Edit
  - Grep
  - Glob
  - AskUserQuestion
---

# Generate an agentic browser test from a description

One-shot generator for `.flow.test.yml` files. Skips Q&A, takes the
description as `$ARGUMENTS`, infers the answers, writes the YAML, runs
the self-checks (parse + schema + `--dry-run` lint), adds a
`_budgets.yml` entry, and tells the dev which CI bucket the flow lands
in. For vague requests, the `agentic-browser-test` skill prompts a short
Q&A instead.

This command must be run from the root of an `oxy-hq/oxygen-internal`
checkout. Source-of-truth files live on `main` (the agentic browser
test suite landed in PR #2275).

## Steps

### 1. Sanity-check the working directory

```bash
test -f web-app/tests/agentic/README.md && test -f json-schemas/flow-test.json
```

If either path is missing, exit and tell the dev to checkout `main` of
`oxy-hq/oxygen-internal` (the agentic test suite shipped in PR #2275).

### 2. Re-read source of truth

Always re-read these — they evolve:

- `web-app/tests/agentic/README.md` — runner mechanics, primitives, fixtures, expect kinds.
- `json-schemas/flow-test.json` — schema. `target:` enum, settings keys, defaults.
- `web-app/tests/agentic/canonical-prompts.md` — verbatim copy-pasteable step text for `cache_scope: shared`.
- `web-app/tests/agentic/flows/_budgets.yml` — existing cost ceilings.
- The most recent flow file in `web-app/tests/agentic/flows/` matching the surface — the team's current taste.
- `internal-docs/agentic-browser-testing-spec.md` for dated change-log entries and the incident retrospective.

### 3. Infer the answers

From `$ARGUMENTS`, infer:

| Field | How to infer |
|---|---|
| `target:` | Match the surface phrase to the schema's enum (chat / ide / threads / onboarding / any). When ambiguous, default to `any`. |
| Filename | Lower-kebab descriptive name **prefixed with the bucket the flow will land in** (`builder-…`, `chat-…`, `threads-…`, `ide-…`, `metric-tree-…`). Don't shadow an existing flow filename. |
| `backend_mode:` | `local` unless the flow needs the multi-tenant (org → workspace) shape, e.g. an `/admin/*` surface. Cloud means the runner spawns `oxy start --enterprise --clean` on port 3001 — an empty DB with no org, and no UI creates one, so a flow that needs org data runs against a backend `oxy seed`ed beforehand (`scripts/verify-all.sh` phase 4). Mixed-mode invocations error loudly. |
| `setup:` | Build from the **3 documented commands** in `fixtures/reset.ts:SetupCommand` only — `reset_test_file` (wipes `demo_project/test.sql`), `restore_demo_file:<rel>` (reverts a demo file via `git show HEAD:…` — used by builder flows), `goto:/<path>`. None of these can make a network call; flows never seed their own data. |
| `cache_actions:` | `true`. Egress-substitution makes `false` no longer required for secret correctness — flip only for operational reasons (force-cold benchmark). |
| `max_steps:` | 15–25 for typical flows. 30 default. Bump only if the flow has long pipelines (builder runs, agentic onboarding). |
| Step text | Convert the dev's verbal user actions into one `act:` per logical action, each followed by a `wait_for:` that names the gate proving it worked. **Bias hard toward explicit `[data-testid=…]` selectors** — grep `web-app/src/**/*.tsx` for the testids referenced by the dev's description before authoring. |
| `cache_scope:` per step | `flow` (default). Opt into `shared` **only** when the step text is byte-identical to a snippet from `canonical-prompts.md`. |
| `expect:` | One `judge:` for the dev's stated success criterion. Zero or more `assert:` for any structural claim the description names ("a SQL artifact appears", "the save button hides", "the URL changes to /threads/<id>"). |

### 4. Grep for testids

Before writing any `act:` step that mentions a button / input / visible
component:

```bash
grep -rnE 'data-testid="[^"]+"' web-app/src/ \
  | grep -i '<keyword from the dev's description>'
```

Use the matched testid verbatim in the `act:` text. If none exists for a
component the test needs, tell the dev that adding one is a one-line
follow-up that drops cold cost ~5× — the runtime's multi-strategy
fallback layer still records `role+name` and `text` strategies, but a
testid primary is the most durable.

### 5. Consider `--scaffold` for component-driven authoring

When the dev names a specific component file rather than a list of
user actions, prefer scaffolding:

```bash
cd web-app && pnpm test:agentic --scaffold <feature-name> --from <component-path>
```

`runner/scaffold.ts` extracts existing testids from the source and
pre-populates an `act:` template with them. Less authoring work + better
default selectors. Then iterate.

### 6. Write the YAML

```bash
mkdir -p web-app/tests/agentic/flows
```

Write the file at `web-app/tests/agentic/flows/<name>.flow.test.yml`.
Copy the `# yaml-language-server: $schema=` header from an existing flow.
Match the existing flows' two-space indent and quoting style.

Drop overcautious disambiguators from the prompts — the 2026-05-11
tightening pass measured that rationale paragraphs explaining the verb
choice ("the verb makes the target unambiguous to the agent so it
doesn't have to infer from context") don't pay rent when the testid is
correct. Rationale belongs in YAML comments above the step.

### 7. Add a `_budgets.yml` entry

Append a new entry to `web-app/tests/agentic/flows/_budgets.yml` using
the defaults:

| Shape | `cold_usd` | `warm_usd` |
|---|---|---|
| Simple (1–3 act steps, no builder/onboarding) | 0.30 | 0.005 |
| Builder / multi-step | 0.50 | 0.01 |
| Cloud-mode onboarding-class | 1.00 | 0.01 |

The reporter writes `⚠️` to the markdown summary if observed cost exceeds
the ceiling. Advisory only — never fails CI.

### 8. Self-checks

Don't hand off broken YAML.

**Parse-check:**

```bash
cd web-app
ANTHROPIC_API_KEY=fake-test-key \
  npx tsx -e "import('./tests/agentic/runner/yaml-loader.ts').then(m => m.loadFlow('tests/agentic/flows/<name>.flow.test.yml')).then(f => console.log('parsed OK:', f.name)).catch(e => { console.error('PARSE FAIL:', e.message); process.exit(1) })"
```

**Schema-check:**

```bash
npx --yes js-yaml web-app/tests/agentic/flows/<name>.flow.test.yml > /tmp/flow.json
npx --yes ajv-cli@latest validate -s json-schemas/flow-test.json -d /tmp/flow.json
```

**Durability lint (`--dry-run`):**

```bash
cd web-app && pnpm test:agentic --dry-run <name>
```

The lint (`runner/lint.ts`) warns on `text-only-selector`,
`css-structure-selector`, and `no-selector-hint`. Two ignored warnings
is fine (the file-input id selector + `text=oxymart` table-picker are
the legitimate ones in canonical flows); more means the generated flow
has room to improve — go back, add testids where the lint flagged.

If any check fails, iterate. Common fixes:

- Unknown `target:` value → check the schema's enum.
- Unknown `wait_for:` primitive → check `runWaitFor` in `tool-registry.ts`.
- Unknown `setup:` command → check `SetupCommand` in `fixtures/reset.ts`.
- Indentation off → match an existing flow byte-for-byte for surrounding structure.
- Lint flagged `text-only-selector` → grep for a testid; add one to the source component if missing.

### 9. Determine CI bucket

The filename prefix determines the bucket:

- `builder-*` → `builder` bucket
- `semantic-*` → `semantic` bucket
- `chat-*` → `ask-agent` bucket
- `threads-*` → `threads` bucket
- `ide-*` → `ide` bucket
- `metric-tree-*` → `metric-tree` bucket

If the filename doesn't match any prefix, tell the dev: "This needs a new
bucket entry in the `resolve-matrix` job's inline JSON in
`.github/workflows/agentic-tests.yaml` (also add the bucket name to the
`flow_bucket` choice list under the `workflow_dispatch` trigger).
Buckets share `backend_mode` — adding a cloud-mode flow to a local-mode
bucket needs a bucket split first."

### 10. Report back

Show the dev:

1. **The path to the new file** + the CI bucket it lands in.
2. **First-run command**:
   ```bash
   HEADED=1 pnpm test:agentic <flow-stem>
   ```
   (or `/run-agentic-tests <flow-stem>` for the wrapped version with `DEBUG=1`).
3. **Cost expectation** vs the `_budgets.yml` ceiling you added.
4. **Where artefacts land**:
   - Markdown report: `web-app/tests/agentic/.results/<ts>.md`
   - JSON: `web-app/tests/agentic/.results/<ts>.json`
   - Trace on failure: `web-app/tests/agentic/.traces/<flow>-<case>.zip` — `pnpm exec playwright show-trace <path>`
5. **Fast CI loop** for iterating on selectors after the first cold record:
   ```bash
   gh workflow run "CI check" --repo oxy-hq/oxygen-internal \
     --ref <branch> --field agentic_only=true
   ```

## Error handling

- **The dev's description is too vague to author from.** Don't guess
  beyond what's stated. Stop and use `AskUserQuestion` for one targeted
  question (usually about the success criterion or the surface area).
- **A required testid doesn't exist on the page.** Tell the dev. Offer
  to author against `role=…[name='…']` or `text=…` (more expensive — the
  flow's selector strategies will still record fallbacks via
  `materializeStrategies()`, but the lint will flag and cold cost runs
  high). Also offer to file a follow-up to add the testid.
- **The README documents a primitive this command doesn't know about.**
  Trust the README. The skill is designed to forward-adapt.
- **The dev wants a setup command outside the 3-item list.** Don't
  invent it. Tell the dev the loader throws on unknowns. If the new
  fixture is genuinely needed, route them to opening a PR against
  `runner/fixtures/reset.ts` (subject to the read-only-against-external-
  systems policy at the top of `web-app/tests/agentic/README.md`).

## Notes

- Never invent setup commands, `wait_for:` primitives, or `assert:` forms
  the source doesn't document. Unknowns are silently dropped or throw —
  either way the flow won't do what you intend.
- Never put real secrets in the YAML. Use `${VAR}` placeholders from the
  `SECRET_ENV_VARS` allowlist (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
  `GEMINI_API_KEY`, `CLICKHOUSE_PASSWORD`, `OXY_DATABASE_URL`).
  Egress-substitution + `redactArgs()` defense-in-depth keeps plaintext
  out of the cache and result artifacts.
- Don't overwrite an existing flow file. If the dev wants to add a case
  to an existing flow, route them to `/agentic-test-add-case`.
- Don't skip the `_budgets.yml` entry. The reporter compares observed
  cost against the ceiling on every run; without an entry there's no
  guardrail on cost drift.
