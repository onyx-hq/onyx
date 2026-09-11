import "@/styles/shadcn/index.css";
import {
  createBrowserRouter,
  createRoutesFromElements,
  Navigate,
  Outlet,
  Route,
  RouterProvider,
  Routes,
  useNavigate,
  useParams,
  useSearchParams
} from "react-router-dom";
import { Button } from "@/components/ui/shadcn/button";
import { SidebarProvider } from "@/components/ui/shadcn/sidebar";
import { Toaster as ShadcnToaster } from "@/components/ui/shadcn/sonner";
import AirwayPage from "@/pages/airway";
import AutomationPage from "@/pages/automation";
import AutomationsListPage from "@/pages/automation/AutomationsListPage";
import ChatPage from "@/pages/chat";
import LauncherPage from "@/pages/launcher";
import ThreadPage from "@/pages/thread";
import "@xyflow/react/dist/style.css";
import React, { Suspense, useEffect, useRef } from "react";
import { HotkeysProvider, useHotkeys } from "react-hotkeys-hook";
import { toast } from "sonner";
import { RouteErrorBoundary } from "@/components/RouteErrorBoundary";
import { Spinner } from "@/components/ui/shadcn/spinner";
import ROUTES from "@/libs/utils/routes";
import ContextGraphPage from "@/pages/context-graph";
import WorldModelView from "@/pages/ide/WorldModel";
import { ErrorBoundary } from "@/sentry";
import { ActingShell } from "./components/admin/ActingShell";
import { BuilderDialog } from "./components/BuilderDialog";
import { FileQuickOpen } from "./components/FileQuickOpen";
import OrgGuard from "./components/OrgGuard";
import OrgSubdomainAuthGate from "./components/OrgSubdomainAuthGate";
import OwnerRedirect from "./components/OwnerRedirect";
import ProtectedRoute from "./components/ProtectedRoute";
import { WorkspaceShell } from "./components/Shell/WorkspaceShell";
import SettingsDialog from "./components/settings/SettingsDialog";
import { useSettingsDeepLink } from "./components/settings/SettingsDialog/useSettingsDeepLink";
import AgenticSetupPage from "./components/workspaces/components/CreateWorkspaceDialog/components/AgenticSetup";
import { LocalWorkspaceSetupDialog } from "./components/workspaces/components/LocalWorkspaceSetupDialog";
import { ManageWorkspacesDialog } from "./components/workspaces/components/ManageWorkspacesDialog";
import { AuthProvider, useAuth } from "./contexts/AuthContext";
import { useWorkspace } from "./hooks/api/workspaces/useWorkspaces";
import useAuthConfig from "./hooks/auth/useAuthConfig";
import useOauthConnectReturn from "./hooks/useOauthConnectReturn";
import useVersionCheck from "./hooks/useVersionCheck";
import { LOCAL_WORKSPACE_ID } from "./libs/utils/constants";
import { setLastWorkspaceId } from "./libs/utils/lastWorkspace";
import AppPage from "./pages/app";
import CliAuth from "./pages/auth/CliAuth";
import DevLogin from "./pages/auth/DevLogin";
import GoogleCallback from "./pages/auth/GoogleCallback";
import MagicLinkCallback from "./pages/auth/MagicLinkCallback";
import OktaCallback from "./pages/auth/OktaCallback";
import DashboardsPage from "./pages/dashboards";
import GitHubCallback from "./pages/github/callback";
import InvitePage from "./pages/Invite";
import LoginPage from "./pages/login";
import OrgDispatcher from "./pages/OrgDispatcher";
import OnboardingPage from "./pages/onboarding";
import PostLoginDispatcher from "./pages/PostLoginDispatcher";
import QuickBooksConnected from "./pages/quickbooks/QuickBooksConnected";
import useAskDock from "./stores/useAskDock";
import useBuilderDialog from "./stores/useBuilderDialog";
import useCurrentOrg from "./stores/useCurrentOrg";
import useCurrentWorkspace from "./stores/useCurrentWorkspace";
import { useDatabaseOperationStore } from "./stores/useDatabaseOperation";
import useFileQuickOpen from "./stores/useFileQuickOpen";
import useSettingsDialog from "./stores/useSettingsDialog";
import type { AuthConfigResponse } from "./types/auth";

// Lazy-load the entire IDE subtree. The IDE pulls in Monaco, monaco-yaml,
// and the editors vendor chunk (~300-400KB gzipped); users who never visit
// /ide should not pay for it on initial load.
const IdePage = React.lazy(() => import("./pages/ide"));

