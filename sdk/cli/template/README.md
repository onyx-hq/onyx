# __NAME__

The oxy workspace for **__NAME__** — everything we build for this customer in
one repo: their semantic model, pipelines, automations, custom apps, and the
agent memory the team shares.

Start a session scoped to this customer with:

    oxyc __SLUG__

## Layout

| Path | What goes here |
| --- | --- |
| `memory/` | Agent memory, written by Claude Code during `oxyc` sessions. `MEMORY.md` is **generated** from the frontmatter of the fact files beside it — do not edit it by hand; regenerate on conflict. |
| `semantics/views/` | `*.view.yml` — one per table: entities, dimensions, measures. The `description` is a contract with the agent, not a label. |
| `semantics/topics/` | `*.topic.yml` — question areas: a base view plus the views it may join to. Not one per table. |
| `pipelines/` | `*.airway.yml` — Airway ELT pipelines. Credentials live in the secret manager, never in the YAML. |
| `workflows/` | `*.automation.yml` — automations. `.automation.yml` is the canonical extension; `.procedure.yml` is legacy and still accepted, so do not start new files with it. |
| `apps/` | Custom app bundles (React + Vite, shipped with `oxyc publish`). |
| `config.yml` | Workspace root config — models, databases, defaults. |
| `.github/workflows/` | GitHub Actions CI for this repo. Not the same thing as `workflows/` above, which holds oxy automations — the two are one letter of context apart and easy to confuse. |
| `.github/scripts/` | Shell the workflows call. Kept out of the YAML because a `run:` block can only ever be executed on a runner, while a script can be run — and tested — anywhere. |
| `scripts/` | Shell **you** run. `dev.sh` is the local dev entry point — see "Developing an app locally" below; `pnpm dev` is how you invoke it. |
| `.oxyc-managed` | Which files here are the tooling's and which are yours, so `oxyc update` can refresh one without overwriting the other. It is the only thing that decides what a sync may rewrite. Add a line when you add a file to the template — not when you add a view or an app. |
| `.gitignore` | What never gets committed: `node_modules/`, build output (`out/`, `dist/`, `build/`), `.oxy_state/`, `.env*`. The same directory names CI prunes when it walks the tree. |

The scaffold ships empty directories on purpose. There are no example views or
pipelines to copy: a stale example carried forward is how a whole workspace
ends up on a deprecated file extension.

## What commits itself, and what does not

A session started with `oxyc` regenerates the memory index and then commits and
pushes **`memory/` only**, at the end of the session, with no review step.

Everything else here — the semantic model, pipelines, automations, apps — you
commit yourself, deliberately, after review. That split is deliberate: a
customer's semantic model should never land in `main` because a session ended.

The practical consequence: do not put anything in `memory/` that needs review
before the team sees it.

## What CI checks

Every pull request runs `.github/workflows/validate.yaml`: a Postgres 16
service container, then `oxy compile` over this whole tree. It needs no
warehouse credentials — compiling is a pure YAML-to-rows transform — so the
gate works before a single database is connected.

It catches:

- **YAML that does not parse**, anywhere in the tree, `config.yml` included.
  The failing path is emitted as a GitHub error annotation, so the failure
  lands on the diff rather than only in the log. It anchors to the **top of
  the file**, not to the offending line: `oxy compile --json` reports a path, a
  kind and a message per failure, and no line or column for the workflow to
  pass on.
- **Two files of one kind declaring the same `name:`** — two views both called
  `orders`, say, which is what a copy-pasted file usually produces.

It does **not** catch, and each of these compiles green today:

- **A view whose `datasource:` names a database that is not in `config.yml`.**
  There is no referential check at compile time; the typo surfaces at query
  time instead.
- **A view with no `name:` field at all.** The compiler derives one from the
  file path rather than failing, so the view exists under a name nobody chose.
- **Anything about whether the model is right** — a measure with the wrong SQL,
  a join on the wrong key, a description that misdescribes the column. Nothing
  mechanical checks those.

