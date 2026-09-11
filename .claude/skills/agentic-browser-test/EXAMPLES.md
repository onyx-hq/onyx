# Worked Examples

Five end-to-end examples covering the distinct surfaces / modes that ship
in CI today (all local-mode: the cloud-mode onboarding example went with
self-serve org creation). Each example shows the dev's request, the inferred answers,
the resulting YAML, and why it's shaped the way it is.

**The canonical reference is `web-app/tests/agentic/flows/*.flow.test.yml`
on `oxy-hq/oxygen-internal` `main`** — re-read those before authoring.
The examples below mirror those flows' current shape (post the
2026-05-11 prompt-tightening pass + the 2026-05-09 DuckDB onboarding
rewrite, both included in PR #2275).

The skill greps `web-app/src/**/*.tsx` for actual testids before writing —
don't invent them.

---

## 1. Chat panel question + SQL artifact verification — `cache_scope: shared` prelude

**Dev says:** *"test that asking the default agent a sales question on the
home page returns a SQL artifact"*

**Inferred:**
- Surface: `chat`. Bucket: `ask-agent`.
- Backend mode: local (default).
- Setup: `goto:/`. No fixtures needed beyond the demo project.
- The first `act:` is the canonical chat-prelude from
  `canonical-prompts.md` → opt into `cache_scope: shared` so `threads-list`
  reuses the same recording.
- Success: a SQL artifact renders.

```yaml
# web-app/tests/agentic/flows/chat-ask.flow.test.yml
# yaml-language-server: $schema=../../../../json-schemas/flow-test.json
#
# Ask a sales question on the Home page using the demo project's `default`
# agent (Oxymart sales analyst) and verify that a SQL artifact is rendered.

name: chat ask returns SQL artifact
target: chat

settings:
  runs: 1
  trace: on-failure
  cache_actions: true
  max_steps: 15

setup:
  - "goto:/"

cases:
  - name: ask a question and get a SQL artifact
    tags: [chat, critical]
    steps:
      # Canonical chat-prelude. `cache_scope: shared` lets `threads-list`
      # reuse this exact recording — see canonical-prompts.md. If you
      # edit this step, mirror the change in `threads-list.flow.test.yml`
      # or both flows pay cold cost on first record.
      - act: |
          Submit "What were the total weekly sales by store?" to the
          'default' agent on the home page:
          1. browser_click [data-testid=agent-selector-button].
          2. From the dropdown, click `role=menuitem[name='default']`.
          3. browser_type into textarea[name=question]: "What were the total weekly sales by store?"
          4. browser_click [data-testid=chat-panel-submit-button].
          End the turn once the URL changes to /threads/<id>.
        cache_scope: shared
      - wait_for: streaming_complete
    expect:
      - assert: "selector [data-testid=agent-artifact] is visible"
      - assert: "selector [data-testid=agent-artifact] has attribute data-artifact-kind=execute_sql"
      - judge: "the response includes a coherent answer about weekly sales per store, not an error message"
```

**Why this shape works:**
- One compound `act:` for four causally-coupled actions — the agent
  selector closes on click-outside, so opening the dropdown and picking
  an item has to be the same step.
- `cache_scope: shared` makes the recording reusable by `threads-list`.
  The step text is byte-identical to `threads-list`'s first step.
- Anchored on `[data-testid=…]` selectors throughout. No `text=`-only
  matches. Lint passes with zero findings.
- `role=menuitem[name='default']` is the correct Playwright form (NOT
  `menuitem[name='default']`).

---

## 2. IDE file edit + save (Monaco quirk) — `reset_test_file` fixture

**Dev says:** *"test that editing test.sql in the IDE and pressing Cmd+S
saves the file"*

**Inferred:**
- Surface: `ide`. Bucket: `ide`.
- Backend mode: local.
- Setup: `reset_test_file` wipes `demo_project/test.sql` to a clean state
  so reruns start consistent + `goto:/ide`.
- Monaco trap: the editable target is `aria-hidden`, sitting behind
  absolute-positioned visual layers — `browser_type`'s selector-based
  fill picks the wrong textarea. Hint at `browser_keyboard_type`.

