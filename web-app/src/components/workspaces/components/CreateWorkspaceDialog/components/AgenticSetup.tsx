import { Home } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Navigate, useNavigate } from "react-router-dom";
import { toast } from "sonner";
import OrgSwitcher from "@/components/org/OrgSwitcher";
import { Button } from "@/components/ui/shadcn/button";
import {
  ResizableHandle,
  ResizablePanel,
  ResizablePanelGroup
} from "@/components/ui/shadcn/resizable";
import useSidebar from "@/components/ui/shadcn/sidebar-context";
import { Spinner } from "@/components/ui/shadcn/spinner";
import useAgents from "@/hooks/api/agents/useAgents";
import useDatabases from "@/hooks/api/databases/useDatabases";
import useGithubSetup from "@/hooks/api/onboarding/useGithubSetup";
import { sseEventToUiBlock, useAnalyticsRun } from "@/hooks/useAnalyticsRun";
import { useBuilderActivity } from "@/hooks/useBuilderActivity";
import useCurrentProjectBranch from "@/hooks/useCurrentProjectBranch";
import {
  clearOnboardingStateForStorageKey,
  initOnboardingStateForStorageKey,
  markOnboardingDismissedForStorageKey
} from "@/libs/utils/onboardingStorage";
import ROUTES from "@/libs/utils/routes";
import { AnalyticsService, type HumanInputQuestion, type UiBlock } from "@/services/api/analytics";
import { type OnboardingResetRequest, OnboardingService } from "@/services/api/onboarding";
import CancelSetupButton from "./CancelSetupButton";
import OnboardingRightRail from "./OnboardingRightRail";
import OnboardingThread from "./OnboardingThread";
import { getPreviousStep, useOnboardingOrchestrator, wantsSecondApp } from "./orchestrator";
import StartOverConfirmDialog from "./StartOverConfirmDialog";
import type {
  GeneratedArtifact,
  OnboardingState,
  PhaseProgress,
  PhaseTimings,
  SubPhaseKey
} from "./types";
import { LLM_KEY_VAR, useOnboardingActions } from "./useOnboardingActions";
import { useViewRunManager } from "./useViewRunManager";

/** Top-level onboarding dispatcher. Onboarding is one-shot per workspace:
 *  once the workspace can answer questions — LLM key configured, at least
 *  one database, at least one public agent — redirect any visit away. */
export default function AgenticSetupPage() {
  const { project } = useCurrentProjectBranch();
  // Falling back to `project.id` keeps fabricated/partial Workspace
  // objects (legacy callers; tests) working until they fill in storage_key.
  const storageKey = project.storage_key ?? project.id;
  return <AgenticSetupForWorkspace key={storageKey} storageKey={storageKey} />;
}

function AgenticSetupForWorkspace({ storageKey }: { storageKey: string }) {
  // Must share Home's probe so the two pages can't disagree on "is
  // onboarding done?" — a coarser gauge (any LLM key set) would ping-pong
  // with Home's per-`key_var` view.
  const {
    data: githubSetup,
    isPending: setupPending,
    isError: setupError,
    refetch: refetchSetup
  } = useGithubSetup();
  const {
    data: databases,
    isPending: databasesPending,
    isError: databasesError,
    refetch: refetchDatabases
  } = useDatabases();
  const {
    data: agents,
    isPending: agentsPending,
    isError: agentsError,
    refetch: refetchAgents
  } = useAgents();

  const orchestrator = useOnboardingOrchestrator(storageKey);
  const { initGithubFlow } = orchestrator;

  const publicAgentCount = (agents ?? []).filter((a) => a.public).length;
  const missingLlmKeyVars = githubSetup?.missing_llm_key_vars ?? [];
  const warehousesNeedingCreds = (githubSetup?.warehouses ?? []).filter(
    (w) => w.dialect.toLowerCase() !== "duckdb" && w.missing_vars.length > 0
  );
  const hasMissingCredentials = missingLlmKeyVars.length > 0 || warehousesNeedingCreds.length > 0;
  const hasMinimumWorkspace = (databases?.length ?? 0) > 0 && publicAgentCount > 0;
  const isReady = !hasMissingCredentials && hasMinimumWorkspace;

  // "In flight" requires a `mode` — a modeless `welcome` entry (e.g. from a
  // direct /onboarding visit on a fully-set-up workspace) shouldn't trap the
  // user on a static welcome screen, so it falls through to the redirect-
  // away path. When creds *are* missing, the seed effect below promotes
  // modeless state into `mode: "demo"` before this check matters.
  const isInFlightWizard =
    orchestrator.state.mode !== undefined && orchestrator.state.step !== "complete";
  const shouldRedirectAway = isReady && !isInFlightWizard;

  useEffect(() => {
    if (shouldRedirectAway) clearOnboardingStateForStorageKey(storageKey);
  }, [shouldRedirectAway, storageKey]);

  // The credential-driven Home → /onboarding redirect doesn't seed mode/step
  // the way cloud's `WorkspacePreparing` does, so do it here. Mode inferred
  // from storage_key shape (UUID → cloud github-import wording; `local:` →
  // demo wording). `initGithubFlow` is idempotent — see its action doc.
  const inferredCredentialFlowMode = storageKey.startsWith("local:") ? "demo" : "github";
  useEffect(() => {
    if (!hasMissingCredentials) return;
    if (orchestrator.state.mode !== undefined) return;
    if (orchestrator.state.step !== "welcome") return;
    initGithubFlow(inferredCredentialFlowMode);
  }, [
    hasMissingCredentials,
    orchestrator.state.mode,
    orchestrator.state.step,
    initGithubFlow,
    inferredCredentialFlowMode
  ]);

  if (setupPending || databasesPending || agentsPending) {
    return (
      <div className='flex h-full items-center justify-center'>
        <Spinner className='size-6' />
      </div>
    );
  }

  // Failing the readiness lookup is a blocking, page-level error: with no
  // data we can neither redirect nor safely render the wizard (an
  // already-onboarded workspace would see a stale form). Surface a retry
  // affordance instead of falling through.
  if (setupError || databasesError || agentsError) {
    return (
      <div className='flex h-full items-center justify-center p-6'>
        <div className='flex max-w-sm flex-col items-center gap-3 text-center'>
          <p className='font-medium text-sm'>Couldn't check workspace setup</p>
          <p className='text-muted-foreground text-xs'>
            We couldn't reach the server to determine whether onboarding is needed. Check your
            connection and try again.
          </p>
          <Button
            variant='outline'
            size='sm'
            onClick={() => {
              void refetchSetup();
              void refetchDatabases();
              void refetchAgents();
            }}
          >
            Retry
          </Button>
        </div>
      </div>
    );
  }

  // If the workspace is already set up, never show the wizard. Only exception
  // is `step === "building"`: isReady can briefly flip true between agent and
  // app phases while the user is actively watching the build, and we want to
  // stay on the wizard until they reach "complete".
  if (shouldRedirectAway) {
    return <Navigate to='..' replace />;
  }

  // BlankOnboardingPage is reserved for the cloud "create from scratch" flow
  // (the only path that sets `mode: "new"`). Everything else — demo/github
  // imports, all of local, post-completion credential gaps — uses
  // GithubOnboardingPage, which only prompts for missing secrets.
  // The `step !== "complete"` guard lets a finished new-wizard whose
  // credentials are later revoked fall into the lightweight flow instead of
  // restarting the from-scratch arc.
  const isResumingNewWizard =
    orchestrator.state.mode === "new" && orchestrator.state.step !== "complete";
  if (isResumingNewWizard) {
    return <BlankOnboardingPage orchestrator={orchestrator} />;
  }
  return <GithubOnboardingPage orchestrator={orchestrator} />;
}