So a green check means "this parses, and the identifiers are unique". It does
not mean the model is correct, and it is not a substitute for reading the diff.

The `oxy` CLI version is **pinned** in that workflow, and the job fails if the
installed binary is older. Both halves matter: the installer does not fail on a
version tag that does not exist, and this CLI answers a subcommand it does not
have with its own help text and exit 0 — so an unpinned, unguarded job can go
green while doing less than you think.

## Developing an app locally

An app in `apps/` is a React + Vite bundle. Running one on your machine means
running it against **a real oxy in the cloud** — there is no local warehouse,
no fixture data, and no local `oxy serve` to point at. From the repo root:

```bash
oxyc login --env dev     # once per env; opens a browser, caches a token
pnpm dev --env dev       # starts everything
```

`pnpm dev` runs `scripts/dev.sh`, and that script is the whole answer: it finds
the apps in this repo, starts both processes, prints what it is doing, and
stops both together when you press Ctrl-C.

**`--env` is required, and that is deliberate.** It says which oxy the app
reads from — `dev`, `staging`, `production`, a full URL
(`--env https://acme.oxygen-hq.com`), or any name the app's own `oxy-app.json`
declares under `environments`. The underlying `oxyc proxy` defaults to
**production** if nothing says otherwise, and a `pnpm dev` that quietly serves
a customer's live data is not something this repo will do. `--env production`
additionally needs `--yes`.

If the repo holds more than one app, name the one you want — by its directory
or just its name:

```bash
pnpm dev --env dev sales-dashboard
```

### The two processes

| Process | Port | What it is |
| --- | --- | --- |
| `oxyc proxy --env <env>` | `3000` | Background. Every `/api` call the app makes goes through here, signed with the token `oxyc login` cached, and out to the cloud. It stands in for a local `oxy serve`, which is why the port is not configurable — the Vite plugin already proxies `/api` to `3000`. Each Oxy Function call prints a `↳ <status> fn <name>  request_id=…  trace_id=…` line — the ids to look up when one fails. |
| `pnpm dev` in the app | `5173` | Foreground. Vite, with hot reload. **This is the URL you open**: <http://localhost:5173>. |

Ctrl-C stops both, and it always returns: a proxy that ignores the polite
signal is given five seconds and then killed outright, because a dev script
whose Ctrl-C sometimes hangs is worse than one that leaks. If a run ever fails
with *port 3000 is already taken*, a proxy from an earlier session outlived its
terminal — the message names the process holding it.

### The guardrails, and how to lift them

The proxy is not a transparent tunnel. Two kinds of traffic are held back so
that an afternoon of local clicking cannot change what the customer sees:

- **Side-effecting calls are HELD** — Oxy Functions (`/fn`), agent runs,
  automation runs. They do not reach the cloud at all. This is the single most
  common "my function isn't running and there is no error" — pass
  **`--allow-writes`** to forward them for real.
- **Tracking events are DROPPED**, so local clicking never lands in the
  customer's analytics. Pass **`--allow-events`** to forward them.

`pnpm dev` prints both, every run, before it starts anything.

### If something is wrong

- **Requests come back 401** — the cached token for that env has expired.
  `oxyc login --env <env>` again.
- **`the 'oxyc' CLI is not on your PATH`** — install it with
  `npm install -g @oxy-hq/cli`. A global install, not `npx`: Ctrl-C stops the
  proxy by pid, and a wrapper process in between keeps the signal from reaching
  it.
- **`this repo has no custom apps yet`** — expected on a fresh repo, and it is
  not an error. The message says how to add one.
- **A change to `apps/` does not show up** — Vite serves the app, but the data
  comes from the cloud through the proxy; a stale *answer* is a semantic-model
  or warehouse question, not a dev-server one.

## Publishing custom apps

**Publishing is self-serve here, and CI publishes nothing unless this repo
turns it on.** That is the default deliberately: it is how Oxy ships its own
custom apps, and it means this repo is useful on the day it is created without
a long-lived credential sitting in it.

### The default: you publish, from your machine

From the app's own directory under `apps/`:

