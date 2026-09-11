// Non-execution CLI subcommands for `pnpm test:agentic`. Each mode reads
// the flow set, optionally a cache file, and prints to stdout. None of
// them spawn a browser, hit the LLM, or mutate state outside the cache
// inspector's read-only access.
//
// Kept separate from `cli.ts` so the run path stays focused on
// orchestration (backend/frontend lifecycle, runtime selection, result
// reporting) and the file size guidelines stay healthy.

import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import { parse as parseYaml } from "yaml";
import { promoteStaging, readStaging } from "./healing";
import { formatFindings, lintFlow } from "./lint";
import type { FlowTest } from "./types";

interface SurfaceEntry {
  surface: string;
  paths: string[];
  flows: string[];
}

interface ListArgs {
  flows: FlowTest[];
  tag?: string;
}

export function runList({ flows, tag }: ListArgs): void {
  if (flows.length === 0) {
    console.log("no flows discovered.");
    return;
  }
  for (const flow of flows) {
    console.log(`${flow.name}  (${flow.file})`);
    for (const c of flow.cases) {
      const matchesTag = !tag || c.tags.includes(tag);
      if (!matchesTag) continue;
      const tags = c.tags.length > 0 ? `  [${c.tags.join(",")}]` : "";
      console.log(`  - ${c.name}  (${c.steps.length} steps)${tags}`);
    }
  }
}

interface DryRunArgs {
  flows: FlowTest[];
  tag?: string;
}

export function runDryRun({ flows, tag }: DryRunArgs): void {
  if (flows.length === 0) {
    console.error("no flows matched.");
    process.exit(2);
  }

  let totalFindings = 0;
  for (const flow of flows) {
    const findings = lintFlow(flow);
    totalFindings += findings.length;
    const cases = tag ? flow.cases.filter((c) => c.tags.includes(tag)) : flow.cases;
    const stepCount = cases.reduce((s, c) => s + c.steps.length, 0);
    console.log(`${flow.name}  (${flow.file})`);
    console.log(`  cases: ${cases.length},  steps: ${stepCount}`);
    if (findings.length > 0) console.log(`  ${formatFindings(findings).split("\n").join("\n  ")}`);
  }

  console.log("");
  console.log(`Total durability findings: ${totalFindings}`);
  console.log(
    "(Cost preview is heuristic — actual cold runs vary $0.05–$0.40/state-changing step. " +
      "First-run cost ≈ steps × $0.10; warm cost ≈ steps × $0.001.)"
  );
}

interface InspectCacheArgs {
  cachePath: string;
}

export function runInspectCache({ cachePath }: InspectCacheArgs): void {
  if (!existsSync(cachePath)) {
    console.log(`no cache file at ${cachePath}`);
    return;
  }
  let parsed: { entries?: Record<string, unknown> };
  try {
    parsed = JSON.parse(readFileSync(cachePath, "utf-8")) as typeof parsed;
  } catch (err) {
    console.error(`failed to parse cache: ${err instanceof Error ? err.message : String(err)}`);
    process.exit(2);
  }
  const entries = parsed.entries ?? {};
  let count = 0;
  for (const [key, entry] of Object.entries(entries)) {
    const e = entry as { version: number; actions: Array<{ tool: string; args: unknown }> };
    count++;
    console.log(`\n[${key.slice(0, 12)}…]  v${e.version}`);
    for (const a of e.actions) {
      console.log(`  ${a.tool}: ${JSON.stringify(a.args)}`);
    }
  }
  console.log(`\n${count} cache entr${count === 1 ? "y" : "ies"}.`);
}

interface WatchArgs {
  flowsDir: string;
  rerun: () => void;
}

export async function runWatch({ flowsDir, rerun }: WatchArgs): Promise<void> {
  console.log(`[runner] watching ${flowsDir} (Ctrl+C to stop)`);
  rerun();
  const { watch } = await import("node:fs");
  watch(flowsDir, { recursive: true }, (event, filename) => {
    if (filename?.endsWith(".flow.test.yml")) {
      console.log(`\n[runner] ${event}: ${filename}`);
      try {
        rerun();
      } catch (err) {
        console.error(`watch run failed: ${err instanceof Error ? err.message : String(err)}`);
      }
    }
  });
  // fs.watch doesn't keep the event loop alive once the keep-alive ref
  // count drops to zero. The interval is a no-op tick that holds the
  // process open until the user kills it.
  setInterval(() => {}, 1 << 30);
}

