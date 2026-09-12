---
source:
  - oxy-hq/skills/skills/oxy-app-builder/SKILL.md
  - oxy-hq/skills/skills/oxy-app-builder/QUICK-REFERENCE.md
reconciled-at: 0c51d931c1f9c1900d4e92e3a55cfa053e50d94c
note: |
  Authored condensation. Not auto-synced — scripts/sync-skills.sh only copies
  the verbatim YAML templates. Re-condense by hand when source material
  changes materially; keep under ~200 lines so the LLM context stays focused.
  scripts/check-skills-drift.sh flags upstream changes.
---

# Oxy App Builder Reference

An Oxy data app is a `*.app.yml` file that pairs **tasks** (operations that
produce tabular or text output) with **displays** (visualizations that render
that output). Mental model: `task -> output -> display`.

> **Two app models — don't confuse them.** This card covers the declarative
> `*.app.yml` app (tasks + displays, rendered by `oxy serve --enterprise`). The
> other model is a **custom-code (React/Vite) app** identified by an
> **`oxy-app.json`** manifest at the app root and deployed with `oxyc publish` —
> see "## Custom-code apps (`oxy-app.json`)" at the end. `oxy-app.json` is the
> deploy manifest for that path, **not** an alternative to `*.app.yml`; never
> claim "there is no oxy-app.json." Build a `*.app.yml` for a dashboard; build
> the custom-code path for bespoke UI beyond tables/charts/markdown.

## File shape

```yaml
name: snake_case_app_name            # optional, but recommended
title: "Human-Friendly App Title"    # recommended — surfaces in app
                                     # listings; falls back to filename
description: |                        # optional, multi-line OK
  What this app shows.

controls:                             # optional, interactive widgets
  - name: region                      # see "## Interactive controls"
    type: select
    options: [All, North, South]
    default: "All"

tasks:                                # REQUIRED, at least one entry
  - name: task_name                   # unique, snake_case
    type: semantic_query              # or execute_sql | workflow | agent
    mode: server                      # optional, see "## Task execution mode"
    # ... type-specific fields

display:                              # REQUIRED, at least one entry
  - type: table
    title: "Section Title"            # optional
    data: task_name                   # references a task by name

published: false                      # optional
```

`AppConfig` uses `deny_unknown_fields`. The only accepted top-level keys
are: `name`, `title`, `description`, `controls`, `tasks`, `display`,
`published`. Anything else fails validation.

## Task types

### `semantic_query` — preferred when the semantic model covers the data

```yaml
- name: revenue_by_month
  type: semantic_query
  topic: sales_mrr                    # must match a *.topic.yml name
  dimensions:
    - sales.month                     # view.dimension
  measures:
    - sales.total_revenue
    - sales.order_count
  filters:                            # optional
    - field: sales.year
      op: eq                          # eq | neq | gt | gte | lt | lte | in
      value: 2024
  orders:                             # optional
    - field: sales.month
      direction: asc                  # asc | desc
```

**Column naming:** semantic query outputs use `view__field` (double underscore)
in display references. So `dimensions: [sales.month]` and
`measures: [sales.total_revenue]` are referenced as `x: sales__month` and
`y: sales__total_revenue`.

**Time-dimension granularity suffix.** When a `time_dimensions` entry sets
`granularity:`, the output column gets an extra `__<granularity>` suffix
appended. So:

```yaml
time_dimensions:
  - dimension: orders.created_date
    granularity: month
```

produces an output column literally named `orders__created_date__month`, and
chart `x:` must reference it with the suffix:
`x: orders__created_date__month` (NOT `x: orders__created_date`). Omitting
the suffix produces a "column not found" Binder error in the in-browser
DuckDB chart engine and the chart silently fails to render.

**Entity labeling — never put a raw UUID/FK on a chart axis.** When the
primary entity's `key:` is an opaque ID (`guid`, `restaurant_id`,
`customer_id`, `order_id`, …), do NOT use that field as the chart `x:`,
`name:`, or table-row label — the dashboard will render uninformative UUIDs.
Two ways to surface a human-readable label instead:

1. **Preferred — semantic_query via foreign entity.** If the joined view
   exposes a name dimension (e.g. `restaurants.location_name`), make sure
   the FK side declares the joined view as a `foreign` entity (see
   semantic-layer card), and pull the name through the topic:
   `dimensions: [restaurants.location_name]` instead of
   `dimensions: [orders.restaurant_id]`. Output column is then
   `restaurants__location_name`.