const FilesLayout = React.lazy(() => import("./pages/ide/Files"));
const EditorPage = React.lazy(() => import("./pages/ide/Files/Editor"));
const LookerExplorerPage = React.lazy(() => import("./pages/ide/Files/Editor/LookerExplore"));
const DatabaseLayout = React.lazy(() => import("./pages/ide/Database"));
const QueryWorkspacePage = React.lazy(() => import("./pages/ide/Database/QueryWorkspace"));
const ModelingPage = React.lazy(() => import("./pages/ide/modeling"));
const SemanticLayerPage = React.lazy(() => import("./pages/ide/SemanticLayer"));
const EdgeLayout = React.lazy(() => import("./pages/ide/edge"));
const EdgeDashboardPage = React.lazy(() => import("./pages/ide/edge/DashboardPage"));
const EdgePlaybackPage = React.lazy(() => import("./pages/ide/edge/PlaybackPage"));
const EdgeDetectionsPage = React.lazy(() => import("./pages/ide/edge/DetectionsPage"));
const EdgeTopologyPage = React.lazy(() => import("./pages/ide/edge/TopologyPage"));
const EdgeDevicesPage = React.lazy(() => import("./pages/ide/edge/DevicesPage"));
const EdgeBoxDetailPage = React.lazy(() => import("./pages/ide/edge/EdgeBoxDetailPage"));
// EdgeTimelinePage is gone — Detections + Playback now own the surface.
// `/ide/edge/timeline` redirects to /playback for back-compat bookmarks.
const EdgeRolloutsPage = React.lazy(() => import("./pages/ide/edge/RolloutsPage"));
const EdgeRolloutDetailPage = React.lazy(() => import("./pages/ide/edge/RolloutDetailPage"));
const EdgeAuditPage = React.lazy(() => import("./pages/ide/edge/AuditPage"));
const EdgePackPage = React.lazy(() => import("./pages/ide/edge/PackPage"));
const TestsLayout = React.lazy(() => import("./pages/ide/tests"));
const TestsDashboardPage = React.lazy(() => import("./pages/ide/tests/TestsDashboardPage"));
const TestsRunsPage = React.lazy(() => import("./pages/ide/tests/TestsRunsPage"));
const TestFileDetailPage = React.lazy(() => import("./pages/ide/tests/TestFileDetailPage"));
const CoordinatorLayout = React.lazy(() => import("./pages/ide/coordinator"));
const OverviewPage = React.lazy(() => import("./pages/ide/coordinator/Overview"));
const JobsPage = React.lazy(() => import("./pages/ide/coordinator/Jobs"));
const JobDetailPage = React.lazy(() => import("./pages/ide/coordinator/Jobs/JobDetail"));
const RunsPage = React.lazy(() => import("./pages/ide/coordinator/Runs"));
const RunDetailPage = React.lazy(() => import("./pages/ide/coordinator/Runs/RunDetail"));
const RecoveryPage = React.lazy(() => import("./pages/ide/coordinator/Recovery"));
const QueueHealthPage = React.lazy(() => import("./pages/ide/coordinator/QueueHealth"));
const ObservabilityLayout = React.lazy(() => import("./pages/ide/observability"));
const ExecutionAnalytics = React.lazy(
  () => import("./pages/ide/observability/execution-analytics")
);
const ClusterMapPage = React.lazy(() => import("./pages/ide/observability/clusters"));
const MetricDetailPage = React.lazy(
  () => import("./pages/ide/observability/metrics/MetricsDetailPage")
);
const MetricsPage = React.lazy(() => import("./pages/ide/observability/metrics/MetricsListPage"));
const TraceDetailPage = React.lazy(() => import("./pages/ide/observability/trace"));
const TracesPage = React.lazy(() => import("./pages/ide/observability/traces"));

