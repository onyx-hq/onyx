/**
 * `oxyc guide` — teach an agent this tool, in one command, in any harness.
 *
 * THE GAP THIS FILLS. There are already three ways an LLM can learn `oxyc`,
 * and each reaches a different audience:
 *
 *   1. the tool describes itself — `--help`, `routes`, `schema`, `exit-codes`.
 *      Works for ANY agent with no setup, and is the foundation. But it is
 *      pull-based: the agent has to already suspect `oxyc` is the right tool.
 *   2. the bundled Claude skill (`oxyc skills install`). Rich, but Claude-only
 *      and behind an install step.
 *   3. error messages that name the next command. The highest-bandwidth
 *      channel, because it arrives exactly when the agent is stuck — but only
 *      once it is already stuck.
 *
 * None of them puts "here is what this tool is for" into an agent's context
 * BEFORE it needs it, in a harness-agnostic way. That is what this is: a
 * compact page a human pastes into `AGENTS.md`, `CLAUDE.md`, `.cursorrules`,
 * a system prompt, or whatever their agent reads.
 *
 * COMPLEMENTS `oxyc mcp` rather than competing with it. MCP reaches an agent
 * whose runtime can launch a server; this reaches every other one — a plain
 * shell, a CI job, a harness with no MCP support — and it costs nothing per
 * turn beyond the lines it occupies.
 *
 * KEPT SHORT ON PURPOSE. Something that lands in a context window on every
 * turn has to earn each line; the detail lives behind `routes` and `schema`.
 */

import { basename, dirname, extname, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { stdoutIsTty } from "../ui/tty.js";

/**
 * How to invoke THIS binary, resolved at runtime.
 *
 * The guide is a page a human pastes verbatim into `AGENTS.md`, so a `<repo>`
 * placeholder is a line an agent runs as-is and gets
 * `Cannot find module '<repo>/…'`. Until the package is published there is no
 * short spelling, so the honest one is the absolute path this process was
 * started from — which the binary knows.
 *
 * `process.argv[1]` is that path, and after a global install it is the
 * `bin/oxyc` symlink rather than `dist/main.mjs`. `node <symlink>` works on
 * POSIX because npm links the JS file itself, so the line stays runnable —
 * it just will not be the path the prose above it describes.
 */
function selfInvocation(): string {
  // A compiled single-file binary IS the executable: there is no `node` and no
  // `dist/main.mjs`. Printing one would hand an agent a path that does not
  // exist, which is the one thing this page cannot afford to do.
  const runner = basename(process.execPath, extname(process.execPath));
  if (runner !== "node" && runner !== "bun") return "oxyc";

  const self = fileURLToPath(import.meta.url);
  // From `dist/main.mjs` at runtime; from `src/commands/guide.ts` under vitest.
  const dist = self.includes(`${sep}dist${sep}`)
    ? process.argv[1] || self
    : resolve(dirname(self), "..", "..", "dist", "main.mjs");
  return `node ${dist}`;
}

const GUIDE = () => `## oxyc — the Oxy CLI

Authenticated HTTP client for the Oxy platform (\`gh api\`-shaped), plus the
tooling that scopes work to one customer. Use it to get real data out of a
deployment: query a customer's warehouse or semantic layer, read threads, runs
and apps, or reproduce a reported bug against live data.

    ${selfInvocation()} <command>

If your runtime speaks MCP, \`oxyc mcp\` serves the same surface as four tools
(\`oxy_routes\`, \`oxy_schema\`, \`oxy_request\`, \`oxy_whoami\`).

### Never guess a path

    oxyc routes <filter>           # what exists, and what each endpoint does
    oxyc schema <path> [-X POST]   # the body it expects
    oxyc api <path> [flags]        # call it

### Getting data out

    oxyc api orgs --md                                  # org ids
    oxyc api {org}/workspaces --md                      # workspace ids
    oxyc api {workspace}/databases --jq '.[].name'      # connection names
    oxyc api {workspace}/sql/query -f 'sql=select 1' -f database=<name> --md

\`{org}\` \`{workspace}\` \`{project}\` \`{customer}\` \`{me}\` fill themselves from the
customer repo you are in, or from \`--org\` / \`--workspace\` / \`--project\`. An
unresolved one errors and names the flag that would fill it.

### Flags worth knowing

    -f k=v / -F k=v   string / JSON-typed field    --input @file   raw body
    --jq '<expr>'     filter server-side shape     --md            markdown table
    --paginate        walk every page              --cache 5m      reuse a recent GET
    --env local|dev|staging|production, or paste a URL

Prefer \`--jq\` and \`--md\` before reading a large response: \`--md\` is far fewer
tokens than the same rows as JSON, which repeats every field name per row.

### Branch on the exit code

    0 ok · 1 it ran and found problems · 2 you called it wrong, stop
    4 log in (\`oxyc login --env <env>\`) · 5 not found · 6 malformed request
    7 retryable (5xx/timeout) · 8 refused

### Beyond the API

    oxyc validate                  # check the workspace YAML — no network, no token
    oxyc proxy --env dev           # local app dev against cloud data
    oxyc publish --env dev         # build + publish a custom app (draft; --promote for live)
    oxyc <customer>                # a session scoped to one customer
    oxyc doctor <customer>         # what the tool knows, changing nothing
    oxyc assume status             # a staff 403 usually means no session, not a role
    oxyc assume start --org <o> -r "why"   # 60 min, not renewable

\`oxyc validate\` is the only one that works entirely offline. It is STRUCTURAL —
\`oxy validate\` also resolves \`databases:\` and \`llm.ref\` and wins where they differ.

### Traps

- \`200\` with a body of \`null\` can mean an EXPIRED SESSION, not "no such
  thing" — \`/api/user\` does exactly that. \`oxyc whoami\` tells them apart.
- \`/sql/query\` returns arrays of strings, HEADER ROW FIRST —
  \`[["id","name"],["1","ada"]]\` — not an object. \`--md\` renders it.
- \`oxyc schema\` covers the data plane only. Blank means undocumented, not
  nonexistent; \`oxyc routes <path>\` confirms the endpoint is real.
- A listed route can still 404 if it is \`ide-only\`; \`oxyc routes --all\` shows those.
- Read freely. Ask before running a mutating request against production.
`;

/**
 * Print the guide.
 *
 * Markdown either way — it is meant to be pasted into a file, and a terminal
 * reader is going to copy it rather than read it in place.
 */
export function runGuide(): void {
  process.stdout.write(GUIDE());
  if (stdoutIsTty()) {
    process.stderr.write("\nPaste that into AGENTS.md / CLAUDE.md, or: oxyc guide >> AGENTS.md\n");
  }
}