interface CheckCoverageArgs {
  surfacesPath: string;
  stagedPaths: string[];
}

export function runCheckCoverage({ surfacesPath, stagedPaths }: CheckCoverageArgs): void {
  if (stagedPaths.length === 0) {
    console.log("[coverage] no staged paths.");
    return;
  }
  const surfaces = loadSurfaces(surfacesPath);

  const hits = new Map<string, Set<string>>();
  for (const staged of stagedPaths) {
    for (const s of surfaces) {
      if (s.paths.some((p) => matchSurfacePath(p, staged))) {
        const set = hits.get(s.surface) ?? new Set<string>();
        for (const f of s.flows) set.add(f);
        hits.set(s.surface, set);
      }
    }
  }

  if (hits.size === 0) {
    console.log("[coverage] no agentic flows matched the staged paths.");
    return;
  }
  console.log("[coverage] PR touches surfaces with agentic flows:");
  for (const [surface, flows] of hits) {
    console.log(`  ${surface}: ${[...flows].join(", ")}`);
  }
  console.log(
    "Run `pnpm test:agentic <flow>` to verify on this branch (advisory; this hook does not block)."
  );
}

function loadSurfaces(surfacesPath: string): SurfaceEntry[] {
  if (existsSync(surfacesPath)) {
    try {
      const parsed = parseYaml(readFileSync(surfacesPath, "utf-8"));
      if (Array.isArray(parsed)) return parsed as SurfaceEntry[];
    } catch (err) {
      console.warn(
        `[coverage] could not load ${surfacesPath}: ${err instanceof Error ? err.message : String(err)}`
      );
    }
  }
  // Built-in fallback — coarse but correct for the v2 starter flow set.
  return [
    {
      surface: "chat-panel",
      paths: ["src/pages/launcher/", "src/components/Ask/", "src/components/Chat/"],
      flows: ["chat-ask"]
    },
    { surface: "ide", paths: ["src/pages/ide/"], flows: ["ide-save"] },
    {
      surface: "builder",
      paths: ["src/components/BuilderDialog/"],
      flows: ["builder-edits-app"]
    }
  ];
}

function matchSurfacePath(pattern: string, target: string): boolean {
  const stripped = pattern.replace(/\*\*$/, "").replace(/\/+$/, "/");
  return target.includes(stripped);
}

export function readStagedFromStdin(): string[] {
  try {
    const data = readFileSync(0, "utf-8");
    return data
      .split(/[\0\n]/)
      .map((s) => s.trim())
      .filter(Boolean);
  } catch {
    return [];
  }
}

export function defaultCachePath(runnerDir: string): string {
  return resolve(runnerDir, "..", ".cache", "bespoke-actions.json");
}

export function defaultStagingPath(runnerDir: string): string {
  return resolve(runnerDir, "..", ".cache", "healing-staging.json");
}

interface AcceptHealingArgs {
  stagingPath: string;
  cachePath: string;
  flowFilter?: string;
}

export function runAcceptHealing({ stagingPath, cachePath, flowFilter }: AcceptHealingArgs): void {
  const staging = readStaging(stagingPath);
  if (staging.entries.length === 0) {
    console.log(`[heal] no staged healing entries at ${stagingPath}.`);
    return;
  }
  const promoted = promoteStaging(stagingPath, cachePath, flowFilter);
  if (promoted === 0) {
    console.log(`[heal] no entries matched filter '${flowFilter ?? "(none)"}'. Nothing promoted.`);
    return;
  }
  console.log(
    `[heal] promoted ${promoted} healing recording${promoted === 1 ? "" : "s"} into ${cachePath}.`
  );
  console.log(
    "[heal] run `git status` and review the cache diff before committing — the new selectors are now the test's ground truth."
  );
}