// Admin and Stripe return URLs are visited rarely; defer their bundles too.
const AdminLayout = React.lazy(() => import("./pages/admin/AdminLayout"));
// Partner console — same shell as admin, fewer capabilities.
const PartnerLayout = React.lazy(() => import("./pages/partners/PartnerLayout"));
const PartnerClients = React.lazy(() => import("./pages/partners/PartnerClients"));
const PartnerCustomApps = React.lazy(() => import("./pages/partners/PartnerCustomApps"));
const PartnerTeam = React.lazy(() => import("./pages/partners/PartnerTeam"));
const PartnerActivity = React.lazy(() => import("./pages/partners/PartnerActivity"));
const AdminBillingQueue = React.lazy(() => import("./pages/admin/AdminBillingQueue"));
const AdminFeatureFlags = React.lazy(() => import("./pages/admin/AdminFeatureFlags"));
const AdminInternalJobs = React.lazy(() => import("./pages/admin/AdminInternalJobs"));
const AdminCompiles = React.lazy(() => import("./pages/admin/AdminCompiles"));
const AdminExplorer = React.lazy(() => import("./pages/admin/AdminExplorer"));
const AdminAudit = React.lazy(() => import("./pages/admin/AdminAudit"));
const AdminAirhouse = React.lazy(() => import("./pages/admin/AdminAirhouse"));
// Customer-apps admin surface (new-auth): per-org app admins + the
// customer-apps registry (Add / Link / Sync / Publish). Lazy-loaded
// alongside the rest of admin since most users never visit it.
const AdminAppAdmins = React.lazy(() => import("./pages/admin/AdminAppAdmins"));
const AdminPublishTokens = React.lazy(() => import("./pages/admin/AdminPublishTokens"));
const AdminCustomApps = React.lazy(() => import("./pages/admin/AdminCustomApps"));
const AppDossierWindow = React.lazy(() => import("./pages/admin/AdminCustomApps/AppDossierWindow"));
// Tenant-management admin surfaces (OXY_OWNER-only). Lazy-loaded alongside
// the rest of admin since most users never visit /admin/* at all.
const AdminTenants = React.lazy(() => import("./pages/admin/AdminTenants"));
const AdminTenantsCockpit = React.lazy(() => import("./pages/admin/AdminTenantsCockpit"));
const AdminOrgs = React.lazy(() => import("./pages/admin/AdminOrgs"));
const AdminOltp = React.lazy(() => import("./pages/admin/AdminOltp"));
const AdminOrgDetail = React.lazy(() => import("./pages/admin/AdminOrgs/AdminOrgDetail"));
const AdminUsers = React.lazy(() => import("./pages/admin/AdminUsers"));
const AdminUserDetail = React.lazy(() => import("./pages/admin/AdminUsers/AdminUserDetail"));
const AdminWorkspaces = React.lazy(() => import("./pages/admin/AdminWorkspaces"));
const AdminWorkspaceDetail = React.lazy(
  () => import("./pages/admin/AdminWorkspaces/AdminWorkspaceDetail")
);
const AdminWorkspaceHealth = React.lazy(() => import("./pages/admin/AdminWorkspaceHealth"));
const AdminAirway = React.lazy(() => import("./pages/admin/AdminAirway"));

const CheckoutSuccessPage = React.lazy(() => import("./pages/billing/CheckoutSuccess"));
const CheckoutCancelledPage = React.lazy(() => import("./pages/billing/CheckoutCancelled"));

const RouteFallback = () => (
  <div className='flex h-full w-full items-center justify-center'>
    <Spinner className='size-6' />
  </div>
);

// Last workspace WorkspaceLayout rendered. Deliberately module-scoped rather
// than a `useRef`: the chrome this guards (`useAskDock`, `useBuilderDialog`,
// `useDatabaseOperationStore`) lives in module-level zustand stores that
// outlive any unmount, so the marker has to outlive it too. A component ref
// is re-seeded whenever WorkspaceLayout unmounts between two workspaces —
// A → an org root / `/admin` / `/partners` / a post-login bounce → B — and
// the reset below would silently never fire on that path (#2962).
let lastSeenWsId: string | undefined;