```yaml
# web-app/tests/agentic/flows/ide-save.flow.test.yml
# yaml-language-server: $schema=../../../../json-schemas/flow-test.json
#
# Open `test.sql` from the demo project in the IDE, append a comment, save,
# and verify the save button hides. `reset_test_file` wipes test.sql to a
# clean state so reruns start consistently.

name: IDE open, edit, and save
target: ide

settings:
  runs: 1
  trace: on-failure
  cache_actions: true
  max_steps: 20

setup:
  - reset_test_file
  - "goto:/ide"

cases:
  - name: open test.sql, append a comment, save
    tags: [ide, critical]
    steps:
      - act: "In the IDE file tree on the left, click 'test.sql' (root). URL changes to /ide/files/<base64>; Monaco loads the file."
      - act: |
          Append '-- agentic test edit' to the open file:
          1. browser_click selector ".monaco-editor" to focus. (Page has
             TWO Monaco editors — file editor on top, SQL IDE results
             below. `.first()` picks the file editor.)
          2. browser_press_key "Meta+End" then "Enter".
          3. browser_keyboard_type (NOT browser_type — Monaco's hidden
             textarea breaks selector-based focus) text "-- agentic test edit".
      - act: "browser_press_key 'Meta+s'. Wait for [data-testid=ide-save-button] to disappear."
    expect:
      - assert: "save button is not visible"
      - judge: "the editor content includes the line '-- agentic test edit'"
```

**Why this shape works:**
- Three atomic steps — each is its own cache entry. If the dev later
  changes the appended-line text, only step 2 re-derives; the open and
  save still warm-replay.
- The Monaco quirk is encoded in-line: `browser_keyboard_type` is named,
  with the explicit "NOT browser_type" reminder. This sidesteps the
  paid-for bug where Monaco's `aria-hidden` textarea makes a
  selector-based fill silently no-op.
- The `assert: "save button is not visible"` form is the IDE-specific
  helper — `runner/judge.ts` does a 5s `waitFor({state:"hidden"})` rather
  than an immediate `isVisible()`, which races on the Meta+s → React
  state flush.
- `.monaco-editor` is one of the two intentionally-ignored lint findings
  (text-only-selector).

---

## 3. Builder dialog — compound `act:`, `restore_demo_file:` fixture

**Dev says:** *"test the Cmd+I builder agent can add a chart to the
insights dashboard"*

**Inferred:**
- Surface: `any` (the builder lives in the app preview pane). Bucket:
  `builder`.
- Backend mode: local — the builder runs against `demo_project/insights.app.yml`.
- The builder dialog must be opened **and** submitted in a single `act:`
  because Meta+i toggles the dialog closed if pressed twice.
- `restore_demo_file:insights.app.yml` reverts the file before each run
  so the builder edit doesn't compound across reruns.

```yaml
# web-app/tests/agentic/flows/builder-edits-app.flow.test.yml
# yaml-language-server: $schema=../../../../json-schemas/flow-test.json

name: builder agent adds a chart to insights dashboard
target: any

settings:
  runs: 1
  trace: on-failure
  cache_actions: true
  backend_mode: local
  max_steps: 30

# `aW5zaWdodHMuYXBwLnltbA==` is base64("insights.app.yml"). Local-mode
# router has no org/workspace prefix — `/apps/<base64>` resolves directly
# to AppPage rendering the file from `demo_project/`.
setup:
  - "restore_demo_file:insights.app.yml"
  - "goto:/apps/aW5zaWdodHMuYXBwLnltbA=="

cases:
  - name: open builder via Cmd+I, ask for a new chart, wait for completion
    tags: [builder, app, critical]
    steps:
      # Page-state lock — if the LLM drifts off /apps/<id> mid-flow,
      # this fails fast rather than silently testing the wrong surface.
      - wait_for: "selector:[data-testid=app-page-root]"

      - act: |
          Open the builder dialog and submit a build prompt in one
          atomic sequence. Auto-approve starts OFF in a fresh browser
          context (localStorage empty → autoApprove === false at mount);
          one unconditional click flips it ON.

          1. browser_press_key "Meta+i". Dialog appears with
             [data-testid=builder-input-textarea] pre-filled with `@insights `.
          2. browser_click [data-testid=builder-auto-approve-toggle]
             EXACTLY ONCE to flip auto-approve ON. Don't read the
             toggle's data-state attribute first — it starts OFF in a
             clean context.
          3. browser_click [data-testid=builder-input-textarea] to focus.
          4. browser_keyboard_type (NOT browser_type) to APPEND:
               "Add a bar chart at the bottom of the dashboard showing
                total weekly sales by store. Title it 'Sales By Store'
                and filter by the existing date and store controls."
          5. browser_press_key "Enter". URL changes to /threads/<id>.

      - wait_for: streaming_complete

    expect:
      - judge: |
          The thread page shows the builder run completed successfully:
          (a) the assistant produced a response referencing a new task
          or display block added to insights.app.yml,
          (b) no red error banner is visible,
          (c) no Stop button beside the chat input.
```