type OrchestratorHandle = ReturnType<typeof useOnboardingOrchestrator>;

function BlankOnboardingPage({ orchestrator }: { orchestrator: OrchestratorHandle }) {
  const actions = useOnboardingActions(orchestrator);
  const { project } = useCurrentProjectBranch();
  const projectId = project?.id ?? "";
  const storageKeyForReset = project?.storage_key ?? project?.id ?? "";
  const { isMobile } = useSidebar();
  const navigate = useNavigate();

  // One run hook per scalar build phase (config, agent, app, optionally app2).
  // The app2 hook is always instantiated (React hooks can't be conditional),
  // but the effects that drive it are gated on `wantsSecondApp(selectedTables)`.
  const configRun = useAnalyticsRun({ projectId });
  const agentRun = useAnalyticsRun({ projectId });
  const appRun = useAnalyticsRun({ projectId });
  const app2Run = useAnalyticsRun({ projectId });

  // Parallel view run manager — handles N concurrent view builds
  const { setViewRunStatus } = orchestrator;
  const onViewDone = useCallback(
    (table: string) => setViewRunStatus(table, "done"),
    [setViewRunStatus]
  );
  const onViewFailed = useCallback(
    (table: string) => setViewRunStatus(table, "failed"),
    [setViewRunStatus]
  );
  const viewRunManager = useViewRunManager(projectId, {
    startViewRun: actions.startViewRun,
    onViewDone,
    onViewFailed
  });

  // Derive builder activity items from the currently-streaming run
  const activeEvents = useMemo(() => {
    for (const run of [configRun, agentRun, appRun, app2Run]) {
      if (run.state.tag === "running" || run.state.tag === "suspended") {
        return run.state.events.map(sseEventToUiBlock);
      }
    }
    return [];
  }, [configRun, agentRun, appRun, app2Run]);
  const builderActivityItems = useBuilderActivity(activeEvents, new Map());

  // Extract stable references
  const step = orchestrator.state.step;
  const buildError = orchestrator.state.buildError;
  const phaseStatuses = orchestrator.state.phaseStatuses;
  const phaseRunIds = orchestrator.state.phaseRunIds;
  const viewRunStatuses = orchestrator.state.viewRunStatuses;
  const selectedTables = orchestrator.state.selectedTables;
  const { goToStep, complete, setBuildError, setPhaseStatus } = orchestrator;
  const { discoverSchemas, resyncWithSelectedTables, startBuildPhase } = actions;

  // Auto-advance: welcome → llm_provider
  useEffect(() => {
    if (step === "welcome") {
      const timer = setTimeout(() => goToStep("llm_provider"), 800);
      return () => clearTimeout(timer);
    }
  }, [step, goToStep]);

  // Auto-advance: schema discovery
  //
  // `discoverSchemas` owns the retry + error-state transitions, so we just
  // kick it off. Swallowing any uncaught error here intentionally — if it
  // does throw we log and let `schemaDiscoveryError` drive the UI. Calling
  // `setDiscoveredSchemas([])` in the old catch path would dispatch
  // `SET_DISCOVERED_SCHEMAS`, which sets step to "table_selection" and can
  // jump the user forward if the effect fires after they hit Back.
  useEffect(() => {
    if (step !== "schema_discovery") return;
    discoverSchemas().catch((err) => {
      console.error("[onboarding] Unexpected error in discoverSchemas:", err);
    });
  }, [step, discoverSchemas]);

  // ── Build phase 1: Re-sync + Config + Views in parallel ─────────────────────
  // Re-sync with only selected tables so .databases/<warehouse>/ is scoped to
  // the user's selection, then start config and all view runs simultaneously.
  // Guard: ref prevents re-entry while the async startBuildPhase is in flight
  // (the state-based guard !phaseStatuses?.config only engages after START_PHASE
  // is dispatched, which happens inside the async chain).
  const configReconnect = configRun.reconnect;
  const viewStartAll = viewRunManager.startAll;
  const buildStartingRef = useRef(false);

  // Reset the ref when leaving build step (going back, starting over)
  useEffect(() => {
    if (step !== "building") buildStartingRef.current = false;
  }, [step]);

  useEffect(() => {
    if (
      step === "building" &&
      !buildError &&
      !phaseStatuses?.config &&
      selectedTables.length > 0 &&
      !buildStartingRef.current
    ) {
      buildStartingRef.current = true;
      // Re-sync scoped to selected tables, then fire config + views in parallel
      resyncWithSelectedTables(selectedTables)
        .then(() => {
          startBuildPhase("config")
            .then((runId) => configReconnect(runId))
            .catch((err: unknown) => {
              buildStartingRef.current = false;
              setBuildError(err instanceof Error ? err.message : "Failed to start config build");
            });
          viewStartAll(selectedTables);
        })
        .catch((err: unknown) => {
          buildStartingRef.current = false;
          setBuildError(err instanceof Error ? err.message : "Failed to sync selected tables");
        });
    }
  }, [
    step,
    buildError,
    phaseStatuses?.config,
    selectedTables,
    resyncWithSelectedTables,
    startBuildPhase,
    configReconnect,
    setBuildError,
    viewStartAll
  ]);

  // ── Build phases 2+3: Agent + App (+ optional App2) in parallel ────────────
  // Start all as soon as config + all view runs complete. App2 only runs when
  // the workspace has ≥ 2 topics — with a single topic there's no variety to
  // warrant a second dashboard.
  const agentReconnect = agentRun.reconnect;
  const appReconnect = appRun.reconnect;
  const app2Reconnect = app2Run.reconnect;
  const shouldBuildApp2 = useMemo(() => wantsSecondApp(selectedTables), [selectedTables]);
  // Config file is "written" when the config run's file_change tool returns,
  // even if the LLM continues generating summary text. The phase completion
  // tracking effect below will mark config as "done" so phase 2 can start.
  const configFileWritten = useMemo(() => {
    if (configRun.state.tag !== "running" && configRun.state.tag !== "suspended") return false;
    return configRun.state.events.some(
      (ev) => ev.type === "tool_result" && (ev.data as { name?: string }).name === "file_change"
    );
  }, [configRun.state]);
  const configDone = phaseStatuses?.config === "done" || phaseStatuses?.config === "failed";
  const allViewsDone = useMemo(() => {
    if (!viewRunStatuses || selectedTables.length === 0) return false;
    return selectedTables.every(
      (t) => viewRunStatuses[t] === "done" || viewRunStatuses[t] === "failed"
    );
  }, [viewRunStatuses, selectedTables]);

  useEffect(() => {
    if (
      step === "building" &&
      configDone &&
      allViewsDone &&
      !phaseStatuses?.agent &&
      !phaseStatuses?.app &&
      (!shouldBuildApp2 || !phaseStatuses?.app2)
    ) {
      const runs = [
        startBuildPhase("agent").then((runId) => {
          agentReconnect(runId);
        }),
        startBuildPhase("app").then((runId) => {
          appReconnect(runId);
        })
      ];
      if (shouldBuildApp2) {
        runs.push(
          startBuildPhase("app2").then((runId) => {
            app2Reconnect(runId);
          })
        );
      }
      Promise.all(runs).catch((err: unknown) => {
        setBuildError(err instanceof Error ? err.message : "Failed to start agent/app build");
      });
    }
  }, [
    step,
    configDone,
    allViewsDone,
    phaseStatuses?.agent,
    phaseStatuses?.app,
    phaseStatuses?.app2,
    shouldBuildApp2,
    startBuildPhase,
    agentReconnect,
    appReconnect,
    app2Reconnect,
    setBuildError
  ]);

  // ── Page-reload recovery ───────────────────────────────────────────────────
  // Reconnect SSE streams to any phase runs that were "running" when the
  // tab was last closed. Intentionally mount-only: these are one-shot resumes,
  // not something to re-trigger as state evolves.
  // biome-ignore lint/correctness/useExhaustiveDependencies: mount-only resume
  useEffect(() => {
    if (phaseRunIds?.config && phaseStatuses?.config === "running") {
      configReconnect(phaseRunIds.config);
    }
    if (phaseRunIds?.agent && phaseStatuses?.agent === "running") {
      agentReconnect(phaseRunIds.agent);
    }
    if (phaseRunIds?.app && phaseStatuses?.app === "running") {
      appReconnect(phaseRunIds.app);
    }
    if (phaseRunIds?.app2 && phaseStatuses?.app2 === "running") {
      app2Reconnect(phaseRunIds.app2);
    }
  }, []);

  // ── Auto-accept file_change suspensions for config, agent, app ──────────
  const configAnswer = configRun.answer;
  useEffect(() => {
    if (step === "building" && configRun.state.tag === "suspended" && !configRun.isAnswering) {
      if (isAutoAcceptSuspension(configRun.state.questions)) {
        configAnswer("Accept");
      } else {
        setPhaseStatus("config", "failed");
        setBuildError(extractSuspensionError(configRun.state.questions));
      }
    }
  }, [
    step,
    configRun.state.tag,
    configRun.isAnswering,
    configRun.state,
    configAnswer,
    setPhaseStatus,
    setBuildError
  ]);

  const agentAnswer = agentRun.answer;
  useEffect(() => {
    if (step === "building" && agentRun.state.tag === "suspended" && !agentRun.isAnswering) {
      if (isAutoAcceptSuspension(agentRun.state.questions)) {
        agentAnswer("Accept");
      } else {
        setPhaseStatus("agent", "failed");
        setBuildError(extractSuspensionError(agentRun.state.questions));
      }
    }
  }, [
    step,
    agentRun.state.tag,
    agentRun.isAnswering,
    agentRun.state,
    agentAnswer,
    setPhaseStatus,
    setBuildError
  ]);

  const appAnswer = appRun.answer;
  useEffect(() => {
    if (step === "building" && appRun.state.tag === "suspended" && !appRun.isAnswering) {
      if (isAutoAcceptSuspension(appRun.state.questions)) {
        appAnswer("Accept");
      } else {
        setPhaseStatus("app", "failed");
        setBuildError(extractSuspensionError(appRun.state.questions));
      }
    }
  }, [
    step,
    appRun.state.tag,
    appRun.isAnswering,
    appRun.state,
    appAnswer,
    setPhaseStatus,
    setBuildError
  ]);

  const app2Answer = app2Run.answer;
  useEffect(() => {
    if (step === "building" && app2Run.state.tag === "suspended" && !app2Run.isAnswering) {
      if (isAutoAcceptSuspension(app2Run.state.questions)) {
        app2Answer("Accept");
      } else {
        setPhaseStatus("app2", "failed");
        setBuildError(extractSuspensionError(app2Run.state.questions));
      }
    }
  }, [
    step,
    app2Run.state.tag,
    app2Run.isAnswering,
    app2Run.state,
    app2Answer,
    setPhaseStatus,
    setBuildError
  ]);

  // ── Phase completion tracking ──────────────────────────────────────────────
  useEffect(() => {
    if (step !== "building") return;
    if (configRun.state.tag === "done") setPhaseStatus("config", "done");
    if (configRun.state.tag === "failed") {
      setPhaseStatus("config", "failed");
      setBuildError(configRun.state.message);
    }
    // Config file written (file_change accepted) but run still streaming —
    // mark as done so phase 2 can start and the rail/messages update.
    if (configFileWritten && phaseStatuses?.config === "running") {
      setPhaseStatus("config", "done");
    }
  }, [
    step,
    configRun.state.tag,
    configFileWritten,
    phaseStatuses?.config,
    setPhaseStatus,
    setBuildError,
    configRun.state
  ]);

  useEffect(() => {
    if (step !== "building") return;
    if (agentRun.state.tag === "done") setPhaseStatus("agent", "done");
    if (agentRun.state.tag === "failed") {
      setPhaseStatus("agent", "failed");
      setBuildError(agentRun.state.message);
    }
  }, [step, agentRun.state.tag, setPhaseStatus, setBuildError, agentRun.state]);

  useEffect(() => {
    if (step !== "building") return;
    if (appRun.state.tag === "done") setPhaseStatus("app", "done");
    if (appRun.state.tag === "failed") {
      setPhaseStatus("app", "failed");
      setBuildError(appRun.state.message);
    }
  }, [step, appRun.state.tag, setPhaseStatus, setBuildError, appRun.state]);

  useEffect(() => {
    if (step !== "building") return;
    if (app2Run.state.tag === "done") setPhaseStatus("app2", "done");
    if (app2Run.state.tag === "failed") {
      setPhaseStatus("app2", "failed");
      setBuildError(app2Run.state.message);
    }
  }, [step, app2Run.state.tag, setPhaseStatus, setBuildError, app2Run.state]);

  // ── Polling fallback for stuck phases ─────────────────────────────────────
  // SSE streams can miss events (race between run completion and SSE connect).
  // Poll the run status via REST API every 5s for any phase stuck at "running".
  const builderThreadId = orchestrator.state.builderThreadId;
  useEffect(() => {
    if (step !== "building" || !builderThreadId || !projectId) return;

    const hasStuckPhase =
      phaseStatuses?.config === "running" ||
      phaseStatuses?.agent === "running" ||
      phaseStatuses?.app === "running" ||
      phaseStatuses?.app2 === "running";
    if (!hasStuckPhase) return;

    // Capture the phase maps at effect-registration time so each interval
    // tick uses a consistent snapshot. The effect re-runs when phaseRunIds
    // or phaseStatuses change, which refreshes these locals.
    const runIdToPhase = Object.entries(phaseRunIds ?? {}) as [
      import("./types").BuildPhase,
      string
    ][];
    const statusesSnapshot = phaseStatuses ?? {};

    const timer = setInterval(() => {
      AnalyticsService.getRunsByThread(projectId, builderThreadId)
        .then((runs) => {
          for (const run of runs) {
            const phase = runIdToPhase.find(([, id]) => id === run.run_id)?.[0];
            if (!phase) continue;
            if (statusesSnapshot[phase] !== "running") continue;
            if (run.status === "done") {
              setPhaseStatus(phase, "done");
            } else if (run.status === "failed" || run.status === "suspended") {
              setPhaseStatus(phase, "failed");
              setBuildError(run.error_message ?? "Build phase failed");
            }
          }
        })
        .catch(() => {}); // Ignore polling errors
    }, 5000);

    return () => clearInterval(timer);
  }, [step, builderThreadId, projectId, phaseStatuses, phaseRunIds, setPhaseStatus, setBuildError]);

  // ── View events for reasoning trace (grouped by run, not interleaved) ──────
  const viewEventBlocks = useMemo(
    () =>
      viewRunManager.events.map(
        (ev) =>
          ({
            seq: Number(ev.id ?? 0),
            event_type: ev.type,
            payload: ev.data
          }) as UiBlock
      ),
    [viewRunManager.events]
  );

  // ── Monotonic artifact accumulation ────────────────────────────────────────
  // Artifacts only grow — once a file is detected as created, it stays.
  // This prevents the progress bar / file checklist from flickering as events
  // arrive from parallel runs.
  const artifactMapRef = useRef(new Map<string, GeneratedArtifact>());
  const [accumulatedArtifacts, setAccumulatedArtifacts] = useState<GeneratedArtifact[]>([]);

  // Reset only when leaving the build flow entirely (going back, starting over).
  // Keep artifacts visible on the "complete" step so the right rail summary can
  // show what was created.
  useEffect(() => {
    if (step !== "building" && step !== "complete") {
      artifactMapRef.current.clear();
      setAccumulatedArtifacts([]);
    }
  }, [step]);

  // Scan all event sources and phase statuses, merge new artifacts
  useEffect(() => {
    if (step !== "building") return;
    const map = artifactMapRef.current;
    let changed = false;

    const addIfNew = (filePath: string) => {
      if (map.has(filePath)) return;
      map.set(filePath, { filePath, description: "", type: inferArtifactType(filePath) });
      changed = true;
    };

    // The HITL-gated write/edit/delete tool_result bodies are just
    // `{ answer: "Accept" }` — they don't carry `file_path`. The path lives
    // on the SSE `file_change_pending` / `file_changed` events emitted by
    // the builder domain (`BuilderEvent::FileChangePending` /
    // `BuilderEvent::FileChanged`), so we read `file_path` from those.
    const collectFromEvent = (ev: { type: string; data: unknown }) => {
      if (ev.type === "file_change_pending" || ev.type === "file_changed") {
        const fp = (ev.data as { file_path?: string })?.file_path;
        if (fp) addIfNew(fp);
      }
    };

    // Scan hook run events (config, agent, app, app2)
    for (const run of [configRun, agentRun, appRun, app2Run]) {
      if (!("events" in run.state)) continue;
      for (const ev of run.state.events) {
        collectFromEvent(ev);
      }
    }

    for (const ev of viewRunManager.events) {
      collectFromEvent(ev);
    }

    // Fallback for completed views
    const vs = orchestrator.state.viewRunStatuses ?? {};
    for (const table of selectedTables) {
      if (vs[table] === "done" || vs[table] === "failed") {
        addIfNew(`semantics/${table.split(".").pop() ?? table}.view.yml`);
      }
    }

    // Fallback for completed phases
    const ps = phaseStatuses ?? {};
    if (ps.config === "done") addIfNew("config.yml");
    if (ps.agent === "done") addIfNew("analytics.agentic.yml");
    if (ps.app === "done") addIfNew("apps/overview.app.yml");
    // No synthesized fallback for app2 — its filename is unpredictable
    // (single-topic deep-dive `apps/<topic>.app.yml` vs cross-topic
    // `apps/<topic1>_<topic2>.app.yml`), so only trust the actual
    // file-change event captured above. Synthesizing a guess would
    // inject a fictional path that never resolves on disk and
    // is not superseded by the real one (`addIfNew` is monotonic).

    if (changed) setAccumulatedArtifacts([...map.values()]);
  }, [
    step,
    configRun.state,
    agentRun.state,
    appRun.state,
    app2Run.state,
    viewRunManager.events,
    orchestrator.state.viewRunStatuses,
    selectedTables,
    phaseStatuses,
    configRun,
    agentRun,
    appRun,
    app2Run
  ]);

  // ── Complete once all parallel phases are done ──────────────────────────────
  useEffect(() => {
    // App2 only factors into completion when the workspace has ≥ 2 topics.
    // For single-topic workspaces it's neither started nor required.
    const app2Settled =
      !shouldBuildApp2 || phaseStatuses?.app2 === "done" || phaseStatuses?.app2 === "failed";
    if (
      step === "building" &&
      (phaseStatuses?.agent === "done" || phaseStatuses?.agent === "failed") &&
      (phaseStatuses?.app === "done" || phaseStatuses?.app === "failed") &&
      app2Settled &&
      !buildError
    ) {
      const knownFiles = deriveCreatedFiles(selectedTables, phaseStatuses);
      const artifactFiles = accumulatedArtifacts.map((a) => a.filePath);
      const files = [...new Set([...artifactFiles, ...knownFiles])];
      const questions =
        appRun.state.tag === "done" ? extractQuestionsFromAnswer(appRun.state.answer) : [];
      complete(files, questions);
    }
  }, [
    step,
    phaseStatuses,
    buildError,
    complete,
    accumulatedArtifacts,
    selectedTables,
    appRun.state,
    shouldBuildApp2
  ]);

  // ── Per-phase start/end time tracking ──────────────────────────────────────
  // Wall-clock timestamps let us render accurate elapsed time + fine-grained
  // progress bars for each sub-milestone. Semantic covers config + all views;
  // agent / app / app2 are tracked per-run.
  const [phaseTimings, setPhaseTimings] = useState<PhaseTimings>({});

  // Semantic start = first view run or config run kicks off (= step enters building
  // and the resync promise resolves). We anchor it to viewRunManager.isRunning or
  // configRun starting, whichever comes first.
  const semanticPhaseStarted =
    viewRunManager.isRunning ||
    configRun.state.tag === "running" ||
    configRun.state.tag === "suspended" ||
    phaseStatuses?.config === "running";
  const semanticPhaseDone = allViewsDone && configDone;

  useEffect(() => {
    if (step !== "building" && step !== "complete") return;
    setPhaseTimings((prev) => {
      let next = prev;
      const now = Date.now();

      const setStart = (key: SubPhaseKey) => {
        if (!next[key]?.start) {
          next = { ...next, [key]: { start: now } };
        }
      };
      const setEnd = (key: SubPhaseKey) => {
        const existing = next[key];
        if (existing?.start && !existing.end) {
          next = { ...next, [key]: { ...existing, end: now } };
        }
      };

      if (semanticPhaseStarted) setStart("semantic");
      if (semanticPhaseDone) setEnd("semantic");

      // Agent / app / app2 phases — track per run status
      const phaseMap: Array<[SubPhaseKey, "agent" | "app" | "app2"]> = [
        ["agent", "agent"],
        ["app", "app"],
        ...(shouldBuildApp2 ? ([["app2", "app2"]] as Array<[SubPhaseKey, "app2"]>) : [])
      ];
      for (const [key, statusKey] of phaseMap) {
        const status = phaseStatuses?.[statusKey];
        if (status === "running") setStart(key);
        if (status === "done" || status === "failed") {
          setStart(key); // in case we missed the start transition
          setEnd(key);
        }
      }

      return next === prev ? prev : next;
    });
  }, [step, semanticPhaseStarted, semanticPhaseDone, phaseStatuses, shouldBuildApp2]);

  // Reset timings when leaving the build flow entirely
  useEffect(() => {
    if (step !== "building" && step !== "complete") {
      setPhaseTimings({});
    }
  }, [step]);

  // ── Per-phase estimate baselines (FROZEN on first start) ───────────────────
  // Capturing at start and never mutating prevents the progress bar / remaining
  // countdown from going backwards when measured averages update mid-phase.
  const [estimateBaselines, setEstimateBaselines] = useState<Partial<Record<SubPhaseKey, number>>>(
    {}
  );

  // Calibrated against real runs: a 4-table ClickHouse workspace takes ~40–45s
  // for the semantic phase (config + 4 parallel view builds). We round up to
  // keep the countdown honest rather than optimistic; over-estimates are much
  // less frustrating than running past 0.
  const computeFreshBaseline = useCallback(
    (key: SubPhaseKey): number => {
      if (key === "semantic") {
        const concurrent = Math.min(10, Math.max(1, selectedTables.length || 1));
        const batches = Math.ceil((selectedTables.length || 1) / concurrent);
        // 40s per batch (one view in parallel) + 10s for config + inspection pre-pass.
        return Math.max(30, batches * 40 + 10);
      }
      if (key === "agent") return 45;
      if (key === "app") return 45; // richer starter dashboard with 2 charts + 2 tables
      return 35; // app2 — focused deep-dive (1 chart + 1 table) is faster
    },
    [selectedTables.length]
  );

  // Capture the baseline the first time a phase's timing registers a start.
  useEffect(() => {
    setEstimateBaselines((prev) => {
      let next = prev;
      const keys: SubPhaseKey[] = shouldBuildApp2
        ? ["semantic", "agent", "app", "app2"]
        : ["semantic", "agent", "app"];
      for (const key of keys) {
        if (phaseTimings[key]?.start && next[key] === undefined) {
          next = { ...next, [key]: computeFreshBaseline(key) };
        }
      }
      return next === prev ? prev : next;
    });
  }, [phaseTimings, computeFreshBaseline, shouldBuildApp2]);

  // Reset baselines when leaving the build flow
  useEffect(() => {
    if (step !== "building" && step !== "complete") {
      setEstimateBaselines({});
    }
  }, [step]);

  const baselineFor = useCallback(
    (key: SubPhaseKey): number => estimateBaselines[key] ?? computeFreshBaseline(key),
    [estimateBaselines, computeFreshBaseline]
  );

  // ── Live tick clock for elapsed times on running phases ─────────────────────
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (step !== "building") return;
    const anyRunning =
      !!phaseStatuses?.config ||
      !!phaseStatuses?.agent ||
      !!phaseStatuses?.app ||
      !!phaseStatuses?.app2 ||
      viewRunManager.isRunning;
    if (!anyRunning) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [step, phaseStatuses, viewRunManager.isRunning]);

  // Compute phase progress snapshots. Baseline is frozen once captured,
  // so elapsed / remaining / ratio all advance monotonically.
  const phaseProgress = useMemo<Record<SubPhaseKey, PhaseProgress>>(() => {
    const compute = (key: SubPhaseKey): PhaseProgress => {
      const timing = phaseTimings[key];
      const est = baselineFor(key);

      // Semantic: factor in real view-count progress so the bar doesn't stall
      // at the time-based estimate when views finish faster than expected.
      if (key === "semantic") {
        const totalViews = selectedTables.length;
        const doneViews = Object.values(viewRunStatuses ?? {}).filter(
          (s) => s === "done" || s === "failed"
        ).length;
        const actualDone = (configDone ? 1 : 0) + doneViews;
        const actualTotal = 1 + totalViews;
        const realRatio = actualTotal > 0 ? actualDone / actualTotal : 0;
        const isRunning = !semanticPhaseDone && !!timing?.start && !timing?.end;
        const elapsedMs = timing?.start
          ? (timing.end ?? (isRunning ? now : timing.start)) - timing.start
          : 0;
        const elapsedSeconds = Math.max(0, Math.floor(elapsedMs / 1000));
        const timeRatio = est > 0 ? Math.min(0.95, elapsedSeconds / est) : 0;
        const ratio = semanticPhaseDone ? 1 : Math.max(realRatio, timeRatio);
        return { ratio, elapsedSeconds, estimatedSeconds: est, isRunning };
      }

      const end = timing?.end;
      const start = timing?.start;
      if (!start) {
        return { ratio: 0, elapsedSeconds: 0, estimatedSeconds: est, isRunning: false };
      }
      const elapsedMs = (end ?? now) - start;
      const elapsedSeconds = Math.max(0, Math.floor(elapsedMs / 1000));
      const isRunning = !end;
      const ratio = end ? 1 : est > 0 ? Math.min(0.97, elapsedSeconds / est) : 0;
      return { ratio, elapsedSeconds, estimatedSeconds: est, isRunning };
    };
    return {
      semantic: compute("semantic"),
      agent: compute("agent"),
      app: compute("app"),
      app2: compute("app2")
    };
  }, [
    phaseTimings,
    baselineFor,
    now,
    selectedTables.length,
    viewRunStatuses,
    configDone,
    semanticPhaseDone
  ]);

  // ── Total build duration (for completion summary) ──────────────────────────
  const buildDurationMs = useMemo(() => {
    const starts = Object.values(phaseTimings)
      .map((t) => t?.start)
      .filter((v): v is number => typeof v === "number");
    const ends = Object.values(phaseTimings)
      .map((t) => t?.end)
      .filter((v): v is number => typeof v === "number");
    if (starts.length === 0) return 0;
    const minStart = Math.min(...starts);
    const maxEnd = ends.length > 0 ? Math.max(...ends) : now;
    return Math.max(0, maxEnd - minStart);
  }, [phaseTimings, now]);

  const railStateWithArtifacts = useMemo(
    () => ({
      ...orchestrator.railState,
      generatedArtifacts: accumulatedArtifacts,
      isBuildComplete: step === "complete"
    }),
    [orchestrator.railState, accumulatedArtifacts, step]
  );

  const previousStep = getPreviousStep(step);

  const [startOverOpen, setStartOverOpen] = useState(false);
  const resetManifest = useMemo(
    () => deriveResetManifest(orchestrator.state),
    [orchestrator.state]
  );

  // ── Stop / Retry in-progress build ─────────────────────────────────────────
  // Any phase can hang (mostly the semantic model) or fail mid-run. Stop is
  // the user's escape hatch: it aborts the SSE streams, fires best-effort
  // cancel requests on the backend, and flips any still-running phase/view
  // to "failed" so the existing Retry Build message surfaces. Retry reuses
  // the same tables / credentials and restarts all phases.
  const stopRunHook = useCallback((run: ReturnType<typeof useAnalyticsRun>) => {
    if (run.state.tag === "running" || run.state.tag === "suspended") {
      run.stop();
    }
  }, []);

  const { stopBuild } = orchestrator;
  const viewCancelAll = viewRunManager.cancelAll;
  const handleStopBuild = useCallback(() => {
    stopRunHook(configRun);
    stopRunHook(agentRun);
    stopRunHook(appRun);
    stopRunHook(app2Run);
    viewCancelAll();
    stopBuild("Build stopped.");
    // Effects can race, but the next retry path resets this explicitly.
    buildStartingRef.current = false;
  }, [stopRunHook, configRun, agentRun, appRun, app2Run, viewCancelAll, stopBuild]);

  const handleRetryBuild = useCallback(() => {
    // Abort any still-running phases before clearing state so a zombie SSE
    // can't land a late terminal event that dirties the new build run.
    handleStopBuild();
    // goToStep("building") from "building" resets phaseRunIds, phaseStatuses,
    // viewRunIds, viewRunStatuses, createdFiles, sampleQuestions, buildError.
    goToStep("building");
    // The build effect guards on this ref; without resetting it here the
    // effect's other gate (`!phaseStatuses?.config`) re-opens but the ref
    // gate stays closed and the retry silently no-ops.
    buildStartingRef.current = false;
  }, [handleStopBuild, goToStep]);

  const configTag = configRun.state.tag;
  const agentTag = agentRun.state.tag;
  const appTag = appRun.state.tag;
  const app2Tag = app2Run.state.tag;
  const hasInFlightBuild = useMemo(() => {
    const ps = phaseStatuses ?? {};
    if (
      ps.config === "running" ||
      ps.agent === "running" ||
      ps.app === "running" ||
      ps.app2 === "running"
    ) {
      return true;
    }
    const vs = viewRunStatuses ?? {};
    if (Object.values(vs).some((s) => s === "running")) return true;
    if (viewRunManager.isRunning) return true;
    // Any SSE stream actively attached — covers the brief window between
    // startBuildPhase resolving and START_PHASE landing in orchestrator state.
    return [configTag, agentTag, appTag, app2Tag].some((t) => t === "running" || t === "suspended");
  }, [
    phaseStatuses,
    viewRunStatuses,
    viewRunManager.isRunning,
    configTag,
    agentTag,
    appTag,
    app2Tag
  ]);

  const handleStartOverConfirmed = useCallback(async () => {
    // Best-effort cancel any in-flight phase runs first, so they don't race
    // the revert by writing new files after we've cleaned up. Capped at 5s
    // total — a long-running sync on the backend can make `cancelRun` slow
    // to respond, and we'd rather proceed with the reset than leave the
    // user staring at "Starting over…" indefinitely.
    if (projectId) {
      const runIds = [
        ...Object.values(orchestrator.state.phaseRunIds ?? {}),
        ...Object.values(orchestrator.state.viewRunIds ?? {})
      ].filter((id): id is string => Boolean(id));
      if (runIds.length > 0) {
        const cancels = Promise.allSettled(
          runIds.map((id) => AnalyticsService.cancelRun(projectId, id))
        );
        const timeout = new Promise((resolve) => setTimeout(resolve, 5000));
        await Promise.race([cancels, timeout]);
      }
    }

    // Revert server-side side effects. Each entry is handled idempotently by
    // the backend, so missing entries are silently skipped.
    if (
      projectId &&
      (resetManifest.secret_names.length > 0 ||
        resetManifest.database_names.length > 0 ||
        resetManifest.model_names.length > 0 ||
        resetManifest.file_paths.length > 0 ||
        resetManifest.directory_paths.length > 0)
    ) {
      try {
        const result = await OnboardingService.resetOnboarding(projectId, resetManifest);
        if (result.warnings.length > 0) {
          toast.warning(
            `Start over completed with ${result.warnings.length} warning${
              result.warnings.length === 1 ? "" : "s"
            }. Check the server logs for details.`
          );
        }
      } catch (err) {
        console.error("[onboarding] Failed to reset onboarding side effects:", err);
        toast.error(
          "Failed to fully reset onboarding. Some files, secrets, or database entries may remain."
        );
      }
    }

    // Reset to a fresh state but keep the workspace's storage_key so the
    // home-page guard still redirects back here until onboarding completes.
    if (storageKeyForReset) initOnboardingStateForStorageKey(storageKeyForReset);
    window.location.reload();
  }, [
    projectId,
    storageKeyForReset,
    orchestrator.state.phaseRunIds,
    orchestrator.state.viewRunIds,
    resetManifest
  ]);

  return (
    <div className='flex h-full flex-col'>
      {/* Header */}
      <div className='flex items-center gap-2 border-border border-b px-4 py-2'>
        {/* Escape hatch: the rail is hidden on this surface and Home bounces an
            incomplete workspace back here, so the org switcher is the only way
            to reach another org or create a new one from setup. */}
        <OrgSwitcher />
        <div className='h-2 w-2 rounded-full bg-primary' />
        <span className='font-medium text-sm'>Oxygen Setup</span>
        <span className='flex-1 text-muted-foreground text-xs'>Setting up your workspace</span>
        {previousStep && (
          <button
            type='button'
            onClick={() => goToStep(previousStep)}
            className='text-muted-foreground text-xs hover:text-foreground'
          >
            &larr; Back
          </button>
        )}
        {step === "building" && hasInFlightBuild && !buildError && (
          <button
            type='button'
            onClick={handleStopBuild}
            className='text-destructive text-xs hover:underline'
          >
            Stop build
          </button>
        )}
        {step !== "welcome" && step !== "complete" && (
          <button
            type='button'
            onClick={() => setStartOverOpen(true)}
            className='text-muted-foreground text-xs hover:text-foreground'
          >
            Start over
          </button>
        )}
        {step !== "complete" && (
          <CancelSetupButton workspaceId={projectId} onBeforeDelete={handleStopBuild} />
        )}
        <button
          type='button'
          onClick={() => {
            // Mark setup deferred so Home doesn't immediately bounce back into
            // the wizard (the missing-credential check drives that redirect).
            // The workspace still surfaces a "finish setup" row on Home and the
            // wizard stays reachable — same mechanism as the in-thread Skip.
            if (orchestrator.state.storageKey) {
              markOnboardingDismissedForStorageKey(orchestrator.state.storageKey);
            }
            navigate(ROUTES.ROOT);
          }}
          aria-label='Go home'
          title='Go home — setup is saved; finish it from Home later'
          className='inline-flex items-center gap-1 text-muted-foreground text-xs hover:text-foreground'
        >
          <Home className='h-3.5 w-3.5' />
          Home
        </button>
      </div>

      <StartOverConfirmDialog
        open={startOverOpen}
        onOpenChange={setStartOverOpen}
        manifest={resetManifest}
        onConfirm={handleStartOverConfirmed}
      />

      {/* Main content: two layout modes.
          Building: resizable split panel (thread + full-height progress rail).
          Complete: single-column (no rail). The setup summary moves inline
          into the CompletionCard so the page reads as one coherent surface. */}
      {step === "complete" ? (
        <div className='flex-1 overflow-hidden'>
          <OnboardingThread
            orchestrator={orchestrator}
            actions={actions}
            semanticRun={configRun}
            agentRun={agentRun}
            appRun={appRun}
            app2Run={app2Run}
            includeApp2={shouldBuildApp2}
            builderActivityItems={builderActivityItems}
            viewEvents={viewEventBlocks}
            viewsRunning={viewRunManager.isRunning}
            phaseProgress={phaseProgress}
            buildElapsedMs={buildDurationMs}
            railState={railStateWithArtifacts}
            phaseTimings={phaseTimings}
          />
        </div>
      ) : (
        <ResizablePanelGroup direction={isMobile ? "vertical" : "horizontal"} className='flex-1'>
          <ResizablePanel defaultSize={60} minSize={40}>
            <OnboardingThread
              orchestrator={orchestrator}
              actions={actions}
              semanticRun={configRun}
              agentRun={agentRun}
              appRun={appRun}
              app2Run={app2Run}
              includeApp2={shouldBuildApp2}
              builderActivityItems={builderActivityItems}
              viewEvents={viewEventBlocks}
              viewsRunning={viewRunManager.isRunning}
              phaseProgress={phaseProgress}
              buildElapsedMs={buildDurationMs}
              onRetryBuild={handleRetryBuild}
            />
          </ResizablePanel>

          <ResizableHandle withHandle />

          <ResizablePanel defaultSize={40} minSize={25} maxSize={50}>
            <OnboardingRightRail
              railState={railStateWithArtifacts}
              buildDurationMs={buildDurationMs}
              phaseTimings={phaseTimings}
            />
          </ResizablePanel>
        </ResizablePanelGroup>
      )}
    </div>
  );
}