const WorkspaceLayout = React.memo(function WorkspaceLayout() {
  const { authConfig, isLocalMode } = useAuth();
  const { wsId: wsIdParam } = useParams<{ wsId: string }>();
  const orgSlug = useCurrentOrg((s) => s.org?.slug);
  const orgId = useCurrentOrg((s) => s.org?.id);
  const navigate = useNavigate();

  // In local mode the router doesn't carry a :wsId segment — the single
  // implicit workspace is addressed by the nil UUID.
  const wsId = isLocalMode ? LOCAL_WORKSPACE_ID : wsIdParam;
  // biome-ignore lint/style/noNonNullAssertion: local gets the constant, cloud gets the :wsId param
  const workspaceQuery = useWorkspace(wsId!);
  const { isPending, isError, error, data, isMaterializing, materializingTimedOut, refetch } =
    workspaceQuery;
  const { setWorkspace, workspace } = useCurrentWorkspace();

  // Settings → Connections leaves the SPA for the provider's consent screen and
  // returns with ?oxy_connected=ok; this reopens the dialog and confirms it.
  useOauthConnectReturn();
  const { setIsOpen: setBuilderDialogOpen } = useBuilderDialog();
  const { setIsOpen: setFileQuickOpenOpen } = useFileQuickOpen();
  useHotkeys("meta+i", () => setBuilderDialogOpen(!useBuilderDialog.getState().isOpen), {
    preventDefault: true,
    useKey: true
  });
  useHotkeys("meta+p", () => setFileQuickOpenOpen(true), { preventDefault: true, useKey: true });
  // react-hotkeys-hook's useKey matching misfired on bare "k" (closing the
  // panel mid-typing); bind ⌘K/Ctrl+K manually with explicit modifier checks.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key.toLowerCase() !== "k" || !(e.metaKey || e.ctrlKey)) return;
      e.preventDefault();
      if (useBuilderDialog.getState().isOpen || useFileQuickOpen.getState().isOpen) return;
      useAskDock.getState().toggle();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  // After a successful Slack install, the backend redirects the browser to
  // /<orgSlug>?slack_installed=ok. Detect the param, surface a toast, pop
  // open the settings dialog on the Integration tab, and strip the param so
  // a refresh doesn't re-fire the toast. (Lived in the cloud-only sidebar
  // footer until the sidebar was removed; WorkspaceLayout matches its mount
  // surface, and the local-mode guard matches its cloud-only scope.)
  // `?settings=<section>` — the link the custom-app shell emits, and any
  // other deep link into the dialog. Read once, then stripped.
  useSettingsDeepLink();
  const [searchParams, setSearchParams] = useSearchParams();
  const openSettingsDialog = useSettingsDialog((s) => s.open);
  useEffect(() => {
    if (isLocalMode) return;
    if (searchParams.get("slack_installed") !== "ok") return;
    toast.success("Slack connected");
    openSettingsDialog("organization.integration");
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        next.delete("slack_installed");
        return next;
      },
      { replace: true }
    );
  }, [isLocalMode, searchParams, setSearchParams, openSettingsDialog]);

  React.useEffect(() => {
    if (!isPending && !isError && data) {
      setWorkspace(data);
    }
  }, [isPending, isError, setWorkspace, data]);

  // Several pieces of chrome are deliberately kept mounted across route
  // changes (the Ask dock, the ⌘I builder dialog) so collapsing/switching
  // pages never loses a draft — but that also means they survive a
  // workspace switch, since only the `:wsId` route param changes.
  // `wsId` (from `useParams`) changes synchronously with navigation, so this
  // is a more robust place for the reset than deriving it from
  // `useCurrentWorkspace` (which only updates once the `setWorkspace(data)`
  // effect above fires, and unmounts/remounts with WorkspaceShell while the
  // target workspace is loading). Reset anything workspace-scoped held in
  // that chrome so a draft, a live thread, or an in-flight sync status from
  // one workspace can't leak into another's (see #2962). Skip the very first
  // render of the session (no prior workspace to leak from) — see
  // `lastSeenWsId` for why the marker is module-scoped.
  useEffect(() => {
    if (!wsId) return;
    if (lastSeenWsId && lastSeenWsId !== wsId) {
      useAskDock.getState().newChat();
      useBuilderDialog.getState().setIsOpen(false);
      useBuilderDialog.getState().setModelingSelection(null);
      useDatabaseOperationStore.getState().clearSyncState();
    }
    lastSeenWsId = wsId;
  }, [wsId]);

  // Remember the last-opened workspace per-org so the post-login dispatcher
  // can skip the picker next time. Skipped in local mode (no real orgs).
  React.useEffect(() => {
    if (isLocalMode) return;
    if (!isPending && !isError && data && orgId && wsId) {
      setLastWorkspaceId(orgId, wsId);
    }
  }, [isPending, isError, data, orgId, wsId, isLocalMode]);

  // In local mode there's nowhere to redirect to — surface the error via toast
  // and let the caller see the empty layout. The cloud fallbacks below don't apply.
  React.useEffect(() => {
    if (!isPending && data?.workspace_error) {
      // Stable id so a repeat REPLACES rather than stacks. The loop that made
      // this spam is fixed above, but a workspace-load error is never worth
      // more than one toast on screen.
      toast.error(data.workspace_error, { id: "workspace-load-error" });
      if (isLocalMode) return;
      if (orgSlug) {
        navigate(ROUTES.ORG(orgSlug).ROOT, { replace: true });
      } else {
        navigate(ROUTES.ROOT, { replace: true });
      }
    }
  }, [isPending, data?.workspace_error, orgSlug, navigate, isLocalMode]);

  useEffect(() => {
    // A workspace that never finished starting is a real, terminal condition,
    // but not the generic load failure: redirecting to the org root would
    // remount the shell, refetch, and land right back here. It gets its own
    // surface below instead.
    if (isError && !materializingTimedOut) {
      const msg =
        (error as { response?: { data?: { error?: string } } })?.response?.data?.error ??
        "Failed to load workspace.";
      toast.error(msg, { id: "workspace-load-error" });
      if (isLocalMode) return;
      if (orgSlug) {
        navigate(ROUTES.ORG(orgSlug).ROOT, { replace: true });
      } else {
        navigate(ROUTES.ROOT, { replace: true });
      }
    }
  }, [isError, materializingTimedOut, error, navigate, orgSlug, isLocalMode]);

  // Still coming up (pod restart / rolling update) — the query is retrying, so
  // this resolves on its own. `isMaterializing` implies `isPending`; naming it
  // lets us say what the wait is for instead of showing a bare spinner.
  if (isPending) {
    return (
      <div className='flex h-full w-full flex-col items-center justify-center gap-3'>
        <Spinner />
        {isMaterializing && (
          <p className='text-muted-foreground text-sm'>Workspace is starting up…</p>
        )}
      </div>
    );
  }

  // Retries exhausted. Say so plainly and offer a retry — spinning forever
  // tells the user nothing and hides a real outage.
  //
  // Copy rule: describe what the USER sees, never why. "Pod", "volume",
  // "deploy", "replica" are our vocabulary, not theirs — they name causes a
  // user can neither verify nor act on. Matches `IdeUnavailablePanel`, which
  // says "Oxygen Factory is restarting. It will resume shortly."
  if (materializingTimedOut) {
    return (
      <div className='flex h-full w-full flex-col items-center justify-center gap-3 p-6 text-center'>
        <p className='font-medium text-sm'>This workspace isn't ready yet</p>
        <p className='max-w-md text-muted-foreground text-sm'>
          It's taking longer than usual to open. It should be available shortly.
        </p>
        <Button variant='outline' size='sm' onClick={() => refetch()}>
          Try again
        </Button>
      </div>
    );
  }

  // When a local-mode server has no config.yml, render a blocking setup
  // dialog instead of the main shell. Short-circuits before the sidebar /
  // IDE / routes mount, so WorkspaceManager-dependent endpoints are never
  // called (they would 503). `WorkspaceStatus` is not mounted in this path
  // either — it would surface config errors as a banner, which is the
  // wrong UX for the first-run case.
  if (isLocalMode && data?.requires_local_setup) {
    return <LocalWorkspaceSetupDialog />;
  }

  if (isError || !workspace) {
    return null;
  }

  return (
    <HotkeysProvider>
      <BuilderDialog />
      <FileQuickOpen />
      <SettingsDialog />
      <ManageWorkspacesDialog />

      <WorkspaceShell>
        <Routes>
          <Route index element={<LauncherPage />} />

          <Route path='home' element={<LauncherPage />} />
          {/* The rail "Chat" button lands here — a composer + recent threads
              (replacing the old bulk threads-management list). */}
          <Route path='threads' element={<ChatPage />} />
          <Route path='threads/:threadId' element={<ThreadPage />} />
          {/* Canonical "Automations" routes (formerly Automations / Automations). */}
          <Route path='automations' element={<AutomationsListPage />} />
          <Route path='automations/:pathb64' element={<AutomationPage />} />
          {/* Back-compat: legacy /automations URLs keep rendering for old
              bookmarks and deep links. */}
          <Route path='workflows' element={<AutomationsListPage />} />
          <Route path='workflows/:pathb64' element={<AutomationPage />} />
          <Route path='pipelines/:pathb64' element={<AirwayPage />} />
          <Route path='pipelines/:pathb64/runs/:runId' element={<AirwayPage />} />
          {/* NOTE: /apps now renders the Dashboards page (published .app.yml
            Data Apps). Customer-apps discovery is handled by the launcher.
            Individual Data Apps at /apps/:pathb64 are unaffected. */}
          <Route path='apps' element={<DashboardsPage />} />
          <Route path='apps/:pathb64' element={<AppPage />} />
          <Route
            path='ide'
            element={
              <Suspense fallback={<RouteFallback />}>
                <IdePage />
              </Suspense>
            }
          >
            {/* Files routes */}
            <Route path='files' element={<FilesLayout />}>
              <Route path=':pathb64' element={<EditorPage />} />
              <Route
                path='looker/:integrationName/:model/:exploreName'
                element={<LookerExplorerPage />}
              />
            </Route>

            {/* Database routes */}
            <Route path='database' element={<DatabaseLayout />}>
              <Route index element={<QueryWorkspacePage />} />
            </Route>

            {/* Data Modeling routes */}
            <Route path='modeling' element={<ModelingPage />} />

            {/* Semantic model — explorer + metric tree */}
            <Route path='semantic' element={<SemanticLayerPage />} />

            {/* World Model — business-model graph; first icon in the IDE sidebar */}
            <Route path='world-model' element={<WorldModelView />} />

            {/* Edge routes — fleet topology, list management, timeline
              playback (subsumes the old Compliance pages), audit log,
              and domain pack — all behind one IDE section so the
              operator stops hunting between Settings and the IDE. */}
            <Route path='edge' element={<EdgeLayout />}>
              <Route index element={<EdgeDashboardPage />} />
              <Route path='playback' element={<EdgePlaybackPage />} />
              <Route path='detections' element={<EdgeDetectionsPage />} />
              <Route path='topology' element={<EdgeTopologyPage />} />
              <Route path='devices' element={<EdgeDevicesPage />} />
              <Route path='boxes/:boxId' element={<EdgeBoxDetailPage />} />
              {/* Legacy /ide/edge/list redirected to the old FleetPage's
                list-toggle URL; FleetPage is gone now, so route to the
                new Devices tab which is its functional successor. */}
              <Route path='list' element={<Navigate to='../devices' replace />} />
              <Route path='timeline' element={<Navigate to='../playback' replace />} />
              <Route path='rollouts' element={<EdgeRolloutsPage />} />
              <Route path='rollouts/:planId' element={<EdgeRolloutDetailPage />} />
              <Route path='audit' element={<EdgeAuditPage />} />
              <Route path='pack' element={<EdgePackPage />} />
            </Route>
            {/* Legacy /ide/compliance — kept as a back-compat redirect so
              bookmarks and the old "View clip" links still land somewhere
              useful. Drops the cameraId/reportId segments since the
              timeline page resolves to the most recent event. */}
            <Route path='compliance/*' element={<Navigate to='../edge/timeline' replace />} />

            {/* Tests routes */}
            <Route path='tests' element={<TestsLayout />}>
              <Route index element={<TestsDashboardPage />} />
              <Route path='runs' element={<TestsRunsPage />} />
              <Route path=':pathb64' element={<TestFileDetailPage />} />
            </Route>

            {/* Coordinator routes */}
            <Route path='coordinator' element={<CoordinatorLayout />}>
              <Route path='overview' element={<OverviewPage />} />
              <Route path='jobs' element={<JobsPage />} />
              <Route path='jobs/:scheduleId' element={<JobDetailPage />} />
              <Route path='runs' element={<RunsPage />} />
              <Route path='runs/:runId' element={<RunDetailPage />} />
              <Route path='recovery' element={<RecoveryPage />} />
              <Route path='queue' element={<QueueHealthPage />} />
              <Route index element={<Navigate to='overview' replace />} />
            </Route>

            {/* Observability routes (enterprise only) */}
            {authConfig.enterprise && (
              <Route path='observability' element={<ObservabilityLayout />}>
                <Route path='traces' element={<TracesPage />} />
                <Route path='traces/:traceId' element={<TraceDetailPage />} />
                <Route path='clusters' element={<ClusterMapPage />} />
                <Route path='metrics' element={<MetricsPage />} />
                <Route path='metrics/:metricName' element={<MetricDetailPage />} />
                <Route path='execution-analytics' element={<ExecutionAnalytics />} />
              </Route>
            )}

            {/* Oxygen Factory lands on the World Model — the business-model
              overview — rather than the raw file tree. */}
            <Route index element={<Navigate to='world-model' replace />} />
          </Route>
          <Route path='onboarding' element={<AgenticSetupPage />} />
          <Route path='context-graph' element={<ContextGraphPage />} />

          <Route path='*' element={<Navigate to='.' />} />
        </Routes>
      </WorkspaceShell>
    </HotkeysProvider>
  );
});

