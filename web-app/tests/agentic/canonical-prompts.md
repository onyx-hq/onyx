# Canonical Prompts Library

Verbatim `act:` step text intended to be copy-pasted into new flows. The
runtime hashes `stepText` to derive cache keys; two flows that paste the
same text resolve to the same recording, so reusing prompts here gives
**cross-flow cache reuse** — the prelude is recorded once and replayed
free across every flow that uses it.

To opt into shared caching, add `cache_scope: shared` to the step. The
default scope is `flow` (private to that flow file/case/step index).

## How to use

1. Find the snippet for the surface you need (e.g. the chat panel).
2. Copy the YAML block **verbatim** into your flow's `steps:` list.
3. Do not edit the prompt text — even whitespace/punctuation changes
   make the cache key diverge.
4. If your flow needs a tweaked variant (different org name, different
   workspace type), keep the canonical step verbatim and add a follow-up
   step with the divergence.

> **Naming convention.** All testid selectors used here follow the
> `[<feature>-<element>]` pattern, mirroring the source attributes
> (e.g. `chat-panel-submit-button`, `builder-input-textarea`).
> If a snippet here references a testid that no longer resolves, the
> recording will Tier-1-heal on its first replay (re-ranking the
> remaining fallback strategies silently) and the failure will
> surface in the markdown summary's drift section.

---

## Builder dialog

### Open the builder via Cmd+I and submit a build prompt (single action)

The builder dialog must be opened **and** submitted in a single `act:`
because Meta+i toggles the dialog closed if pressed twice. See
`builder-edits-app.flow.test.yml` for the canonical sequence.

```yaml
- act: |
    Open the builder dialog AND submit a build prompt in one action
    sequence (do not re-press Meta+i mid-sequence; do not click outside
    the dialog):

    1. browser_press_key with key "Meta+i" to open the dialog. Verify
       [data-testid=builder-dialog-root] becomes visible.
    2. If [data-testid=builder-auto-approve-toggle] has data-state="off",
       click it to flip ON. (If already on, leave it.)
    3. browser_click [data-testid=builder-input-textarea] to focus.
    4. browser_keyboard_type the build prompt text.
    5. browser_press_key with key "Enter". The dialog closes and the URL
       changes to /threads/<id>.
```

(No `cache_scope: shared` here — the prompt body varies per flow, so
sharing across flows would defeat the purpose. The mechanics around the
prompt body are stable but the body itself changes.)

---

## Chat panel

### Ask a question via the Ask dock

The Ask dock ([data-testid=ask-dock]) is the universal ask surface: the
top-bar "Ask Oxygen" button ([data-testid=ask-oxygen-button]) opens it, and
submitting streams the answer inside the dock in place (the URL stays put).
The dock header's "Full view" control ([data-testid=ask-dock-full]) promotes
the in-dock thread to the routed `/threads/<id>` page.

The demo project ships a single agent — `analytics` (analytics.agentic.yml),
selected by default — since #2346 removed the classic `default` agent, so no
agent-picker step is needed.

```yaml
- act: |
    browser_click [data-testid=ask-oxygen-button] in the top bar. The
    right-side Ask dock ([data-testid=ask-dock]) opens with a composer.

- wait_for: "selector:[data-testid=ask-dock]"

- act: |
    Submit a question via the Ask dock:
    1. browser_type into textarea[name=question] inside
       [data-testid=ask-dock]: "What were the total weekly sales by store?"
    2. browser_click the [data-testid=chat-panel-submit-button] inside
       [data-testid=ask-dock] (scope to the dock — the home fallback has its
       own composer behind it).
    The dock switches to its thread view and the answer streams in place.
```

(Not `cache_scope: shared` today — no two flows submit byte-identical dock
text. Promote to shared if a second flow needs the exact same submit.)

---

## Launcher navigation

The left sidebar is gone — the home launcher is the navigation anchor.
Interior pages carry a TopBar back button (`[data-testid=topbar-back-home]`).
Utility tiles at the bottom of the launcher open list modals or navigate:

| Affordance | Testid | Result |
| --- | --- | --- |
| Threads tile | `utility-tile-threads` | opens `[data-testid=threads-modal]` (rows navigate to `/threads/<id>`) |
| Automations tile | `utility-tile-automations` | opens `[data-testid=automations-modal]` (rows navigate to `/workflows/<pathb64>`) |
| Studio tile | `utility-tile-studio` | navigates to `/ide` |

There is no launcher affordance to `.app.yml` Data Apps (the launcher grid
shows published **custom** apps only); reach a Data App by direct
`browser_navigate` to `/apps/<base64>` in local mode, or via the
onboarding completion screen's `onboarding-explore-app-<index>` buttons
in cloud mode (workspace-prefixed URLs embed a per-run UUID, so recorded
absolute navigations would break warm replay).

### Open the Threads modal from the launcher

```yaml
- act: |
    browser_click [data-testid=utility-tile-threads]. The Threads
    list modal ([data-testid=threads-modal]) opens.
  cache_scope: shared
```

### Open the Automations modal from the launcher

```yaml
- act: |
    browser_click [data-testid=utility-tile-automations]. The
    Automations list modal ([data-testid=automations-modal]) opens.
  cache_scope: shared
```

### Promote the thread drawer to the routed thread page

```yaml
- act: |
    browser_click [data-testid=thread-drawer-open-full]. The drawer
    closes and the URL changes to /threads/<id> — the full thread
    page with a TopBar back button.
  cache_scope: shared
```

---

## When NOT to use shared scope

- **Body varies between flows.** If your prompt has any flow-specific
  content (workspace name, custom selector, app name), do not paste it
  here verbatim — diverging text invalidates the shared key for everyone.
- **Step depends on prior state.** A canonical "click submit" only works
  if every flow that uses it arrived at the same page state through the
  same prelude. If your flow has a different prefix, the cached actions
  may not replay successfully.
- **First implementation.** Land the flow with `cache_scope: flow`
  (default), confirm the recording is stable, then promote to `shared`
  in a follow-up commit.

## Promoting a step to shared

If you have a flow-private recording that you want to share:

1. Move the prompt verbatim into this file.
2. Add `cache_scope: shared` to your flow's step.
3. Re-run the flow once cold. The new shared key gets recorded.
4. Subsequent runs across all flows referencing the same prompt hit the
   shared entry.