// ── GitHub-import flow ──────────────────────────────────────────────────────
//
// Runs a much smaller version of the onboarding UI: the user walks through the
// secrets (LLM API keys, warehouse credentials) the cloned repo's config.yml
// actually references, then lands on a simplified completion screen that
// surfaces the repo's own apps + agents as call-to-actions. No semantic-model
// build, no selection-cards for model/warehouse-type (those are dictated by
// the repo).

function GithubOnboardingPage({ orchestrator }: { orchestrator: OrchestratorHandle }) {
  const actions = useOnboardingActions(orchestrator);
  const { project } = useCurrentProjectBranch();
  const projectId = project?.id ?? "";
  const projectStorageKey = project?.storage_key ?? project?.id ?? "";
  const { isMobile } = useSidebar();
  const navigate = useNavigate();

  // Analytics-run stubs — unused in github mode but required by `OnboardingThread`'s
  // props. They stay in their initial `idle` state and never stream.
  const semanticRun = useAnalyticsRun({ projectId });
  const agentRun = useAnalyticsRun({ projectId });
  const appRun = useAnalyticsRun({ projectId });
  const app2Run = useAnalyticsRun({ projectId });

  const { step, storageKey: onboardingStorageKey } = orchestrator.state;
  const { fetchGithubSetup } = actions;
  const { setGithubSetup } = orchestrator;

  // Load the missing-secrets manifest on mount; any failure falls through as
  // an empty setup ("nothing to configure"), which auto-advances to complete.
  // Depending on `orchestrator` directly would re-fire on every render (it's a
  // fresh object from the hook); pull out the one method we use so the dep
  // array only tracks stable references.
  //
  // Gate the fetch on the active workspace matching the onboarding-state
  // workspace: right after navigating from another workspace, the Zustand
  // useCurrentWorkspace store hasn't caught up to the new URL yet, so
  // `projectId` briefly points at the previous workspace. Fetching with that
  // stale id returns an empty manifest (those secrets were already saved in
  // the prior onboarding), which would auto-advance this flow past every
  // prompt straight to "complete".
  useEffect(() => {
    if (step !== "github_loading") return;
    if (!projectStorageKey || projectStorageKey !== onboardingStorageKey) return;
    let cancelled = false;
    fetchGithubSetup().catch((err: unknown) => {
      if (cancelled) return;
      console.error("[onboarding] Failed to load github setup", err);
      toast.error("Could not read config.yml. You can still open the workspace.");
      setGithubSetup({ missing_llm_key_vars: [], warehouses: [] });
    });
    return () => {
      cancelled = true;
    };
  }, [step, fetchGithubSetup, setGithubSetup, projectStorageKey, onboardingStorageKey]);

  const [startOverOpen, setStartOverOpen] = useState(false);
  const resetManifest = useMemo<OnboardingResetRequest>(() => {
    const setup = orchestrator.state.githubSetup;
    // Only offer to delete secrets the user actually provided during this
    // flow. We never touch files/databases/models — those come from the
    // cloned repo, not onboarding.
    const secret_names: string[] = [];
    for (const k of setup?.missing_llm_key_vars ?? []) {
      secret_names.push(k.var_name);
    }
    for (const w of setup?.warehouses ?? []) {
      for (const v of w.missing_vars) {
        secret_names.push(v.var_name);
      }
    }
    return {
      secret_names,
      database_names: [],
      model_names: [],
      file_paths: [],
      directory_paths: []
    };
  }, [orchestrator.state.githubSetup]);

  const mode = orchestrator.state.mode ?? "github";
  const handleStartOverConfirmed = useCallback(async () => {
    if (projectId && resetManifest.secret_names.length > 0) {
      try {
        await OnboardingService.resetOnboarding(projectId, resetManifest);
      } catch (err) {
        console.error("[onboarding] Failed to reset onboarding secrets", err);
        toast.error("Failed to fully reset onboarding. Some secrets may remain.");
      }
    }
    if (projectStorageKey) initOnboardingStateForStorageKey(projectStorageKey, mode);
    window.location.reload();
  }, [projectId, projectStorageKey, resetManifest, mode]);

  const railState = orchestrator.railState;
  const headerSubtitle =
    mode === "demo" ? "Setting up your demo workspace" : "Connecting your repository";

  return (
    <div className='flex h-full flex-col'>
      <div className='flex items-center gap-2 border-border border-b px-4 py-2'>
        {/* Escape hatch — see BlankOnboardingPage header. */}
        <OrgSwitcher />
        <div className='h-2 w-2 rounded-full bg-primary' />
        <span className='font-medium text-sm'>Oxygen Setup</span>
        <span className='flex-1 text-muted-foreground text-xs'>{headerSubtitle}</span>
        {step !== "github_loading" && step !== "complete" && (
          <button
            type='button'
            onClick={() => setStartOverOpen(true)}
            className='text-muted-foreground text-xs hover:text-foreground'
          >
            Start over
          </button>
        )}
        {step !== "complete" && <CancelSetupButton workspaceId={projectId} />}
        <button
          type='button'
          onClick={() => {
            // Mark setup deferred so Home doesn't immediately bounce back into
            // the wizard (the missing-credential check drives that redirect).
            // The workspace still surfaces a "finish setup" row on Home and the
            // wizard stays reachable — same mechanism as the in-thread Skip.
            if (orchestrator.state.storageKey) {
              markOnboardingDismissedForStorageKey(orchestrator.state.storageKey);
            }
            navigate(ROUTES.ROOT);
          }}
          aria-label='Go home'
          title='Go home — setup is saved; finish it from Home later'
          className='inline-flex items-center gap-1 text-muted-foreground text-xs hover:text-foreground'
        >
          <Home className='h-3.5 w-3.5' />
          Home
        </button>
      </div>

      <StartOverConfirmDialog
        open={startOverOpen}
        onOpenChange={setStartOverOpen}
        manifest={resetManifest}
        onConfirm={handleStartOverConfirmed}
      />

      {step === "complete" ? (
        <div className='flex-1 overflow-hidden'>
          <OnboardingThread
            orchestrator={orchestrator}
            actions={actions}
            semanticRun={semanticRun}
            agentRun={agentRun}
            appRun={appRun}
            app2Run={app2Run}
            includeApp2={false}
            builderActivityItems={[]}
            railState={railState}
          />
        </div>
      ) : (
        <ResizablePanelGroup direction={isMobile ? "vertical" : "horizontal"} className='flex-1'>
          <ResizablePanel defaultSize={60} minSize={40}>
            <OnboardingThread
              orchestrator={orchestrator}
              actions={actions}
              semanticRun={semanticRun}
              agentRun={agentRun}
              appRun={appRun}
              app2Run={app2Run}
              includeApp2={false}
              builderActivityItems={[]}
            />
          </ResizablePanel>

          <ResizableHandle withHandle />

          <ResizablePanel defaultSize={40} minSize={25} maxSize={50}>
            <OnboardingRightRail railState={railState} />
          </ResizablePanel>
        </ResizablePanelGroup>
      )}
    </div>
  );
}