```bash
oxyc login --env dev       # once per env; opens a browser, caches a token
oxyc publish --env dev     # a draft, to the dev environment
```

`--env` is not optional in practice. The CLI defaults it to **production**, so
an invocation that leaves the flag off does not publish "nowhere in
particular" — it publishes at the customer's live environment. Name it every
time. The draft channel is the default; `--promote` publishes live instead.

**A push to `main` still runs `.github/workflows/publish.yaml`, and it is
still worth having.** Its `build` job installs the workspace and builds every
bundle under `apps/` — which needs no credential — so a bundle that stopped
compiling fails on the merge, rather than in front of you an hour later when
you sit down to publish it. The `publish` job is then **skipped**, the run is
**green**, and the log carries a notice saying publishing here is self-serve
and naming the two commands above.

### Turning CI publishing on

One repository **variable**:

> Settings → Secrets and variables → Actions → **Variables** →
> `OXY_CI_PUBLISH` = `true`

A variable rather than a secret, for two reasons. It is read in the `build`
job — the only job that runs when publishing is off, and so the only one that
can say anything about it — and that job must reference no secret at all,
because it executes the postinstall and build scripts of every dependency your
apps pull in. The other reason is plainer: a switch whose whole job is to tell
"off on purpose" apart from "misconfigured" has to be legible, in Settings and
in a run log. A secret is neither.

`true`, `yes`, `on` and `1` all mean on; `false`, `no`, `off`, `0` and a
variable that is not set at all mean off. **Anything else fails the run**
instead of being read as one of them: `OXY_CI_PUBLISH=ture` guessed as off
stops publishing with nobody told, and guessed as on publishes when nobody
asked, so it is not guessed.

Note what the switch does **not** do. It decides whether the publish job runs;
it never softens what happens once it has. With CI publishing on, a missing
`OXY_TOKEN` is a **hard failure** exactly as it always was. That is the reason
there is a switch at all rather than the obvious shortcut of skipping whenever
no token turns up: inferred from the token, "this repo does not publish from
CI" and "somebody forgot production's token" are the same observation — and
the second one ships nothing, quietly, for as long as nobody looks.

Everything from here to the end of this section describes a repo that has
opted in.

### Where an opted-in CI publishes

`.github/workflows/publish.yaml` builds every bundle under `apps/` and ships
it with `oxyc publish`. It runs on a push to `main` that touches `apps/`, the
lockfile, or the workflow itself, and it can be run by hand from the Actions
tab.

**Where it publishes is decided by how it was triggered, and it is always
said out loud:**

| Trigger | Event | `oxyc publish --env` | Channel | Reviewers |
| --- | --- | --- | --- | --- |
| push to `main` | `push` | `dev` | draft | none |
| Actions tab (**Run workflow**) | `workflow_dispatch` | `production` | draft, or live with **Promote** | whatever `production` requires |

The **Event** column is the raw GitHub event name, and it is here rather than
implied because it is the key the mapping is written against — this table and
`.github/scripts/publish-env.sh` are checked against each other, row by row,
by `customer-tooling`'s own suite.

That mapping lives in `.github/scripts/publish-env.sh`, and a trigger it has
no entry for **fails the run** rather than picking one. The reason is worth
knowing, because it was a live bug in this workflow: `oxyc publish` defaults
`--env` to **production**, so an invocation that leaves the flag off does not
publish "nowhere in particular" — it publishes at the customer's live
environment. Every publish here names its environment explicitly, and there
is no code path that can fall back to a default.

There is deliberately **no environment picker** on the manual run. An input
would need a default, which is the thing being removed; and the same resolved
name also selects the GitHub environment whose `OXY_TOKEN` the job publishes
with, so leaving it to a form field would mean the credential and the
destination agree only by an operator's care. To publish a dev draft by hand,
re-run the push-triggered run from the Actions tab — a re-run keeps the
original event, so it still resolves to `dev`.

