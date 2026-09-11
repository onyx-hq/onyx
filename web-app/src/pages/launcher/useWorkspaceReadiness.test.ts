// @vitest-environment jsdom

import { renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  hasPendingOnboardingForStorageKey,
  isOnboardingDismissedForStorageKey,
  markOnboardingDismissedForStorageKey,
  storageKey
} from "@/libs/utils/onboardingStorage";
import useWorkspaceReadiness from "./useWorkspaceReadiness";

/**
 * What Home shows about an unfinished workspace. It never redirects into the
 * setup wizard any more — workspaces are set up by the Oxygen team — so what's
 * left to pin is which gap rows appear, for whom, and when Home is allowed to
 * call the IdeOnly credential probe at all. Its inputs are unreliable in
 * opposite directions:
 *
 *   - the credential probe reads the workspace secret store only, so a key set
 *     via an env var reads as missing on a workspace that works;
 *   - the wizard state is localStorage, so it's absent for a teammate and stale
 *     for anyone who abandoned setup halfway.
 */

const WS_ID = "ws-1";

type User = { is_owner: boolean; is_app_admin: boolean };
const STAFF: User = { is_owner: false, is_app_admin: true };
const MEMBER: User = { is_owner: false, is_app_admin: false };

let isLocalMode = false;
let user: User = STAFF;
let githubSetup: {
  missing_llm_key_vars: { var_name: string; vendor: string; sample_model_name?: string }[];
  warehouses: { name: string; dialect: string; missing_vars: { var_name: string }[] }[];
  models?: { name: string; key_var: string | null }[];
};
let agents: { path: string; public: boolean; model?: string }[];
let databases: { name: string }[];

vi.mock("@/contexts/AuthContext", () => ({ useAuth: () => ({ isLocalMode }) }));
vi.mock("@/hooks/api/users/useCurrentUser", () => ({
  default: () => ({ data: user, isPending: false })
}));
vi.mock("@/hooks/useCurrentProjectBranch", () => ({
  default: () => ({ project: { id: WS_ID, storage_key: WS_ID } })
}));
vi.mock("@/stores/useCurrentOrg", () => ({
  default: (selector: (s: { org: { slug: string } }) => unknown) =>
    selector({ org: { slug: "acme" } })
}));
vi.mock("@/stores/useSettingsDialog", () => ({ default: () => vi.fn() }));
vi.mock("react-router-dom", () => ({
  useNavigate: () => vi.fn(),
  useLocation: () => ({ pathname: "/", search: "", state: null }),
  useParams: () => ({ wsId: WS_ID })
}));

/** Every `enabled` flag Home passed to the setup probe, newest last. */
const probeEnabledCalls: boolean[] = [];

/**
 * A prior `/onboarding` visit primed the shared query key. `AgenticSetup` calls
 * `useGithubSetup()` on `queryKeys.onboarding.githubSetup(projectId)`, so this
 * is reachable in a real session — and it is the case where the mock must NOT
 * assume the answer, or the "no verdict without a probe" test passes by
 * construction.
 */
let probeCacheWarm = false;

// Mirrors React Query: `enabled: false` stops FETCHING, not cache reads. Cold
// and disabled means `data` undefined with `isPending` TRUE (the state that
// hangs a naive `isPending` wait); warm and disabled still serves the cached
// payload, and the hook — not this mock — has to ignore it.
vi.mock("@/hooks/api/onboarding/useGithubSetup", () => ({
  default: (enabled = true) => {
    probeEnabledCalls.push(enabled);
    const hasData = enabled || probeCacheWarm;
    return {
      data: hasData ? githubSetup : undefined,
      isPending: !hasData,
      isError: false
    };
  }
}));
vi.mock("@/hooks/api/agents/useAgents", () => ({
  default: () => ({ data: agents, isPending: false, isError: false })
}));
vi.mock("@/hooks/api/databases/useDatabases", () => ({
  default: () => ({ data: databases, isPending: false, isError: false })
}));

/** A workspace that can answer questions: one db, one public agent, no gaps. */
const setUpWorkspace = () => {
  githubSetup = { missing_llm_key_vars: [], warehouses: [], models: [] };
  agents = [{ path: "analytics.agent.yml", public: true, model: "claude" }];
  databases = [{ name: "primary" }];
};