// ── Helpers ──────────────────────────────────────────────────────────────────

function isAutoAcceptSuspension(questions: HumanInputQuestion[]): boolean {
  return questions.some((q) => {
    try {
      const parsed = JSON.parse(q.prompt);
      return parsed.type === "file_change";
    } catch {
      return q.suggestions?.some(
        (s) => s.toLowerCase() === "accept" || s.toLowerCase() === "reject"
      );
    }
  });
}

/** Extract a user-facing error message from non-auto-acceptable suspension questions. */
function extractSuspensionError(questions: HumanInputQuestion[]): string {
  for (const q of questions) {
    try {
      const parsed = JSON.parse(q.prompt);
      if (parsed.message) return parsed.message;
    } catch {
      // Plain text prompt — use as-is
      if (q.prompt) return q.prompt;
    }
  }
  return "Build failed due to an unexpected error";
}

/** Infer the artifact type from a file path. */
function inferArtifactType(filePath: string): GeneratedArtifact["type"] {
  if (filePath.endsWith(".view.yml")) return "view";
  if (filePath.endsWith(".app.yml")) return "app";
  if (filePath.endsWith(".topic.yml")) return "topic";
  if (filePath.endsWith(".agentic.yml") || filePath.endsWith(".agentic.yaml")) return "agentic";
  return "config";
}

