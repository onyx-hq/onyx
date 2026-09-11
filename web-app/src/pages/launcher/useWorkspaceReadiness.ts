import { Database, GitFork, KeyRound } from "lucide-react";
import { useEffect, useMemo } from "react";
import { useLocation, useNavigate, useParams } from "react-router-dom";
import { useAuth } from "@/contexts/AuthContext";
import useAgents from "@/hooks/api/agents/useAgents";
import useDatabases from "@/hooks/api/databases/useDatabases";
import useGithubSetup from "@/hooks/api/onboarding/useGithubSetup";
import useCurrentUser from "@/hooks/api/users/useCurrentUser";
import useCurrentProjectBranch from "@/hooks/useCurrentProjectBranch";
import {
  clearOnboardingDismissedForStorageKey,
  hasPendingOnboardingForStorageKey,
  isOnboardingDismissedForStorageKey
} from "@/libs/utils/onboardingStorage";
import ROUTES from "@/libs/utils/routes";
import { getAgentNameFromPath } from "@/libs/utils/string";
import useCurrentOrg from "@/stores/useCurrentOrg";
import useSettingsDialog from "@/stores/useSettingsDialog";

export interface SetupGap {
  icon: typeof Database;
  label: string;
  action: () => void;
  cta: string;
}

export type WorkspaceReadiness =
  | { status: "loading" }
  | { status: "ready"; gaps: SetupGap[]; shouldDisableChat: boolean };

/**
 * The old chat-home's gating logic, extracted: workspace match and setup gaps
 * (per-agent LLM key resolution, warehouse creds, no-db/no-agent). Originally
 * copied verbatim from pages/home — see git history of pages/home/index.tsx
 * for the original inline comments and rationale.
 *
 * Home never redirects into the setup wizard. It used to — first on any
 * missing credential, later only on pending wizard state — and every version
 * dragged someone who just wanted their apps into a setup flow they didn't
 * own. Workspaces are set up by the Oxygen team now, so the credential gaps
 * are "Finish setup" rows shown only to staff (see `canFinishSetup`); the
 * comment at `needsSetupProbe` explains why Home doesn't even make that call
 * on the common path.
 */