/** The credential probe reporting a missing LLM key for the default agent. */
const reportMissingLlmKey = () => {
  githubSetup = {
    missing_llm_key_vars: [{ var_name: "ANTHROPIC_API_KEY", vendor: "anthropic" }],
    warehouses: [],
    models: [{ name: "claude", key_var: "ANTHROPIC_API_KEY" }]
  };
};

const seedPendingWizard = () =>
  localStorage.setItem(
    storageKey(WS_ID),
    JSON.stringify({ step: "github_llm_keys", storageKey: WS_ID, mode: "github" })
  );

const probeWasEnabled = () => probeEnabledCalls[probeEnabledCalls.length - 1];

beforeEach(() => {
  localStorage.clear();
  probeEnabledCalls.length = 0;
  probeCacheWarm = false;
  isLocalMode = false;
  user = STAFF;
  setUpWorkspace();
});
afterEach(() => vi.clearAllMocks());

/**
 * `GET /{ws}/onboarding/github-setup` reads config.yml off the working copy, so
 * the manifest pins it IdeOnly and it does NOT degrade — a serve replica
 * answers 502 `x-oxy-required-role: ide` whenever the Factory pod is
 * restarting, which the Axios interceptor turns into the app-wide "Oxygen
 * Factory is temporarily unavailable" banner. Home calling it unconditionally
 * meant every rollout showed that banner to every user. These tests pin the
 * gate that keeps Home off that route.
 */
describe("useWorkspaceReadiness — the IdeOnly setup probe", () => {
  it("is not called at all on a normal Home visit", () => {
    renderHook(() => useWorkspaceReadiness());
    expect(probeWasEnabled()).toBe(false);
  });

  it("does not leave Home spinning when it is skipped", () => {
    // A disabled React Query sits at isPending forever.
    const { result } = renderHook(() => useWorkspaceReadiness());
    expect(result.current.status).toBe("ready");
  });

  it("is called while a wizard is pending for this workspace", () => {
    seedPendingWizard();
    renderHook(() => useWorkspaceReadiness());
    expect(probeWasEnabled()).toBe(true);
  });

  it("is called for someone who deferred setup, so their gap rows survive", () => {
    markOnboardingDismissedForStorageKey(WS_ID);
    reportMissingLlmKey();
    renderHook(() => useWorkspaceReadiness());
    expect(probeWasEnabled()).toBe(true);
  });

  it("retires a dismissal that has nothing left to defer", () => {
    // CompletionCard sets the dismiss flag on EVERY successful completion, so
    // the workspace creator carries it with no unfinished setup. Left alone it
    // would keep them on the IdeOnly route — and in the Factory banner — for
    // the life of that browser profile.
    markOnboardingDismissedForStorageKey(WS_ID);
    renderHook(() => useWorkspaceReadiness());
    expect(isOnboardingDismissedForStorageKey(WS_ID)).toBe(false);
    // Converged: the next load takes the no-probe path.
    probeEnabledCalls.length = 0;
    renderHook(() => useWorkspaceReadiness());
    expect(probeWasEnabled()).toBe(false);
  });

  it("keeps the dismissal while the deferred gap is still real", () => {
    markOnboardingDismissedForStorageKey(WS_ID);
    reportMissingLlmKey();
    renderHook(() => useWorkspaceReadiness());
    expect(isOnboardingDismissedForStorageKey(WS_ID)).toBe(true);
  });

  it("leaves pending wizard state alone when retiring a dismissal", () => {
    // Clearing the resume point instead of just the flag would lose a
    // half-finished wizard.
    seedPendingWizard();
    markOnboardingDismissedForStorageKey(WS_ID);
    renderHook(() => useWorkspaceReadiness());
    expect(hasPendingOnboardingForStorageKey(WS_ID)).toBe(true);
  });

  it("is called in legacy local mode, which has no fleet to protect and no accounts", () => {
    isLocalMode = true;
    user = MEMBER;
    renderHook(() => useWorkspaceReadiness());
    expect(probeWasEnabled()).toBe(true);
  });

  it("is not called for a viewer without staff standing, even mid-wizard", () => {
    // The probe only feeds "Finish setup" rows, which such a viewer never sees.
    user = MEMBER;
    seedPendingWizard();
    markOnboardingDismissedForStorageKey(WS_ID);
    renderHook(() => useWorkspaceReadiness());
    expect(probeWasEnabled()).toBe(false);
  });
});