/** Derive the expected file list from completed phase statuses + selected tables. */
function deriveCreatedFiles(
  selectedTables: string[],
  phaseStatuses?: Partial<Record<string, string>>
): string[] {
  const files: string[] = [];
  const ps = phaseStatuses ?? {};
  if (ps.config === "done") files.push("config.yml");
  for (const table of selectedTables) {
    const name = table.split(".").pop() ?? table;
    // Each selected table gets a matching .view.yml + .topic.yml pair (see
    // `crates/agentic/builder/src/onboarding.rs`).
    files.push(`semantics/${name}.view.yml`);
    files.push(`semantics/${name}.topic.yml`);
  }
  if (ps.agent === "done") files.push("analytics.agentic.yml");
  if (ps.app === "done") files.push("apps/overview.app.yml");
  // app2 is intentionally omitted — its filename is unpredictable.
  // The caller merges this list with `accumulatedArtifacts`, which already
  // has the actual app2 path captured from the file-change event.
  return files;
}

/**
 * Derive the "Start over" reset manifest from the current onboarding state.
 *
 * Includes every side effect that onboarding *may* have applied so far: the
 * LLM key secret, the warehouse entry (which carries its password secret on
 * the backend), the model entry (which carries its key_var secret), any
 * generated file that was either tracked via `createdFiles` or is expected
 * given the user's table selection, and the `.databases/<warehouse>/` sync
 * directory. The backend skips missing entries, so being conservative here is
 * safe.
 */