It is **two jobs**, and the split is the security boundary rather than a
tidiness one. `build` runs `pnpm install` and `pnpm -r build` — which is to
say it executes the postinstall and build scripts of every dependency your
apps pull in — and holds no publish credential at all. It hands the built
output to `publish` as a workflow artifact. `publish` holds `OXY_TOKEN`, and
runs no package script: `oxyc publish --dir` uploads a pre-built directory
as-is instead of building anything. So a compromised dependency in an app's
tree never runs on a runner where the token exists.

**On a fresh repo it does nothing at all, and that is the intended
behaviour.** The scaffold ships no apps and no `pnpm-lock.yaml` — there is
nothing to lock yet — so the first step looks for `apps/**/oxy-app.json`,
finds none, skips the install and the build, skips the whole publish job, and
the run goes **green**. The install is skipped along with the publish
deliberately: `pnpm install --frozen-lockfile` fails without a lockfile, and
a repo whose CI is red the day it is created is a repo where nobody reads CI.

Before it can publish anything, each of the two **GitHub environments** —
`dev` and `production` — needs its own **`OXY_TOKEN`** (Settings →
Environments → *environment* → Environment secrets), a publish-scoped API key
minted in oxy against that workspace. This is the credential the self-serve
model does without, and it is worth knowing what it is before you create one:
a personal API key records the project it was minted against, but that project
never reaches validation — a leaked key carries the minter's **user-level**
access, not "this app's project only".

**A missing one fails the run, loudly, and that is the intended behaviour.**
It is also why CI publishing is a switch rather than something inferred from
the token's presence: with the switch on, no token means a red run naming the
environment it was looked for in, never a green run that published nothing.

**And any repository-level `OXY_TOKEN` must be DELETED, not left in place.**
GitHub's order is environment → repository → organisation, and that is
*precedence, not exclusion*: an environment secret overrides a repository one
of the same name, it does not suppress it. A repo still carrying an old
repository-level token therefore serves **both** environments from it wherever
the environment has none — every run looks healthy, the missing-token check
never fires, and production ships under whatever that token was scoped to.

Nothing in CI can catch it. GitHub does not tell a job which level a secret
came from, so the two cases are identical from inside the run. It is a
one-time human check, and it is the single thing to get right when moving an
existing repo onto environments:

```bash
gh secret list --repo <owner>/<repo>              # repository level
gh secret delete OXY_TOKEN --repo <owner>/<repo>  # if one is listed
```

Nothing else needs configuring: each app's `oxy-app.json` carries its own
`slug` and `orgSlug`, and the project is resolved from the target at publish
time. If an environment's oxy is not where `--env` would resolve it, set
`OXY_TARGET` as an **environment** variable on that environment — never as a
repository one, since it *overrides* `--env` and a repository-level value
points both environments at the same oxy.

So adding the first app is two things — three, if this repo publishes from CI:

1. the bundle at `apps/<app>/` (or `apps/<org>/<app>/`), with an
   `oxy-app.json` carrying `slug` and `orgSlug`;
2. a committed `pnpm-lock.yaml` — run `pnpm install` once and commit the
   lockfile it writes. Only the lockfile: `node_modules/` and every app's
   build output are covered by `.gitignore`, so `git add -A` after an install
   stages the one file CI needs and none of the ones it does not;
3. **only with `OXY_CI_PUBLISH` set** — an `OXY_TOKEN` in each environment.

Miss one and the workflow fails loudly and names it: the discovery step
points at the manifest, or the missing lockfile, or the missing secret. It
does not guess and it does not half-publish. (The first two are needed even
with CI publishing off: the `build` job installs and builds either way.)

A push publishes to the **draft** channel of the **dev** environment — draft
is the CLI's own default, not a policy this workflow invented; dev is this
repo's. Going live in production is a separate, deliberate act, and it is
worth knowing exactly what the two ways of doing it are:

- **`/admin/apps`** promotes *the build you are looking at*. If you have
  reviewed a draft and want that exact bundle live, this is the one you want.
