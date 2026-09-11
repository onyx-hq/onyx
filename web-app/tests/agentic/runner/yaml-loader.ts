import { readFileSync } from "node:fs";
import { basename } from "node:path";
import { parse } from "yaml";
import { validatePlaceholders } from "./secrets";
import type { FlowSettings, FlowTarget, FlowTest } from "./types";

const DEFAULT_SETTINGS: FlowSettings = {
  runs: 1,
  // Used as the escalation target when the cheap pickup model (haiku) stalls.
  // For a single-model run, this is also the only model used. Sonnet 4.7
  // is not yet on this Anthropic account (404 not_found_error); revert to
  // 4-6 until 4-7 is GA. Update pricing.ts at the same time when bumping.
  model: "claude-sonnet-4-6",
  judge_model: "claude-haiku-4-5-20251001",
  trace: "on-failure",
  cache_actions: true,
  max_steps: 30,
  // Default to single-workspace mode. Flows that need the multi-tenant
  // (org → workspace) shape opt in via `backend_mode: cloud`.
  backend_mode: "local"
};

const VALID_TARGETS: FlowTarget[] = ["chat", "ide", "threads", "onboarding", "any"];

export function loadFlow(filePath: string): FlowTest {
  const raw = parse(readFileSync(filePath, "utf-8")) as Record<string, unknown>;
  if (!raw || typeof raw !== "object") {
    throw new Error(`${filePath}: expected YAML object at root`);
  }

  const cases = raw.cases;
  if (!Array.isArray(cases) || cases.length === 0) {
    throw new Error(`${filePath}: 'cases' must be a non-empty array`);
  }

  const target = (raw.target as FlowTarget | undefined) ?? "any";
  if (!VALID_TARGETS.includes(target)) {
    throw new Error(
      `${filePath}: invalid target '${target}', expected one of ${VALID_TARGETS.join(", ")}`
    );
  }

  const settings = { ...DEFAULT_SETTINGS, ...(raw.settings as Partial<FlowSettings> | undefined) };
  if (settings.backend_mode !== "local" && settings.backend_mode !== "cloud") {
    throw new Error(
      `${filePath}: settings.backend_mode must be 'local' or 'cloud', got ${JSON.stringify(settings.backend_mode)}`
    );
  }

  return {
    name: (raw.name as string | undefined) ?? basename(filePath, ".flow.test.yml"),
    file: filePath,
    target,
    settings,
    setup: (raw.setup as string[] | undefined) ?? [],
    cases: cases.map((c, i) => normalizeCase(c, filePath, i))
  };
}

function normalizeCase(raw: unknown, file: string, idx: number): FlowTest["cases"][number] {
  if (!raw || typeof raw !== "object") {
    throw new Error(`${file}: case[${idx}] is not an object`);
  }
  const c = raw as Record<string, unknown>;
  if (typeof c.name !== "string") {
    throw new Error(`${file}: case[${idx}] missing 'name'`);
  }
  if (!Array.isArray(c.steps) || c.steps.length === 0) {
    throw new Error(`${file}: case '${c.name}' must have at least one step`);
  }
  for (const [i, step] of c.steps.entries()) {
    if (!step || typeof step !== "object") {
      throw new Error(`${file}: case '${c.name}' step[${i}] is not an object`);
    }
    const s = step as Record<string, unknown>;
    if ("tool" in s || "args" in s) {
      throw new Error(
        `${file}: case '${c.name}' step[${i}] uses deprecated 'tool'/'args'. ` +
          "The runtime now exposes only generic browser tools — describe the step in 'act:'."
      );
    }
    const hasAct = typeof s.act === "string";
    const hasWait = typeof s.wait_for === "string";
    if (!hasAct && !hasWait) {
      throw new Error(`${file}: case '${c.name}' step[${i}] must set 'act' or 'wait_for'`);
    }
    if (hasAct) {
      // Validate `${VAR}` placeholders against the current environment but
      // do NOT substitute here — substitution happens at the egress
      // boundary (LLM prompt + tool dispatch) so plaintext secrets never
      // reach the action cache or the result artifact. See secrets.ts.
      validatePlaceholders(s.act as string, `${file}: case '${c.name}' step[${i}]`);
    }
    if ("cache_scope" in s && s.cache_scope !== "flow" && s.cache_scope !== "shared") {
      throw new Error(
        `${file}: case '${c.name}' step[${i}] cache_scope must be 'flow' or 'shared', got ${JSON.stringify(s.cache_scope)}`
      );
    }
  }
  return {
    name: c.name,
    tags: (c.tags as string[] | undefined) ?? [],
    steps: c.steps as FlowTest["cases"][number]["steps"],
    expect: (c.expect as FlowTest["cases"][number]["expect"] | undefined) ?? []
  };
}