/** Local-mode router: a flat shape with the implicit workspace mounted at `/`.
 *  Mirrors the backend's local-mode route set (no org, no login, no workspace
 *  management). Any path the user visits that isn't a workspace sub-route
 *  falls through to the `*` handler inside `WorkspaceLayout` and lands on `/`. */
const getLocalRouter = () =>
  createBrowserRouter(
    createRoutesFromElements(
      <Route errorElement={<RouteErrorBoundary />}>
        {/* QuickBooks OAuth success landing — must resolve before the `/*`
            catch-all so the popup/redirect return renders this page. */}
        <Route path='/quickbooks/connected' element={<QuickBooksConnected />} />
        <Route
          path='/*'
          element={
            <ProtectedRoute>
              <SidebarProvider>
                <WorkspaceLayout />
              </SidebarProvider>
            </ProtectedRoute>
          }
        />
      </Route>
    )
  );

const getCloudRouter = (authConfig: AuthConfigResponse) =>
  createBrowserRouter(
    createRoutesFromElements(
      <Route errorElement={<RouteErrorBoundary />}>
        {/* Auth routes when auth is enabled */}
        {authConfig.auth_enabled && (
          <>
            <Route path={ROUTES.AUTH.LOGIN} element={<LoginPage />} />
            <Route path={ROUTES.AUTH.GOOGLE_CALLBACK} element={<GoogleCallback />} />
            <Route path={ROUTES.AUTH.OKTA_CALLBACK} element={<OktaCallback />} />
            <Route path={ROUTES.AUTH.MAGIC_LINK_CALLBACK} element={<MagicLinkCallback />} />
          </>
        )}

        {/* Dev sign-in bypass — public, and mounted unconditionally. Sits
            outside the `auth_enabled` block because it IS the sign-in for a
            browser automation tool: one `goto('/dev-login')` and the browser
            holds a real session.

            Not gated on `authConfig.dev_login`, which is per-caller and false
            off-box: gating it there meant the developer browsing their own
            server as `192.168.1.x` fell through to `/*` → `ProtectedRoute` →
            `/login` and got a silent bounce, while the page's own copy — which
            exists to say "browse localhost, or set OXY_DEV_LOGIN_EMAILS" —
            could never render. The gate is the server's 404; this page's job is
            to explain it. Hiding the route buys no probe-resistance either: the
            bundle ships the page and its strings to every caller regardless. */}
        <Route path={ROUTES.AUTH.DEV_LOGIN} element={<DevLogin />} />

        {/* GitHub callback must always be accessible (used during the workspace import popup flow) */}
        <Route path='/github/callback' element={<GitHubCallback />} />

        {/* QuickBooks OAuth success landing — public; posts the realm id back
            to the opener (popup) or bounces to the return path (mobile). */}
        <Route path='/quickbooks/connected' element={<QuickBooksConnected />} />

        {/* Invitation accept — public; the page itself redirects to /login if needed */}
        <Route path='/invite/:token' element={<InvitePage />} />

        {/* `oxy login` browser handoff — public; reads the session token and
            hands it to the CLI's loopback listener, bouncing through /login
            first when not yet signed in. */}
        <Route path='/cli-auth' element={<CliAuth />} />

        {/* Auth-gated routes. `ActingShell` wraps EVERY authenticated page so the
            assume-role banner — and its one-click exit — follows an operator
            wherever they land, including surfaces (the partner console) that use
            neither WorkspaceShell nor AdminLayout. It renders children untouched
            when no session is live. */}
        <Route
          path='/*'
          element={
            <ProtectedRoute>
              <ActingShell>
                <Outlet />
              </ActingShell>
            </ProtectedRoute>
          }
        >
          {/* The custom-app detail dossier, popped out into its own window
              (the `window` dock mode). Sits OUTSIDE `AdminLayout` on purpose:
              it only ever renders in a ~560px popup, where the admin nav
              chrome would be dead weight. The APIs it reads are app-admin
              gated server-side. */}
          <Route
            path='admin/apps/:orgSlug/:appSlug/panel'
            element={
              <Suspense fallback={<RouteFallback />}>
                <AppDossierWindow />
              </Suspense>
            }
          />

          {/* Admin queue (OXY_OWNER-gated server-side) — sits outside
              `OwnerRedirect` so owners can actually reach it. */}
          <Route
            element={
              <Suspense fallback={<RouteFallback />}>
                <AdminLayout />
              </Suspense>
            }
          >
            {/* Bare `/admin` lands somewhere — Custom apps is the most-
                used admin surface, and the AdminLayout's owner/admin
                redirect already runs above this so app-admins reach it
                cleanly, owners reach it cleanly, and unauthorized users
                get bounced before the Navigate even runs. */}
            <Route path='admin' element={<Navigate to={ROUTES.ADMIN.CUSTOMER_APPS} replace />} />
            <Route path='admin/billing/queue' element={<AdminBillingQueue />} />
            <Route path='admin/feature-flags' element={<AdminFeatureFlags />} />
            <Route path='admin/internal-jobs' element={<AdminInternalJobs />} />
            <Route path='admin/compiles' element={<AdminCompiles />} />
            {/* ROUTES.ADMIN.INTERNAL_JOBS */}
            <Route path='admin/explorer' element={<AdminExplorer />} />
            <Route path='admin/audit' element={<AdminAudit />} />
            <Route path='admin/airhouse' element={<AdminAirhouse />} />
            {/* ROUTES.ADMIN.EXPLORER */}
            <Route path='admin/app-admins' element={<AdminAppAdmins />} />
            {/* ROUTES.ADMIN.PUBLISH_TOKENS — open to any Global Admin */}
            <Route path='admin/publish-tokens' element={<AdminPublishTokens />} />
            {/* Customer-apps admin is mounted at /admin/apps (canonical
                ROUTES.ADMIN.CUSTOMER_APPS in libs/utils/routes.ts) with an
                optional master-detail tail. AdminCustomApps reads
                :orgSlug + :appSlug from useParams to pre-select the detail
                pane; the bare /admin/apps lands on the list-only state. */}
            <Route path='admin/apps' element={<AdminCustomApps />} />
            <Route path='admin/apps/:orgSlug/:appSlug' element={<AdminCustomApps />} />
            {/* Tenant-management surfaces — list + master/detail via :id tail. */}
            <Route path='admin/tenants' element={<AdminTenantsCockpit />} />
            <Route path='admin/tenants/overview' element={<AdminTenants />} />
            <Route path='admin/oltp' element={<AdminOltp />} />
            <Route path='admin/orgs' element={<AdminOrgs />} />
            <Route path='admin/orgs/:orgId' element={<AdminOrgDetail />} />
            <Route path='admin/users' element={<AdminUsers />} />
            <Route path='admin/users/:userId' element={<AdminUserDetail />} />
            <Route path='admin/workspaces' element={<AdminWorkspaces />} />
            <Route path='admin/workspaces/:workspaceId' element={<AdminWorkspaceDetail />} />
            <Route path='admin/workspace-health' element={<AdminWorkspaceHealth />} />
            {/* Airway admission policy — Global Owner only (see AdminSidebar's
                ownerOnly gate and AdminLayout's APP_ADMIN_ROUTE_PREFIXES, which
                deliberately omits this route). */}
            <Route path='admin/airway' element={<AdminAirway />} />
          </Route>

          {/* Partner console — anyone holding a partner role (the server enforces
              scope on every call). Same shell as AdminLayout, deliberately: both
              are operations surfaces for administering organizations you don't
              personally own. Not Oxy-staff-gated and not owner-redirected. */}
          <Route path='partners' element={<PartnerLayout />}>
            <Route index element={<PartnerClients />} />
            <Route path='apps' element={<PartnerCustomApps />} />
            <Route path='team' element={<PartnerTeam />} />
            <Route path='activity' element={<PartnerActivity />} />
          </Route>

          {/* User-facing routes — owners get bounced to the admin queue. */}
          <Route element={<OwnerRedirect />}>
            {/* Top-level: smart dispatcher picks no-org page / org root / last or first workspace */}
            <Route index element={<PostLoginDispatcher />} />
            {/* No org yet: pending invites, join by link, log out */}
            <Route path='onboarding' element={<OnboardingPage />} />

            {/* Org-scoped routes */}
            <Route path=':orgSlug' element={<OrgGuard />}>
              {/* Stripe Checkout return URLs. The path includes `/billing/`,
                  which is the `OrgGuard` paywall bypass — these pages
                  render even while billing.status is `incomplete`. */}
              <Route
                path='billing/checkout-success'
                element={
                  <Suspense fallback={<RouteFallback />}>
                    <CheckoutSuccessPage />
                  </Suspense>
                }
              />
              <Route
                path='billing/checkout-cancelled'
                element={
                  <Suspense fallback={<RouteFallback />}>
                    <CheckoutCancelledPage />
                  </Suspense>
                }
              />

              {/* Org root picks a workspace and redirects into it, or shows "being set up" */}
              <Route index element={<OrgDispatcher />} />
              {/* The org onboarding wizard is gone; old links land on the org root. */}
              <Route path='onboarding' element={<Navigate to='..' replace />} />

              {/* Workspace-scoped routes */}
              <Route
                path='workspaces/:wsId/*'
                element={
                  <SidebarProvider>
                    <WorkspaceLayout />
                  </SidebarProvider>
                }
              />
            </Route>
          </Route>
        </Route>
      </Route>
    )
  );

