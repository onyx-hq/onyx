# Project Guidelines for Claude

## Package Manager

This project uses **pnpm**. Always use `pnpm` — never `npm` or `yarn`.

```bash
pnpm add <package>       # install a dependency
pnpm add -D <package>    # install a dev dependency
pnpm remove <package>    # remove a dependency
pnpm install             # install all dependencies
```

## Tech Stack

- **Framework:** React 19 + Vite + TypeScript
- **Styling:** Tailwind CSS v4
- **UI Components:** shadcn/ui
- **Routing:** React Router Dom v7
- **Data Fetching:** TanStack React Query v5
- **API Client:** Axios (`./src/services/api/axios.ts`) with JWT access token interceptors
- **Forms:** React Hook Form v7 + Zod v4
- **Global State:** Zustand v5 (`web-app/src/stores`)
- **Charts:** Echarts
- **Icons:** Lucide React
- **Toasts:** Sonner

## Common Commands

```bash
pnpm dev                 # start dev server (Vite)
pnpm build               # tsc + vite build
pnpm lint                # ESLint
pnpm preview             # preview production build
```

## Project Structure

```
src/
├── assets/          # Static assets
├── components/      # Shared cross-feature components
│   ├── ui/          # Purely presentational; ui/shadcn/ is CLI-managed
│   └── ...          # Feature-scoped shared components (Chat, Markdown, etc.)
├── contexts/        # React context providers
├── hooks/
│   ├── api/         # TanStack Query hooks, one subfolder per domain
│   │   └── queryKey.ts  # All query keys defined here
│   ├── auth/
│   ├── messaging/   # SSE streaming hooks
│   └── workflow/
├── lib/             # Utilities (cn, schemas, etc.)
├── libs/            # Third-party integration helpers
├── pages/           # Route-level page components
│   ├── launcher/    # Home launcher (apps-first anchor: app cards, modals, recents)
│   ├── thread/      # Thread detail page
│   ├── threads/     # Threads list
│   ├── ide/         # Developer Portal (Monaco IDE + observability + settings)
│   ├── workflow/    # Workflow page
│   ├── app/         # Data App page
│   ├── context-graph/
│   ├── workspaces/
│   └── ...
├── services/
│   └── api/         # Axios client + domain API functions (one file per domain)
├── stores/          # Zustand global stores
├── styles/          # Global CSS + shadcn theme tokens
├── types/           # Shared TypeScript types
├── utils/           # Pure utility functions
└── App.tsx          # Router (createBrowserRouter)
```

## Naming Conventions

- PascalCase for React components, hooks, and folders containing components (e.g., `ProjectsListPage`, `useProjects`, `Projects/`).
- camelCase for functions, variables, and non-component exports (e.g., `fetchProjects`, `projectId`).
- UPPER_SNAKE_CASE for constants (e.g., `API_BASE_URL`).
- kebab-case for file names of simple pages without sub-components (e.g., `login/index.tsx`).

## Folder Structure Best Practices

- **Simple pages** (single file, no sub-components): flat kebab-case file under `src/pages/` (e.g., `login/index.tsx`).
- **Complex features** (multiple pages sharing sub-components): dedicated PascalCase folder with a `components/` subfolder (e.g., `pages/projects/`). Sub-components shared across pages in the feature go here.
- **Complex pages** (a single page with its own sub-components): the page becomes a PascalCase folder with `index.tsx` as the entry point and a `components/` subfolder for page-only sub-components.
- **Shared components:** cross-feature reusables go in `src/components/`. If the component is purely presentational (no side effects, no data fetching, no business logic), place it in `src/components/ui/` instead.

- **Colocation:** sub-components, hooks, constants, and utils live at the level of the component that uses them — not bubbled up. This applies **recursively** at every level, not just pages.

- **When a component grows sub-components:** it becomes a PascalCase folder with `index.tsx` as the component itself and a `components/` subfolder for its children. If a child also grows sub-components, apply the same pattern recursively. Same for hooks, `constants.ts`, `utils.ts` — they live beside the component that owns them.

- Sub-components shared between sibling pages in the same feature live in the feature’s `components/` folder.

**Example — recursive colocation:**

```
pages/projects/
├── ProjectsListPage/
│   ├── index.tsx                  ← the page component
│   ├── constants.ts               ← constants only used by ProjectsListPage
│   ├── useProjectsList.ts         ← hook only used by ProjectsListPage
│   └── components/
│       ├── ProjectItem/
│       │   ├── index.tsx          ← ProjectItem component
│       │   └── components/
│       │       ├── Actions/
│       │       │   ├── index.tsx  ← Actions component (grew large)
│       │       │   └── components/
│       │       │       ├── DeleteAction.tsx
│       │       │       └── EditAction.tsx
│       │       ├── ProjectBadge.tsx
│       │       └── ProjectMeta.tsx
│       ├── ProjectsTableHeader.tsx
│       ├── ProjectsPagination.tsx
│       └── CreateProjectDialog.tsx
├── ProjectDetailPage.tsx
└── components/
    └── EditProjectDialog.tsx      ← shared by ProjectsListPage + ProjectDetailPage
```