export default function useWorkspaceReadiness(): WorkspaceReadiness {
  const { isLocalMode } = useAuth();
  const { data: user, isPending: userPending } = useCurrentUser();
  const { project } = useCurrentProjectBranch();
  const orgSlug = useCurrentOrg((s) => s.org?.slug) ?? "";
  const navigate = useNavigate();
  const location = useLocation();
  const openSettings = useSettingsDialog((s) => s.open);
  const { wsId: urlWsId } = useParams<{ wsId: string }>();
  const locationState = location.state as { agentPath?: string } | null;

  // The Zustand store lags one render behind workspace switches; gate every
  // query so we don't decide a redirect using the previous workspace's data.
  // In local mode there is no :wsId URL segment, so skip the URL check.
  const wsMatch = !!project?.id && (isLocalMode || project.id === urlWsId);

  // Falling back to `project.id` keeps fabricated/partial Workspace objects
  // (legacy callers; tests) working until they fill in storage_key.
  const projectStorageKey = project?.storage_key ?? project?.id ?? "";
  const hasPendingWizardState = hasPendingOnboardingForStorageKey(projectStorageKey);
  const onboardingDismissed = isOnboardingDismissedForStorageKey(projectStorageKey);

  // "Finish setup" opens the setup wizard, which is staff work: platform
  // standing is the server's display flag for that. Legacy local mode has no
  // accounts at all — whoever runs it is the operator.
  const canFinishSetup = isLocalMode || !!(user?.is_owner || user?.is_app_admin);

  // `onboarding/github-setup` reads config.yml off the working copy, so the
  // manifest pins the whole `/{workspace_id}/onboarding/*` subtree to IdeOnly
  // (`role_manifest.rs`) — and unlike `/details` and `/status` it is NOT in
  // `degrades_when_ide_unreachable`, so a serve replica answers 502 with
  // `x-oxy-required-role: ide` while the Factory pod restarts. The Axios
  // interceptor turns that into the app-wide "Oxygen Factory is temporarily
  // unavailable" banner. Firing it on every Home load meant every rollout put
  // that banner in front of every user, none of whom had asked for anything
  // the Factory owns.
  //
  // So Home only probes when THIS browser is tracking a setup for THIS
  // workspace (mid-wizard, or deferred via "Skip for now"), plus legacy local
  // mode where there's no fleet and the probe is the only entry into setup —
  // and only for a viewer who can act on the answer: the probe feeds nothing
  // but the "Finish setup" rows (and retiring a stale dismissal). Everyone
  // else gets Home from the FleetOk `/agents` + `/databases` reads.
  const needsSetupProbe =
    wsMatch && canFinishSetup && (hasPendingWizardState || onboardingDismissed || isLocalMode);

  // In cloud mode `github_setup` checks only DB secrets, so a key set via an
  // env var on the server is still reported missing — intentional, to prompt
  // operators to configure it through the UI. In local mode the endpoint also
  // checks env vars, so keys in .env are treated as present.
  const {
    data: githubSetup,
    isPending: setupPending,
    isError: setupError
  } = useGithubSetup(needsSetupProbe);
  const { data: agents = [], isPending: agentsPending, isError: agentsError } = useAgents();
  const {
    data: databases = [],
    isPending: databasesPending,
    isError: databasesError
  } = useDatabases(wsMatch);

  // Mirror AgentsDropdown's default-agent logic. We rely on `agent.model`
  // already being on each listing item — fetching the agent's full config
  // here would block the home render on a serial round-trip.
  const defaultAgent = useMemo(() => {
    const publicAgents = agents.filter((a) => a.public);
    if (publicAgents.length === 0) return null;
    if (locationState?.agentPath) {
      const preferred = publicAgents.find((a) => a.path === locationState.agentPath);
      if (preferred) return preferred;
    }
    return [...publicAgents].sort((a, b) =>
      (a.name ?? getAgentNameFromPath(a.path)).localeCompare(b.name ?? getAgentNameFromPath(b.path))
    )[0];
  }, [agents, locationState?.agentPath]);

  // `enabled: false` suppresses FETCHING, not cache reads — and AgenticSetup
  // populates this very query key, so a Home visit after `/onboarding` would
  // otherwise render gap rows off a cached payload that can never refresh (a
  // key the user just saved would still read as missing). Gate the DATA on the
  // probe, not just the request, so "we didn't ask" really does mean "we have
  // no verdict".
  const setup = needsSetupProbe ? githubSetup : undefined;

  // Fall through on API errors so a broken endpoint doesn't trap the user.
  const anyApiError = setupError || agentsError || databasesError;
  const missingLlmKeys = setup?.missing_llm_key_vars ?? [];
  const missingLlmKeyVars = new Set(missingLlmKeys.map((k) => k.var_name));
  // DuckDB is file-backed; its `password_var` is a config artifact, not a prompt.
  const warehousesNeedingCreds = (setup?.warehouses ?? []).filter(
    (w) => w.dialect.toLowerCase() !== "duckdb" && w.missing_vars.length > 0
  );

  // Tie the LLM gap to the agent the chat actually uses, not to any-key-missing
  // — otherwise a saved Anthropic key wouldn't suppress the warning when the
  // active agent is on Anthropic but other unused OpenAI models still lack a
  // key. Two resolution paths so older backends still work:
  //   1. `models[]` (new) — full model -> key_var map.
  //   2. `missing_llm_key_vars[].sample_model_name` (existing) — partial,
  //      only resolves when the agent's model is the dedupe-winning sample.
  const modelKeyVarMap = new Map((setup?.models ?? []).map((m) => [m.name, m.key_var]));
  const defaultAgentModel = defaultAgent?.model;
  const matchedBySample = defaultAgentModel
    ? missingLlmKeys.find((k) => k.sample_model_name === defaultAgentModel)?.var_name
    : undefined;
  const resolvedKeyVar: string | null | undefined =
    defaultAgentModel !== undefined && modelKeyVarMap.has(defaultAgentModel)
      ? (modelKeyVarMap.get(defaultAgentModel) ?? null)
      : matchedBySample !== undefined
        ? matchedBySample
        : undefined;
  const llmKeyMissingForAgent =
    resolvedKeyVar === undefined
      ? missingLlmKeys.length > 0
      : resolvedKeyVar !== null && missingLlmKeyVars.has(resolvedKeyVar);

  const hasDatabases = databases.length > 0;
  const hasPublicAgents = agents.filter((a) => a.public).length > 0;
  const hasWarehouseCredentials = warehousesNeedingCreds.length === 0;

  // With the probe off there is no credential verdict at all — both halves read
  // false off the undefined `setup`. That's the intended reading: no evidence of
  // a gap, rather than a gap assumed from a call we never made. The cost is
  // stated plainly in `shouldDisableChat` below.
  //
  // Even with a verdict, a missing credential is never a reason to hijack Home
  // into the full-page wizard: the probe reads the workspace secret store only,
  // so a key supplied by an env var reads as missing on a workspace that works.
  const hasMissingCredentials = !anyApiError && (llmKeyMissingForAgent || !hasWarehouseCredentials);
  const isWorkspaceReady = hasDatabases && hasPublicAgents && !hasMissingCredentials;

  // A disabled query sits at `isPending` forever, so only wait on the setup
  // probe when we actually asked for it — otherwise Home spins indefinitely.
  // The user decides whether we ask, so wait for it first rather than flip the
  // probe on a render later.
  const isLoading =
    !wsMatch ||
    userPending ||
    (needsSetupProbe && setupPending) ||
    agentsPending ||
    databasesPending;

  // Retire the deferral once there is demonstrably nothing left to defer.
  // `CompletionCard` sets it on every successful completion, not only on "Skip
  // for now", and nothing else ever clears it — so without this the workspace
  // CREATOR keeps the flag forever, keeps probing an IdeOnly route on every
  // Home load, and keeps catching the Factory banner on every rollout with no
  // unfinished setup at all. One clean load converges that browser to the
  // no-probe path. The wizard state is left alone so a resume still works.
  const shouldRetireDismissal =
    !isLoading && !anyApiError && onboardingDismissed && isWorkspaceReady;
  useEffect(() => {
    if (shouldRetireDismissal) clearOnboardingDismissedForStorageKey(projectStorageKey);
  }, [shouldRetireDismissal, projectStorageKey]);

  if (isLoading) {
    return { status: "loading" };
  }

  // Absolute paths: `home` and `onboarding` are siblings in WorkspaceLayout,
  // so relative `to='onboarding'` resolves to `/home/onboarding` (404).
  const routes = ROUTES.ORG(orgSlug).WORKSPACE(project.id);

  const isSetupComplete = hasDatabases && hasPublicAgents;
  // On API error we don't render any gap rows (we can't trust the data), so
  // the user would see a locked chat with no actionable steps. Let them try
  // chatting instead.
  //
  // Deliberately NOT tied to the credential check. On the common path the probe
  // never runs, so `llmKeyMissingForAgent` is false and no key gap is known —
  // meaning a cloud workspace with a genuinely missing key shows no row here and
  // an enabled chat, and the user's first signal is a failed send. That is the
  // accepted cost of not calling an ide-pinned route on every Home load: the
  // check's false positives (env-var keys read as missing) would otherwise lock
  // chat on workspaces that work. Surfacing the gap lazily from a send failure
  // is the follow-up; the honest fix is moving the probe onto the compile
  // boundary so it can run on any replica.
  const shouldDisableChat = !anyApiError && !isSetupComplete;

  const gaps: SetupGap[] = [];
  if (!anyApiError) {
    // Credential gaps: the wizard collects exactly these secrets, so the CTA
    // opens it rather than pushing the user into Settings to guess which
    // `*_var` the config references. Offered, not forced — and only to staff,
    // whose job workspace setup is (the probe is already off for anyone else;
    // the gate is restated here so the rows can't outlive a probe change).
    if (canFinishSetup && llmKeyMissingForAgent) {
      gaps.push({
        icon: KeyRound,
        label: "LLM API key not set",
        action: () => navigate(routes.ONBOARDING),
        cta: "Finish setup"
      });
    }
    if (canFinishSetup && warehousesNeedingCreds.length > 0) {
      gaps.push({
        icon: KeyRound,
        label:
          warehousesNeedingCreds.length === 1
            ? `Missing credentials for ${warehousesNeedingCreds[0].name}`
            : `Missing credentials for ${warehousesNeedingCreds.length} databases`,
        action: () => navigate(routes.ONBOARDING),
        cta: "Finish setup"
      });
    }
    // Gaps the wizard can't fix — they need config.yml edits, so they are staff
    // work too. Everyone else learns why chat is locked from the composer.
    if (canFinishSetup && !hasDatabases) {
      gaps.push({
        icon: Database,
        label: "No database connection",
        action: () => openSettings("workspace.databases"),
        cta: "Configure"
      });
    }
    if (canFinishSetup && !hasPublicAgents) {
      gaps.push({
        icon: GitFork,
        label: "No agents configured",
        action: () => navigate(routes.IDE.ROOT),
        cta: "Open IDE"
      });
    }
  }

  return { status: "ready", gaps, shouldDisableChat };
}