function deriveResetManifest(state: OnboardingState): OnboardingResetRequest {
  const secret_names: string[] = [];
  if (state.llmProvider) {
    secret_names.push(LLM_KEY_VAR[state.llmProvider]);
  }

  const database_names: string[] = [];
  if (state.warehouseType) {
    // Warehouse entries are named after the warehouse type (see
    // `buildWarehouseConfig` in useOnboardingActions.ts).
    database_names.push(state.warehouseType);
  }

  const model_names: string[] = [];
  if (state.llmModel) {
    // The config writer uses the user-facing model name as the entry's `name`.
    model_names.push(state.llmModel);
  }

  const fileSet = new Set<string>();
  // Files the builder actually reported as created (populated on COMPLETE).
  for (const f of state.createdFiles ?? []) {
    if (f && f !== "config.yml") fileSet.add(f);
  }
  // Files that would be created given the current state (covers partial runs
  // where COMPLETE never fired).
  for (const f of deriveCreatedFiles(state.selectedTables, state.phaseStatuses)) {
    if (f !== "config.yml") fileSet.add(f);
  }

  // Recursively delete `.databases/<warehouse>/` — DatabaseService.syncDatabase
  // writes per-table `.schema.yml` files here that `remove_database` does not
  // touch.
  const directory_paths: string[] = [];
  if (state.warehouseType) {
    directory_paths.push(`.databases/${state.warehouseType}`);
  }

  // DuckDB upload path: remove the directory the uploaded CSV/Parquet files
  // were written to (default `.db/`). Only include when we actually uploaded
  // — a user who typed an existing path should never have it deleted.
  if (state.warehouseType === "duckdb" && (state.uploadedWarehouseFiles?.length ?? 0) > 0) {
    const subdir = state.uploadedWarehouseSubdir ?? ".db";
    if (!directory_paths.includes(subdir)) {
      directory_paths.push(subdir);
    }
  }

  return {
    secret_names,
    database_names,
    model_names,
    file_paths: [...fileSet].sort(),
    directory_paths
  };
}

/** Extract sample questions from the builder's summary text. */
function extractQuestionsFromAnswer(answer: string): string[] {
  if (!answer) return [];
  const questions: string[] = [];
  for (const line of answer.split(/\n/)) {
    const trimmed = line.trim();
    const match = trimmed.match(/^(?:\d+[.)]\s*|[-*]\s+)(.+\?)\s*$/);
    if (match) {
      questions.push(match[1].trim());
      continue;
    }
    if (trimmed.endsWith("?") && trimmed.length > 15 && !trimmed.startsWith("#")) {
      questions.push(trimmed);
    }
  }
  return questions.slice(0, 3);
}