**Why this shape works:**
- Compound `act:` is mandatory here — Meta+i is a toggle. Five tightly-
  coupled actions in one step keeps the dialog open through the whole
  sequence.
- `restore_demo_file:insights.app.yml` reads from `git show HEAD:…` so
  the file is reset without touching the dev's index. Symlink-safe.
- The judge is the substance of the test. No selector-based asserts on
  the thread page — Radix portals + scroll position make them brittle,
  and the judge already covers what matters.

---

## 4. Threads list — shared prelude across flows

**Dev says:** *"test that the threads list page renders and a user can
open a past thread"*

**Inferred:**
- Surface: `threads`. Bucket: `threads`.
- Backend mode: local.
- A fresh local-mode Postgres has zero threads. Rather than add a
  `seed_thread` fixture (extra surface area), the flow drives a
  chat-ask prelude first to populate the list. The prelude's `act:` text
  is byte-identical to `chat-ask`'s first step → `cache_scope: shared`.

```yaml
# web-app/tests/agentic/flows/threads-list.flow.test.yml
# yaml-language-server: $schema=../../../../json-schemas/flow-test.json

name: threads list
target: threads

settings:
  runs: 1
  trace: on-failure
  cache_actions: true
  backend_mode: local
  max_steps: 25

setup:
  - "goto:/"

cases:
  - name: submit a question, navigate to /threads, open the seeded thread
    tags: [threads, regression]
    steps:
      # Canonical chat-prelude — byte-identical to chat-ask's step so
      # `cache_scope: shared` lets both flows reuse the same recording.
      - act: |
          Submit "What were the total weekly sales by store?" to the
          'default' agent on the home page:
          1. browser_click [data-testid=agent-selector-button].
          2. From the dropdown, click `role=menuitem[name='default']`.
          3. browser_type into textarea[name=question]: "What were the total weekly sales by store?"
          4. browser_click [data-testid=chat-panel-submit-button].
          End the turn once the URL changes to /threads/<id>.
        cache_scope: shared

      - wait_for: streaming_complete

      - act: |
          browser_click [data-testid=sidebar-threads-toggle]. URL
          changes to /threads with a paginated thread list.

      - wait_for: "selector:[data-testid=thread-item]"

      - act: |
          browser_click [data-testid=thread-title] on the first row.
          URL changes to /threads/<id>; conversation history loads.

      - wait_for: network_idle

    expect:
      - judge: |
          The page is on a thread detail URL (/threads/<id>) with a past
          conversation visible (at least one user message + one agent
          response). The conversation is about weekly sales per store
          (the question we submitted in the prelude).
```

**Why this shape works:**
- The shared prelude is the optimization: if `chat-ask` already recorded
  the four-step submit sequence on `cache_scope: shared`, this flow's
  first `act:` resolves to the same cache entry. Cold record happens
  once, replays free across both flows.