describe("useWorkspaceReadiness — Home, never the wizard", () => {
  it("leaves a set-up workspace on Home", () => {
    const { result } = renderHook(() => useWorkspaceReadiness());
    expect(result.current.status).toBe("ready");
  });

  it("does not bounce a working workspace whose credential probe reports a missing key", () => {
    // The cloud probe ignores env vars, and localStorage is per-browser — so
    // this is what a teammate opening an already-onboarded workspace looks like.
    markOnboardingDismissedForStorageKey(WS_ID);
    reportMissingLlmKey();
    const { result } = renderHook(() => useWorkspaceReadiness());
    expect(result.current.status).toBe("ready");
  });

  it("offers the missing key as a gap row instead", () => {
    markOnboardingDismissedForStorageKey(WS_ID);
    reportMissingLlmKey();
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps.map((g) => g.label)).toContain("LLM API key not set");
  });

  it("names the warehouse whose credentials are missing", () => {
    markOnboardingDismissedForStorageKey(WS_ID);
    githubSetup = {
      missing_llm_key_vars: [],
      warehouses: [
        {
          name: "snowflake_prod",
          dialect: "snowflake",
          missing_vars: [{ var_name: "SF_PASSWORD" }]
        }
      ],
      models: []
    };
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps.map((g) => g.label)).toContain(
      "Missing credentials for snowflake_prod"
    );
  });

  it("reports no credential gap when it never asked", () => {
    // No probe means no verdict — not a gap inferred from a call we skipped.
    reportMissingLlmKey();
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps).toEqual([]);
  });

  it("ignores cached probe data when the probe is off", () => {
    // `enabled: false` stops the fetch, not the cache read, and AgenticSetup
    // fills the same query key. Rendering gap rows off that payload would show
    // a verdict that can never refresh — a key saved a minute ago still listed
    // as missing.
    probeCacheWarm = true;
    reportMissingLlmKey();
    const { result } = renderHook(() => useWorkspaceReadiness());
    expect(probeWasEnabled()).toBe(false);
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps).toEqual([]);
  });

  it("ignores abandoned wizard state once the workspace is usable", () => {
    // The old loop: Home sends them to the wizard, the wizard sees in-flight
    // state and won't send them back.
    seedPendingWizard();
    const { result } = renderHook(() => useWorkspaceReadiness());
    expect(result.current.status).toBe("ready");
  });

  it("respects an explicit 'Skip for now'", () => {
    seedPendingWizard();
    reportMissingLlmKey();
    markOnboardingDismissedForStorageKey(WS_ID);
    const { result } = renderHook(() => useWorkspaceReadiness());
    expect(result.current.status).toBe("ready");
  });

  it("offers a pending wizard as a row rather than resuming it", () => {
    // The last redirect Home had: pending wizard state on a workspace that
    // isn't set up yet. Now the wizard is one click away, not forced.
    seedPendingWizard();
    reportMissingLlmKey();
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps).toContainEqual(
      expect.objectContaining({ label: "LLM API key not set", cta: "Finish setup" })
    );
  });

  it("does not start setup from the credential probe in legacy local mode either", () => {
    isLocalMode = true;
    reportMissingLlmKey();
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps.map((g) => g.label)).toContain("LLM API key not set");
  });
});

describe("useWorkspaceReadiness — Finish setup is staff work", () => {
  it("shows no Finish setup rows to a viewer without staff standing", () => {
    user = MEMBER;
    seedPendingWizard();
    markOnboardingDismissedForStorageKey(WS_ID);
    reportMissingLlmKey();
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps.filter((g) => g.cta === "Finish setup")).toEqual([]);
  });

  it("locks chat for that viewer without offering config steps they can't take", () => {
    user = MEMBER;
    databases = [];
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps).toEqual([]);
    expect(result.current.shouldDisableChat).toBe(true);
  });

  it("offers staff the no-database step", () => {
    user = STAFF;
    databases = [];
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps.map((g) => g.label)).toContain("No database connection");
  });

  it("counts a Global Owner as staff", () => {
    user = { is_owner: true, is_app_admin: false };
    markOnboardingDismissedForStorageKey(WS_ID);
    reportMissingLlmKey();
    const { result } = renderHook(() => useWorkspaceReadiness());
    if (result.current.status !== "ready") throw new Error("expected ready");
    expect(result.current.gaps.map((g) => g.cta)).toContain("Finish setup");
  });
});