const getRouter = (authConfig: AuthConfigResponse) =>
  authConfig.mode === "local" ? getLocalRouter() : getCloudRouter(authConfig);

function App() {
  const { data: authConfig, isPending } = useAuthConfig();
  useVersionCheck();

  // Only recreate the router when routing-relevant fields change — prevents the
  // router from being torn down on every authConfig refetch (e.g. when a GitHub
  // popup closes and the window regains focus), which would reset page state.
  const routerRef = useRef<ReturnType<typeof getRouter> | null>(null);
  const prevRouterKey = useRef<string | null>(null);
  const routerKey = authConfig ? `${authConfig.auth_enabled}:${authConfig.mode}` : null;
  if (authConfig && routerKey !== prevRouterKey.current) {
    routerRef.current = getRouter(authConfig);
    prevRouterKey.current = routerKey;
  }
  const router = routerRef.current;

  if (isPending || !authConfig || !router) {
    return (
      <div className='flex h-full w-full items-center justify-center'>
        <Spinner className='size-6' />
      </div>
    );
  }

  return (
    <ErrorBoundary fallback={<div>Something went wrong. Please refresh.</div>} showDialog>
      <AuthProvider authConfig={authConfig}>
        <OrgSubdomainAuthGate>
          <RouterProvider router={router} />
        </OrgSubdomainAuthGate>
        <ShadcnToaster />
      </AuthProvider>
    </ErrorBoundary>
  );
}

export default App;