2. **Fallback — execute_sql with a JOIN.** When semantic_query can't reach
   the name (no foreign entity, multi-view topic doesn't exist), use
   `execute_sql` with an explicit JOIN to the lookup view and `SELECT`
   the name column directly.

If neither path is available (no name field exists in any related view),
fall back to the FK but warn in the markdown header that rows are keyed
by ID — never silently render UUIDs as a chart axis.

### `execute_sql` — fallback for custom SQL

```yaml
- name: sales_by_region
  type: execute_sql
  database: clickhouse                # clickhouse | postgres | bigquery | local
  sql_query: |
    SELECT region, SUM(amount) AS total_sales
    FROM sales
    GROUP BY region
```

Or with a file reference:

```yaml
- name: sales_report
  type: execute_sql
  database: clickhouse
  sql_file: queries/sales_report.sql
```

Column names come from the SELECT aliases (`x: region`, `y: total_sales`).

### `workflow` — invoke a sub-workflow

```yaml
- name: ops
  type: workflow
  src: workflows/operations.automation.yml
  variables:
    period: "2024-Q4"
```

Reference the inner task outputs with dot notation: `data: ops.location_summary`.

### `agent` — AI-generated narrative

```yaml
- name: insights
  type: agent
  agent_ref: analyst.agent.yml
  inputs:                             # previous task names passed as context
    - revenue_data
  prompt: |
    Summarize the revenue data in 3 bullet points.
```

Reference the agent's text output in markdown via `{{insights}}`.

## Display types

### `markdown`

```yaml
- type: markdown
  title: "AI Insights"                # optional
  content: |
    # Header
    Free-form markdown. Agent outputs can be interpolated: {{agent_task_name}}.
```

### `table`

```yaml
- type: table
  title: "Regional Details"           # optional
  data: task_name                     # REQUIRED
```

### `line_chart`

```yaml
- type: line_chart
  title: "Revenue Over Time"
  data: monthly_revenue
  x: month                            # REQUIRED
  y: revenue                          # REQUIRED
  x_axis_label: "Month"               # optional
  y_axis_label: "Revenue ($)"         # optional
  series: region                      # optional; splits into multiple lines
```

### `bar_chart`

```yaml
- type: bar_chart
  title: "Sales by Region"
  data: regional_sales
  x: region                           # categorical
  y: total_sales                      # numeric
  series: product_category            # optional; grouped/stacked bars
```

### `pie_chart`

```yaml
- type: pie_chart
  title: "Market Share"
  data: market_data
  name: company                       # label column
  value: market_share                 # numeric column
```

### `row` — side-by-side layout

```yaml
- type: row
  columns: 2                          # optional; defaults to len(children). >= 1.
  children:                            # required, list of display blocks
    - type: bar_chart
      data: by_category
      x: category
      y: revenue
    - type: pie_chart
      data: by_region
      name: region
      value: revenue
```

`children` may contain any display block (charts, tables, markdown, or
`control` blocks). Use rows to place paired charts or KPI tables next
to each other.

## Interactive controls

Controls are widgets (dropdown / date picker / on-off toggle) rendered
as a bar above the dashboard. Changing a control re-runs every task
whose SQL references it via `{{ controls.<name> }}` Jinja.

### Three widget kinds

| Widget kind | Renders as     | Value type            | Use for                     |
| ----------- | -------------- | --------------------- | --------------------------- |
| `select`    | Dropdown       | string                | Pick one option from a list |
| `date`      | Date picker    | string `YYYY-MM-DD`   | Pick a date                 |
| `toggle`    | On/off switch  | boolean               | Yes/no filters              |

### Two declaration forms — pick one, do not mix

**Inline `- type: control` inside `display:` (preferred).** The widget
kind uses the key **`control_type:`**, not `type:` — `type:` is already
consumed by the display discriminant.

```yaml
display:
  - type: control
    name: region                      # -> {{ controls.region }}
    control_type: select              # control_type, NOT type
    label: Region                     # optional, defaults to name
    options: [All, North, South]
    default: "All"

  - type: control
    name: start_date
    control_type: date
    default: "2024-01-01"

  - type: control
    name: holidays_only
    control_type: toggle
    default: false
```

**Top-level `controls:` array (alternative).** Each entry uses plain
`type:` for the widget kind:

```yaml
controls:
  - name: region
    type: select                      # plain `type:` at top level
    options: [All, North, South]
    default: "All"
```

The two forms are merged at load time — declaring the same control in
both duplicates the widget.

> **#1 controls mistake.** Inside `- type: control`, writing
> `type: select` instead of `control_type: select` is invalid. The
> display-form key is `control_type:`; the top-level-array form key is
> `type:`. They are not interchangeable.

### Control fields

| Field          | Required | Applies to | Notes                                                            |
| -------------- | -------- | ---------- | ---------------------------------------------------------------- |
| `name`         | yes      | all        | snake_case; referenced as `{{ controls.<name> }}`                |
| `control_type` | yes      | inline     | `select` \| `date` \| `toggle`. Top-level array uses `type:`.    |
| `label`        | no       | all        | UI label; defaults to `name`.                                    |
| `default`      | no       | all        | Initial value. Quote strings; toggle uses `true`/`false`. Jinja OK. |
| `options`      | no       | select     | Static list of dropdown choices.                                 |
| `source`       | no       | select     | Task name whose first column populates the dropdown.             |

Use `source:` OR `options:`, not both.

### Populating a `select` from data

```yaml
tasks:
  - name: store_list                  # feeds the dropdown
    type: execute_sql
    database: local
    sql_query: |
      SELECT 'All' AS Store
      UNION ALL
      SELECT DISTINCT CAST(Store AS VARCHAR) FROM 'sales.csv' ORDER BY Store

display:
  - type: control
    name: store
    control_type: select
    source: store_list                # use `source:`, NOT `options:`
    default: "All"
```

The dropdown can't be empty, so include the `All` sentinel row yourself.

### Referencing controls in SQL

```sql
SELECT region, SUM(revenue) AS total
FROM sales
WHERE sale_date >= {{ controls.start_date | sqlquote }}
  AND ({{ controls.region | sqlquote }} = 'All'
       OR region = {{ controls.region | sqlquote }})
  {% if controls.holidays_only %}AND period = 'Holiday'{% endif %}
GROUP BY region
```

Rules — the agent gets these wrong:

1. **Pipe every string/date value through `| sqlquote`.** It wraps in
   single quotes and escapes embedded quotes (`O'Brien` → `'O''Brien'`).
2. **Never add your own quotes around a `sqlquote` value.**
   `'{{ controls.x | sqlquote }}'` produces `''value''` — broken SQL.
3. **Optional-filter idiom.** A `select` can't be empty, so use the
   `All` sentinel:
   `({{ controls.x | sqlquote }} = 'All' OR col = {{ controls.x | sqlquote }})`.
4. **Toggle filters inside `{% if %}`** —
   `{% if controls.flag %}AND ...{% endif %}` (no `else` clause).
5. **Date values are strings.** Compare directly
   (`col >= {{ controls.d | sqlquote }}`) or `TRY_CAST(... AS DATE)`.

### Client-mode Jinja is intentionally minimal

Client-mode tasks re-run in the browser's DuckDB WASM engine, which
understands **only these four Jinja forms**:

- `{{ controls.x }}` — raw substitution
- `{{ controls.x | sqlquote }}` — quoted SQL string literal
- `{{ controls.x | default('v') }}` — substitution with fallback
- `{% if controls.x %}...{% endif %}` — truthy-only conditional

Anything else — `{% for %}` loops, `{% if a == b %}` comparisons,
`{% else %}` / `{% elif %}`, other filters — breaks the live re-run.
Put comparison logic in SQL (`CASE`, `OR`), not in Jinja.

`default:` and `options:` may use Jinja evaluated once at app load,
most usefully `now()`:

```yaml
- type: control
  name: year
  control_type: select
  options: ["All", "{{ now(fmt='%Y') }}", "{{ now(fmt='%Y') | int - 1 }}"]
  default: "All"
```

## Task execution mode (`mode`)

Every task takes an optional `mode:` — `client` (default) or `server` —
that controls how it re-runs when a control changes:

| `mode`             | Re-runs on            | Use when                                                |
| ------------------ | --------------------- | ------------------------------------------------------- |
| `client` (default) | Browser DuckDB WASM   | `execute_sql` + inline `sql_query` + `database: local`  |
| `server`           | Server                | External warehouse, `sql_file:`, workflow/semantic/agent |

```yaml
- name: revenue
  type: execute_sql
  database: clickhouse
  mode: server                        # external DB -> server mode
  sql_query: |
    SELECT ... WHERE store = {{ controls.store | sqlquote }}
```

Tasks against non-local databases are forced to server mode regardless
of the YAML, so **when in doubt set `mode: server`** — it always works.
For local-DuckDB apps with inline SQL, leave `mode` unset.

## SQL dialect notes

When you author or profile SQL inside an `execute_sql` task — or when
you ask the warehouse for shape/distribution stats before picking
fields — pick the right dialect for the configured database. The
common gotchas:

| Dialect    | `DATE_TRUNC` form                                  | Stddev fn                |
| ---------- | -------------------------------------------------- | ------------------------ |
| BigQuery   | `DATE_TRUNC(<col>, MONTH)` (column first, no quotes) | `STDDEV(<col>)`          |
| Snowflake  | `DATE_TRUNC('month', <col>)`                       | `STDDEV(<col>)`          |
| Postgres   | `DATE_TRUNC('month', <col>)`                       | `STDDEV(<col>)`          |
| DuckDB     | `DATE_TRUNC('month', <col>)`                       | `STDDEV(<col>)`          |
| ClickHouse | `toStartOfMonth(<col>)`                            | `stddevPop(<col>)` (lowercase) |

Other places `.app.yml` SQL diverges across dialects:

| Concern             | Postgres / DuckDB            | Snowflake               | BigQuery                          | ClickHouse / MySQL    |
| ------------------- | ---------------------------- | ----------------------- | --------------------------------- | --------------------- |
| Identifier quoting  | `"col"`                      | `"col"`                 | `` `col` ``                       | `` `col` `` / unquoted |
| Cast to date        | `CAST(x AS DATE)`, `x::date` | `CAST(x AS DATE)`       | `CAST(x AS DATE)`                 | `toDate(x)`           |
| Date arithmetic     | `d + INTERVAL '1 day'`       | `DATEADD(day, 1, d)`    | `DATE_ADD(d, INTERVAL 1 DAY)`     | `d + INTERVAL 1 DAY`  |

## Profiling template

Before committing to a measure or entity for a chart or ranked table,
profile the underlying data so you don't ship a flat-line trend or a
top-10 with one row in it. One consolidated SELECT is enough:

```sql
SELECT
  COUNT(*) AS rows,
  COUNT(DISTINCT <entity_expr>) AS entity_card,
  MIN(<time_expr>) AS min_date, MAX(<time_expr>) AS max_date,
  COUNT(DISTINCT DATE_TRUNC('month', <time_expr>)) AS month_count,
  MIN(<measure_expr>) AS min_val, MAX(<measure_expr>) AS max_val,
  STDDEV(<measure_expr>) AS measure_stddev
FROM <table>
```

Substitute `<entity_expr>`, `<time_expr>`, `<measure_expr>`, and
`<table>` with the view's actual `expr:` strings — never guess column
names. Apply the dialect substitutions from the table above for
`DATE_TRUNC` and `STDDEV` when the warehouse is BigQuery or
ClickHouse.

A topic is fit for ranking and trend visualizations when:

- `rows ≥ 100`,
- `month_count ≥ 3` (enough time for a meaningful trend),
- `measure_stddev > 0` (not a flat measure that draws as a horizontal
  line at one value),
- `entity_card` is between 5 and 500 (top/bottom-N actually differ).

## Failure recovery

Profiling queries fail mid-build for routine reasons — dialect
mismatch, type-cast errors, an aggregation function the warehouse
doesn't expose. The recovery rule is:

1. **Simplify and retry once** — drop `STDDEV`, drop `month_count`, or
   replace `DATE_TRUNC` with the dialect's equivalent. At most two
   attempts per topic in total.
2. **Skip the topic on the second failure** — never loop on the same
   failing query. Move on to the next candidate; if none qualify,
   omit the affected block entirely rather than ship a misleading
   chart.

This rule applies anywhere in `.app.yml` authoring where you query
the warehouse before committing layout decisions, not just the
onboarding flow.

## Common patterns

### Multi-task dashboard (KPIs + trend + breakdown)

```yaml
name: executive_dashboard

tasks:
  - name: kpis
    type: semantic_query
    topic: sales_mrr
    measures:
      - sales.total_revenue
      - sales.order_count
      - sales.avg_order_value

  - name: monthly
    type: semantic_query
    topic: sales_mrr
    dimensions:
      - sales.month
    measures:
      - sales.total_revenue
    orders:
      - field: sales.month
        direction: asc

  - name: regional
    type: semantic_query
    topic: sales_mrr
    dimensions:
      - sales.region
    measures:
      - sales.total_revenue
    orders:
      - field: sales.total_revenue
        direction: desc

display:
  - type: markdown
    content: "# Executive Dashboard"

  - type: table
    title: "Key Metrics"
    data: kpis

  - type: line_chart
    title: "Monthly Revenue"
    data: monthly
    x: sales__month
    y: sales__total_revenue

  - type: bar_chart
    title: "Revenue by Region"
    data: regional
    x: sales__region
    y: sales__total_revenue

  - type: table
    title: "Regional Details"
    data: regional
```

### Semantic query + agent insights

```yaml
tasks:
  - name: revenue_by_segment
    type: semantic_query
    topic: sales_metrics
    dimensions: [segment.name]
    measures: [sales.total_revenue]

  - name: callouts
    type: agent
    agent_ref: sales_analyst.agent.yml
    inputs: [revenue_by_segment]
    prompt: "Give 3 bullet-point callouts on segment performance."

display:
  - type: markdown
    title: "Callouts"
    content: "{{callouts}}"

  - type: bar_chart
    title: "Revenue by Segment"
    data: revenue_by_segment
    x: segment__name
    y: sales__total_revenue
```

## Build rules

1. **Prefer `semantic_query` over `execute_sql`** whenever a matching topic
   exists. It surfaces join logic the LLM otherwise has to re-derive.
2. **Every `display.data` must match a `tasks[].name` exactly.** Dot notation
   (`workflow_task.inner`) is allowed only for `workflow`-type tasks.
3. **Chart column fields (`x`, `y`, `series`, `name`, `value`) must match
   real output columns.** For semantic queries that means `view__field`.
4. **Unique, snake_case task names.** Duplicates silently shadow each other
   in some dialects.
5. **Never add `# yaml-language-server:` schema comments.** Oxy's validator
   treats unknown fields strictly.
6. **Inline `- type: control` blocks set the widget kind with
   `control_type:`, not `type:`.** Use one declaration form per app —
   inline `display:` or top-level `controls:`, never both for the same
   control.
7. **Tasks that read controls and run against non-local databases must
   set `mode: server`.** Client mode only works for local-DuckDB inline
   `execute_sql`.

## Validation

- `oxy validate --file=my_app.app.yml` checks YAML structure only — it
  does **not** execute task SQL, verify that `{{ controls.x }}` matches
  a declared control, or check that `source:` names a real task.
- Apps render in the Oxy web UI (`oxy start --enterprise`). `oxy run` does
  **not** execute `.app.yml` files.
- **Pre-test referenced workflows and agents before finalizing the app.**
  Each task that points at a `workflow_ref` / `agent_ref` is opaque to
  `oxy validate`; SQL errors inside those files won't surface until the
  app runs. Run any referenced `*.automation.yml` and `*.agent.yml` once
  to confirm they execute clean before opening the app.
- **After adding controls, smoke-test the app and change each control**
  to confirm dependent tasks re-run — control wiring fails only at
  runtime.

## Smoke-testing the app (do this before declaring done)

Schema validation (`validate_project`) only catches structural YAML errors.
It does **not** catch malformed SQL — broken JOIN syntax, dialect-specific
type mismatches, missing `ON` clauses, type coercion failures — which all
fail at task-execution time. The most common failure mode is a brand-new
`.app.yml` that loads as a blank dashboard with a runtime error, because
the model wrote SQL that *parses* but doesn't *run* on the target warehouse.

After every write or edit to a `.app.yml`, run the file end-to-end and
confirm every task succeeds. Use the `run_app(file_path, params_json)`
tool when it is available — it executes the same path the dashboard takes
on first load and returns a per-task summary. Pass `params_json: "{}"` to
exercise control defaults; pass a real JSON object when you specifically
need to validate behavior under non-default control values. If the tool
is not available in the current run, fall back to running each task
manually: invoke `semantic_query` for `type: semantic_query` tasks
(passing the same topic / dimensions / measures) and `execute_sql` for
`type: execute_sql` tasks (passing the rendered `sql_query` against the
same `database`).

Failure-recovery loop (matches the pattern used by view smoke-tests):

1. If every task succeeds, stop — the file is done.
2. If any task fails, read the error, diagnose, and call `edit_file`
   **once** with a targeted corrective edit — exact `old_string` /
   `new_string` pair (most common fixes: re-state the JOIN with `ON`,
   replace a dialect-specific function with the matrix entry from
   `## SQL dialect notes`, or drop the offending measure / dimension
   and pick a different one from the view).
3. Re-run `run_app` (or the manual fallback) **once**. If it still
   fails, stop and report the error verbatim — do **not** loop. A second
   silent retry usually compounds the bad guess.

This step is non-negotiable for onboarding apps because the user has no
chance to fix the file before opening it; for chat-builder edits, prefer
running the smoke test whenever the warehouse tools are reachable.

## Custom-code apps (`oxy-app.json`)

Everything above is the declarative `*.app.yml` path (no manifest needed). The
**custom-code** path is a React/TS/Vite front-end deployed with `oxyc publish`;
every publishable app is identified by an **`oxy-app.json`** manifest at its
root. Build this path when the user wants bespoke UI beyond tables/charts/
markdown, or says "publish", "deploy", "oxy-app.json", or points at a
`module-designs`-style React app. For a plain dashboard, build a `*.app.yml`.

**The manifest** (`oxy-app.json`, the single source of truth the CLI reads):
required `schemaVersion` (currently `2`), `slug`, `orgSlug`, `name`, and
`build.outDir` (normally `dist`); optional `description`, `status` (card
subtitle line), `art` (card image), `icon`, and `ask`
(`{ agent: <path to .agentic.yml>, suggestedQuestions: [...] }` wires the
launcher ask box to an agent). `env` / `target` / `project` are **not** baked
into the manifest — CI or CLI flags supply them (`OXY_TARGET`, `OXY_ORG`,
`OXY_ENV`, `--project`).

**Three load-bearing rules** (getting any one wrong breaks the publish, not
local dev):

1. `oxy-app.json` at the module root.
2. `vite.config.ts` with `base: process.env.OXY_APP_BASE_PATH || "/"` (so
   code-split chunks resolve under `/customer-apps/<org>/<app>/`; local
   `pnpm dev`/`build` fall back to `/`) **and** a `copyOxyAssets()` plugin that
   copies `oxy-app.json` + its referenced `icon`/`art` into `dist/` on
   `closeBundle`. Load-bearing: `oxyc publish` tars only the built dir, and the
   server captures launcher-card metadata from the bundle's own `oxy-app.json`
   (`app_builds.manifest_json`) — a manifest left only at the module root never
   reaches the server.
3. **The workspace root belongs to whoever owns the repo.** An app inside the
   customer-apps monorepo must NOT carry its own `pnpm-workspace.yaml`: pnpm
   resolves a workspace root as the nearest ancestor, so one at
   `apps/<org>/<app>/` makes that app its own root and cuts it off from the
   monorepo's overrides, catalog, and `workspace:` links to the shared packages
   its bundle imports. The monorepo root owns that file — including any
   `allowBuilds` entry the app's dependencies need. A *standalone* app,
   scaffolded by `create-oxy-app` outside the monorepo, does own its root, and
   needs `allowBuilds: { esbuild: false }` there **only if it depends on esbuild
   directly**: under Vite 8 esbuild is an **optional peer**, not a dependency,
   so it is not pulled in transitively and a frontend-only app needs no
   build-script declaration at all. (`false` rather than `true` because esbuild
   ships its platform binary as an optional dependency and runs fine without its
   postinstall.)

**Publish flow:** run `oxyc publish` from the module dir — it builds, tars
`dist/`, and uploads the app as a **draft**; an Oxy admin promotes the draft to
live in the admin UI (CI never publishes straight to the live channel). First
publish auto-registers the app when `--project` is passed. `oxyc` is the
`@oxy-hq/cli` npm package (`npm install -g @oxy-hq/cli`); the `oxy` binary no
longer has a `publish` subcommand. Check `oxyc publish --help` for the flags.
Reference
implementations live in `module-designs/` (`DEPLOYMENT.md` + each module's
`oxy-app.json`).