- **The Actions tab**, running this workflow with a **Ref** and **Promote**
  ticked, **rebuilds that ref and publishes the result live**. It does not
  promote an existing draft — `oxyc publish` cannot, so the workflow cannot
  either. If `main` has moved since the draft you reviewed,
  this ships the newer commit, under a new build id.

That is why the ref is an explicit, required input rather than something the
run infers: pin it to the SHA you reviewed and the two routes agree. Leaving
it at its default, `main`, is fine and publishes main's current tip — the run
resolves the commit it actually checked out and prints it, so what shipped is
never left to inference. Do not read the commit off the run's title or the
branch you launched it from: on a manual run those track GitHub's "Use
workflow from" dropdown, which is a separate control from the Ref input here.

### Putting a human in front of a publish

The publish job runs in the GitHub Actions **environment** it is publishing
to — `dev` or `production`, the same string it passes to `--env`. On a fresh
repo those are labels and nothing else: GitHub creates a referenced
environment that does not exist, with no protection rules. (Environments in a
private repo need GitHub Team or Pro; the `oxy-hq` org is on Team.)

Either becomes a gate when someone wants one — Settings → Environments →
`production` → **Required reviewers** — and no workflow edit is needed to add
it. Because the trigger already decides the environment, reviewers on
`production` hold up exactly the manual runs and none of the dev drafts a
merge produces. Putting them on `dev` instead would hold up every merge, which
is almost never what anyone means.

It does **not** run tests, typecheck, or check that an app works. If the
build produces output, the bundle ships. One case is worth knowing about
because it is silent everywhere else: an app whose `package.json` has no
`build` script is skipped by pnpm **without failing**, so the workflow
refuses to publish an empty output directory rather than uploading nothing
successfully. Nothing catches a bundle that builds and is simply wrong.

## This repo is the customer's oxy workspace

The semantic model in here does nothing until the platform compiles it, so this
repo is registered as __NAME__'s oxy workspace — **twice**, once as dev and
once as production. Both track this repo and this same `main`.

Registration happens through the **org onboarding UI** and needs a GitHub App
installation for the org. There is no CLI command for it — do not go looking for
one. Registration supports pointing at a subdirectory, so the workspace root can
sit inside a larger repo if that is ever needed.

Until it is registered, the model here is still worth writing and still worth
validating; it just is not queryable by agents or Ask Oxygen yet.

### Two workspaces, one branch — and who promotes what

There is no `dev` branch, and that is the design rather than an omission. A
semantic model is not application code: a long-lived divergent branch means a
measure exists in one environment and not the other, and the symptom is a
query that worked yesterday quietly returning nothing. One definition, two
pointers at it.

It works because **merging to `main` promotes nothing on its own** — the
platform compiles on an explicit action (the IDE button, admin's *Run compile
now*, the CLI) and never on a push. So the two workspaces differ only in when
someone points each at the branch:

| What | Who moves it |
| --- | --- |
| Custom apps → dev | CI, on every merge to `main` (draft) |
| Custom apps → production | CI, on a manual run from the Actions tab |
| Semantic model → dev | a human, or a scheduled compile if one is set up |
| **Semantic model → production** | **a human, always** |

Promoting the production semantic model is a deliberate, human step: open the
production workspace in Oxygen and pull the latest commit. **CI does not do
it**, and holds no credential that could — the `OXY_TOKEN` in each environment
is publish-scoped and cannot compile or promote a workspace at all. So a merge
to `main` never changes what production answers until someone decides it
should.

## Node and pnpm

Node **20** (`.nvmrc`), matching the working app monorepo this build config is
derived from. Use **pnpm**, never npm or yarn — there is no `package-lock.json`
here and there should not be one.

pnpm is floored at **>= 10**, one major above that monorepo's `>= 9`, and the
difference is deliberate: pnpm 9 reads no settings at all from
`pnpm-workspace.yaml`, so on pnpm 9 the `onlyBuiltDependencies` block there is
silently inert and an app's `esbuild` / `sharp` postinstall is blocked with no
diagnostic. A floor that admits a version where the config does nothing is
worse than no floor.