## React Conventions

- **Functional components only.** Do not use class components.
- **Single responsibility.** Keep components small and focused; extract sub-components when a component grows.
- **Local state first.** Use `useState`/`useReducer` for component-local state.
- **Global state (Zustand): requires approval.** The existing stores (`auth.store`, `theme.store`) cover auth tokens and theme. Before adding a new store or expanding an existing one, warn the user and get explicit approval. Prefer React Query cache or URL state for server/UI state.
- **Custom hooks:** extract reusable logic into `src/hooks/' when it’s shared across components or represents a distinct concern (e.g., data fetching, form handling, permission checks).

### When to extract a sub-component

Extract when **any** of these are true:

- The JSX block represents a distinct, named UI concept (e.g., a table row, a card, a dialog) — give it its own file.
- The block is complex enough that you need to scroll past it to understand the parent component.
- The same JSX structure appears in more than one place.
- A component file exceeds ~150 lines — treat this as a signal to review responsibilities.

Do **not** extract just to reduce line count. A 30-line block that only exists in one place and has no distinct identity should stay inline.

### When to extract a custom hook

Extract when **any** of these are true:

- A component contains `useQuery` or `useMutation` alongside rendering logic — data fetching always goes into a hook.
- A component has 3+ related `useState` calls that represent one concern (e.g., pagination state, filter state).
- The same query or state logic is duplicated across two components.

Keep in the component: dialog open/close state, form state, UI-only toggles that don’t involve data fetching.

**Where to put the hook:**

- Used by one page only → co-locate inside that page’s folder (e.g., `ProjectsListPage/useProjectsList.ts`)
- Used by multiple components → `src/hooks/`

## API Layer

- All HTTP calls go through the shared `apiClient` from `src/services/api/axios.ts` — do not create additional Axios instances unless for a clearly separate service (e.g., `vibeCodingClient` for the vibe coding backend).
- Group API functions by domain in `src/services/api/<domain>.ts` (e.g., `projects.ts`, `auth.ts`).
- The `axios.ts` interceptor handles JWT attachment from `localStorage` and redirects to `/login` on 401 — do not duplicate this logic.
- Use **TanStack React Query** for all server state (fetching, caching, mutations). Do not store server data in Zustand or component state.
- All hooks that call the API (`useQuery`, `useMutation`, `useInfiniteQuery`) must live in `src/hooks/api/`. Do not place them in pages, components, or other hook folders.
- Every `useQuery` / `useInfiniteQuery` hook must use a `queryKey` defined in `src/hooks/api/queryKey.ts` — never use ad-hoc inline arrays. Add new keys to that file before creating the hook.

## Routing

- All routes are defined in `src/App.tsx` using `createBrowserRouter`.
- Access control: `<ProtectedRoute>` wraps all app routes when `authConfig.auth_enabled` is true. Enterprise-only routes (e.g. Observability) are gated via `authConfig.enterprise` conditionals in `MainLayout` — there is no `<AdminRoute>` component.
- Do not lazy-load pages unless bundle size becomes a concern — current setup uses direct imports.

## Forms

- Use **React Hook Form** for all form state.
- Define validation schemas with **Zod** in `src/lib/schemas.ts` (shared) or co-located with the form.
- Connect via `@hookform/resolvers/zod`.

## UI Components

- **Always check shadcn/ui first** before creating any new component — do not build a custom component that shadcn already provides. Browse https://ui.shadcn.com/docs/components to verify availability. If shadcn has it, use it.
- Add new shadcn components via CLI: `pnpm dlx shadcn@latest add <component>` — do not copy files manually.
- Do not edit files under `src/components/ui/shadcn` manually; they are managed by the shadcn CLI.

## Current Project & Branch

Use `useCurrentProjectBranch()` from `src/hooks/useCurrentProjectBranch.ts` to get the active project and branch context. **Must be called inside the IDE route** (it depends on `useIDE()` context); it throws if no project is selected.

```ts
const { project, branchName, isMainEditMode, gitEnabled } = useCurrentProjectBranch();
```

- `project` — the current `Project` object (from `useCurrentProject` store)
- `branchName` — the active branch: IDE-local branch override when inside the IDE, otherwise `project.active_branch`
- `isMainEditMode` — true when local git + remote is configured and the user is on a protected branch (edits are allowed but saving auto-creates a new branch)
- `gitEnabled` — true when local git or cloud GitHub integration is active

If you only need the project (outside the IDE), use `useCurrentProject()` from `src/stores/useCurrentProject.ts` directly.

## Authorization

- Use `useCurrentUser()` from `src/hooks/api/users/useCurrentUser.ts` to get the authenticated user's profile (id, email, name, picture). **`UserInfo` does not carry `role` or `is_admin`** — those are per-org.
- For org-scoped gates, read role from `useCurrentOrg((s) => s.role)` (values: `"owner" | "admin" | "member"`). Treat `"owner"` and `"admin"` as admin-equivalent.
- For workspace-scoped gates, the backend's `EffectiveWorkspaceRole` already handles the org→workspace role resolution — trust server-side 403s instead of duplicating the check in the UI when possible.
- Feature flags from `authConfig` control broader access: `authConfig.enterprise` for enterprise-only features, `authConfig.cloud` for cloud-only features.

## Code Style

- TypeScript strict mode is enabled — avoid `any`; use proper types or `unknown`.
- Prettier and ESLint are configured — run `pnpm lint` before committing.
- Use `cn()` from `src/lib/utils.ts` for conditional Tailwind class merging.
- **No raw color values:** Do not use colors that are not part of the shadcn/ui theme (e.g. `text-red-500`, `bg-[#4554]`, `border-blue-300`). Use semantic CSS variables via Tailwind theme ./src/styles/shadcn tokens (`text-destructive`, `bg-muted`, `border-border`, etc.) or the brand color tokens defined in the design system (`text-primary`, `bg-primary`).
- **No hardcoded sizes:** Do not use arbitrary size values (e.g. `w-[4px]`, `h-[13px]`, `p-[6px]`). Use Tailwind's spacing/sizing scale (`w-1`, `h-3`, `p-1.5`) or design tokens. If a standard scale value truly does not fit, get explicit approval before using an arbitrary value.

## Error Handling

- **API errors:** Handle errors in the React Query layer. Use `onError` in `useMutation` or the `error` field from `useQuery` — do not wrap individual API calls in `try/catch` inside components.
- **User-facing errors:** Surface errors via Sonner toasts (`toast.error(...)`) for transient failures (mutations, background fetches). For blocking failures (page-level load errors), render an inline error state in the component.
- **No silent failures:** Never swallow errors with an empty `catch` block or `catch(() => {})`. At minimum, log with `console.error` in development; prefer surfacing something to the user.
- **Error boundaries:** Before adding an `ErrorBoundary`, ask: "Can this component throw a render-time exception that React Query / normal error state cannot catch, AND would that crash be worse than showing a fallback?" Only add one if both answers are yes.

  **Add an `ErrorBoundary` when all of these are true:**
  - The component wraps a third-party renderer that can throw during render (e.g. ECharts, Monaco, a chart library) — React Query cannot catch render-phase throws.
  - A crash here would silently break the rest of the page (e.g. a dashboard where one broken widget should not kill the others).
  - There is a meaningful fallback UI to show (a proper error block, not a bare `<div>`).

  **Do NOT add an `ErrorBoundary` when:**
  - The failure is an API/network error — use React Query's `error` field and render an inline error state.
  - The component is a normal React component with no third-party render risk — let the app-level Sentry boundary handle unexpected crashes.
  - You would need a bare `<div>Something went wrong</div>` as the fallback — that adds no value over the app-level boundary.

  **Implementation rules (when you do add one):**
  - Use `ErrorBoundary` from `react-error-boundary` (not `@/sentry`) for component-level isolation.
  - Always pass a proper fallback component (look for an existing `*Error` component in the same folder, or create one).
  - Pass `resetKeys={[data]}` so the boundary resets automatically when its input data changes.
- **Type-safe error handling:** When catching errors, type them as `unknown` and narrow with `instanceof Error` or a typed API error helper before accessing `.message`. Never cast caught errors as `any`.
- **Async/await in event handlers:** Wrap `async` event handlers in `try/catch` only when the function is not already managed by React Query. If using `mutateAsync`, wrap the call site; if using `mutate`, rely on the `onError` callback instead.
- **Validation errors:** Use Zod schema validation for form inputs. Surface field-level errors through React Hook Form's `formState.errors` — do not manually set error state alongside RHF.

## Agentic Browser Tests

Cross-cutting UI flows are tested by `web-app/tests/agentic/`. Treat this as a regression-test surface for any change that touches:

- Chat panel / launcher / Ask surfaces (`pages/launcher/**`, `components/Ask/**`, `components/Chat/**`) → `flows/chat-*.flow.test.yml`, `flows/launcher-*.flow.test.yml`
- IDE (`pages/ide/**`) → `flows/ide-*.flow.test.yml`
- Builder dialog (`components/BuilderDialog/**`) → `flows/builder-*.flow.test.yml`
- Onboarding (`pages/onboarding/**`, `components/workspaces/**`) → no flow today (`onboarding-blank-workspace` went with self-serve org creation); add one with `/test-feature` when you change these
- Semantic model + agentic analytics integration (`pages/thread/analytics/**`, `pages/ide/Objects/SemanticLayer/**`) → `flows/semantic-*.flow.test.yml`

**Skill-driven workflow (preferred for authoring + triage):**

The [`agentic-browser-test`](../.claude/skills/agentic-browser-test/SKILL.md) skill handles authoring + maintenance — you don't need to learn the YAML schema, action-cache contract, or healing model directly.

- `/test-feature <description>` — one-shot `.flow.test.yml` generation
- `/agentic-test-add-case <flow> <description>` — extend an existing flow
- `/run-agentic-tests <pattern>` — run with `HEADED=1 DEBUG=1`; runner auto-spawns the right backend
- `/fix-agentic-test <flow-or-bucket>` — triage a failure (Tier-1 / Tier-2 / behavioral / cache-health)
- `/accept-agentic-healing <flow>` — promote staged Tier-2 healing recordings

The skill re-reads the canonical sources (this README, `json-schemas/flow-test.json`, `canonical-prompts.md`, existing flows, `_budgets.yml`) on every invocation, so it stays in sync as the surface evolves.

**Quick commands (raw CLI):**

```bash
pnpm test:agentic --list                  # list flows + cases + tags (no execution)
pnpm test:agentic --dry-run               # validate YAML, run durability lint, preview cost
pnpm test:agentic --inspect-cache         # dump recorded actions from .cache/bespoke-actions.json
pnpm test:agentic --watch <name>          # re-run dry-run on YAML change
pnpm test:agentic <name>                  # actually run a flow (requires ANTHROPIC_API_KEY)
pnpm test:agentic --check-coverage --staged < <(git diff --cached --name-only)
                                          # which flows touch staged paths (advisory)
```

**Selector conventions:**

- Prefer `[data-testid=...]` selectors in `act:` prompts (record best, drift least).
- Fall back to `role=…[name=…]` if no testid is available; use `text=…` only as a last resort.
- The runtime auto-records 2–3 fallback selector strategies per click, so flows survive label tweaks. UI transformations big enough to defeat all fallbacks trigger a healing PR comment for human review (`pnpm test:agentic --accept-healing <flow>` to promote the new recording into `.cache/bespoke-actions.json`).
- When adding a `data-testid` to a component you also test in `tests/agentic/flows/`, follow the `<feature>-<element>` convention and reference it verbatim in the flow's `act:` prompt.
- Specific testids worth knowing about in agentic flows:
  - `app-page-root` — root of `/apps/<id>` page; used as a page-state lock.
  - `agent-selector-button` — chat panel's agent picker trigger.
  - `chat-panel-submit-button` — submit a question from `/` chat panel.
  - `message-input-send-button` / `message-input-stop-button` — thread page input controls; visibility indicates whether a run is in-flight.
  - `builder-dialog-root`, `builder-input-textarea`, `builder-auto-approve-toggle` — Cmd+I builder dialog.
  - `file-change-accept` / `file-change-reject` — per-`edit_file` suspension buttons in the analytics thread view.
  - `reasoning-pill-<label-kebab>` — agentic-analytics reasoning trace artifact pills (e.g. `reasoning-pill-semantic-query`, `reasoning-pill-compile-semantic-query`); clicking opens the right-side artifact sidebar.

**Hard rule (per the 2026-05-06 incident):** never seed/mutate external systems (warehouses, port-forwarded services). The setup-command surface in `tests/agentic/fixtures/reset.ts` is intentionally minimal — `goto:`, `reset_test_file`, and `restore_demo_file:` only, none of which can make a network call. Flows never seed their own data: a cloud-mode flow that needs an org runs against a backend prepared beforehand with `oxy seed` (see `scripts/verify-all.sh`), since no UI creates orgs any more. Any new setup command that would call out is prohibited. See `tests/agentic/README.md` policy section for details.

**Adding a new flow:** prefer `/test-feature <description>` (the skill greps for testids, picks the right primitives, validates against the schema, and adds a `_budgets.yml` entry). For component-driven bootstrapping you can also use `pnpm test:agentic --scaffold <name> --from <component-path>` directly. See `tests/agentic/canonical-prompts.md` for copy-pasteable prelude steps that share cache entries via `cache_scope: shared`.