- After the chat submit, the flow navigates to `/threads` (list) then
  into `/threads/<id>` (detail). The judge claim covers both — naming
  the question we asked makes the regression diagnostic ("the prelude
  was cached, but the thread we opened isn't the one we just created").

---

## 5. Regression pattern — bug-fix coverage

**Dev says:** *"add a regression test for the bug I just fixed where the
agent selector text didn't update after switching agents mid-session"*

**Inferred:**
- Surface: `chat`. Bucket: `ask-agent`.
- Backend mode: local.
- The demo project ships two agents: `default` and `analytics`. Toggle
  default → analytics → back to default and assert the trigger text
  reflects the active agent.
- The bug class is "selector state desyncs from chat input state in some
  scroll positions" — capture the user-visible symptom in the `judge:`
  claim.

```yaml
# web-app/tests/agentic/flows/chat-panel-agent-switch.flow.test.yml
# yaml-language-server: $schema=../../../../json-schemas/flow-test.json

name: chat panel agent switch
target: chat

settings:
  runs: 1
  trace: on-failure
  cache_actions: true
  backend_mode: local
  max_steps: 25

setup:
  - "goto:/"

cases:
  - name: switch from default to analytics agent and back
    tags: [chat, regression]
    steps:
      - wait_for: "selector:[data-testid=agent-selector-button]"

      - act: |
          browser_click [data-testid=agent-selector-button] to open the
          dropdown, then browser_click `role=menuitem[name='analytics']`.
          Trigger button updates to 'analytics'.

      - wait_for: "selector:[data-testid=agent-selector-button]"

      - act: |
          browser_click [data-testid=agent-selector-button] again, then
          browser_click `role=menuitem[name='default']`. Trigger button
          reverts to 'default'.

    expect:
      - assert: "selector [data-testid=agent-selector-button] is visible"
      - judge: |
          The page is on the home/chat surface, the agent selector
          button is visible, and the selector text reflects the
          'default' agent (NOT 'analytics' or another agent). No error
          banner is visible.
```

**Why this shape works for regressions:**
- Tight scope: one bug → one case. Don't bundle unrelated regressions
  into one flow; they share a cache entry across runs.
- The `wait_for` between toggles is the synchronisation gate — without
  it the second act could race the first dropdown closing.
- The `judge:` claim names the **specific behaviour** the bug had and
  the fixed behaviour ("selector text reflects 'default' agent, NOT
  'analytics'") so a future reader can recognise the regression at a
  glance.

---

## Patterns to copy verbatim (cache reuse)

The action cache key for `cache_scope: shared` is
`sha256("shared|" + stepText)`. Byte-identical step text across flows
resolves to the same entry. When authoring a new flow whose prelude
matches an existing one, **copy the exact wording from
`canonical-prompts.md`** rather than paraphrasing — same key, free reuse.

Common sub-sequences canonicalized today:

- **Submit a question to the default agent** — copy from
  `canonical-prompts.md` § Chat panel.

Don't paraphrase for readability — paraphrasing costs ~$0.05–0.20 of LLM
the first time a paraphrased flow runs in CI, and breaks shared-scope
reuse across both flows.

---

## Patterns to avoid

- **Inventing setup commands.** Only the 3 documented commands in
  `fixtures/reset.ts:SetupCommand` (`reset_test_file`,
  `restore_demo_file:<rel>`, `goto:<path>`) are honoured. Anything else
  throws at load time.
- **Inventing `wait_for:` primitives.** The 4 primitives in `runWaitFor`
  (`tool-registry.ts`) are exhaustive. `selector_hidden:` is new
  (2026-05-10); don't write `selector_invisible:` or similar.
- **Hard-coding secrets.** Use `${VAR}` placeholders from the
  `SECRET_ENV_VARS` allowlist. Adding a new secret requires extending
  `runner/secrets.ts`.
- **Putting verification logic in `steps:`.** Steps act on the page;
  verification belongs in `expect:`. Don't write "verify the response is
  rendered" as an `act:`.
- **`cache_actions: false` as a "make it work" hack.** Flaky warm
  replay is a runtime bug to file against the runner, not papered over.
- **Mixing `backend_mode` across flows in one invocation.** The runner
  errors loudly. CI buckets are single-mode for this reason.
