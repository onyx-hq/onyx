import type { PaginationState } from "@tanstack/react-table";

const agentKeys = {
  all: ["agent"] as const,
  list: (projectId: string, branchName: string) =>
    [...agentKeys.all, "list", projectId, branchName] as const,
  get: (pathb64: string, projectId: string, branchName: string) =>
    [...agentKeys.all, "get", pathb64, projectId, branchName] as const
};

const analyticsKeys = {
  all: ["analytics"] as const,
  runByThread: (projectId: string, threadId: string) =>
    [...analyticsKeys.all, "runByThread", projectId, threadId] as const,
  runsByThread: (projectId: string, threadId: string) =>
    [...analyticsKeys.all, "runsByThread", projectId, threadId] as const
};

const adminAssumeKeys = {
  all: ["adminAssume"] as const,
  current: () => [...adminAssumeKeys.all, "current"] as const,
  history: () => [...adminAssumeKeys.all, "history"] as const
};

const adminPartnerKeys = {
  all: ["adminPartner"] as const,
  list: () => [...adminPartnerKeys.all, "list"] as const,
  detail: (id: string) => [...adminPartnerKeys.all, "detail", id] as const
};

const auditKeys = {
  all: ["audit"] as const,
  search: (params: Record<string, unknown>) => [...auditKeys.all, "search", params] as const
};

const partnerKeys = {
  all: ["partner"] as const,
  mine: () => [...partnerKeys.all, "mine"] as const,
  orgs: (partnerId: string) => [...partnerKeys.all, "orgs", partnerId] as const,
  members: (partnerId: string, orgId: string) =>
    [...partnerKeys.all, "members", partnerId, orgId] as const,
  audit: (partnerId: string) => [...partnerKeys.all, "audit", partnerId] as const,
  apps: (partnerId: string, orgId: string) =>
    [...partnerKeys.all, "apps", partnerId, orgId] as const,
  workspaces: (partnerId: string, orgId: string) =>
    [...partnerKeys.all, "workspaces", partnerId, orgId] as const,
  health: (partnerId: string) => [...partnerKeys.all, "health", partnerId] as const,
  appTokens: (partnerId: string, appId: string) =>
    [...partnerKeys.all, "app-tokens", partnerId, appId] as const,
  people: (partnerId: string) => [...partnerKeys.all, "people", partnerId] as const
};

const threadKeys = {
  all: ["thread"] as const,
  list: (projectId: string, page?: number, limit?: number, search?: string) =>
    [...threadKeys.all, "list", projectId, { page, limit, search }] as const,
  item: (projectId: string, threadId: string) =>
    [...threadKeys.all, projectId, { threadId }] as const,
  messages: (projectId: string, threadId: string) =>
    [...threadKeys.all, "messages", projectId, threadId] as const
};

const traceKeys = {
  all: ["trace"] as const,
  list: (
    projectId: string,
    limit?: number,
    offset?: number,
    status?: string,
    duration?: string,
    search?: string,
    from?: number,
    to?: number
  ) =>
    [
      ...traceKeys.all,
      "list",
      projectId,
      { limit, offset, status, duration, search, from, to }
    ] as const,
  item: (projectId: string, traceId: string) => [...traceKeys.all, projectId, { traceId }] as const,
  waterfall: (projectId: string, traceId: string) =>
    [...traceKeys.all, "waterfall", projectId, traceId] as const
};

const executionAnalyticsKeys = {
  all: ["executionAnalytics"] as const,
  percentiles: (projectId: string, days?: number) =>
    [...executionAnalyticsKeys.all, "percentiles", projectId, { days }] as const,
  histogram: (projectId: string, days?: number) =>
    [...executionAnalyticsKeys.all, "histogram", projectId, { days }] as const,
  cost: (projectId: string, days?: number) =>
    [...executionAnalyticsKeys.all, "cost", projectId, { days }] as const
};

const agenticAutomationKeys = {
  all: ["agentic-automations"] as const,
  files: (projectId: string) => [...agenticAutomationKeys.all, "files", projectId] as const,
  file: (projectId: string, pathB64: string) =>
    [...agenticAutomationKeys.all, "file", projectId, pathB64] as const,
  run: (projectId: string, runId: string) =>
    [...agenticAutomationKeys.all, "run", projectId, runId] as const,
  runsForAutomation: (projectId: string, automationRef: string) =>
    [...agenticAutomationKeys.all, "runs-for-automation", projectId, automationRef] as const
};

const airwayKeys = {
  all: ["agentic-airway"] as const,
  run: (projectId: string, runId: string) => [...airwayKeys.all, "run", projectId, runId] as const,
  runsForPipeline: (projectId: string, pipelineRef: string) =>
    [...airwayKeys.all, "runs-for-pipeline", projectId, pipelineRef] as const,
  /** A pipeline's list of backfill ranges (the gantt). */
  ranges: (projectId: string, pipelineRef: string) =>
    [...airwayKeys.all, "ranges", projectId, pipelineRef] as const,
  /** Per-range chunk coverage (the drill-in grid). */
  coverage: (projectId: string, rangeId: string) =>
    [...airwayKeys.all, "coverage", projectId, rangeId] as const,
  files: (projectId: string) => [...airwayKeys.all, "files", projectId] as const
};

const scheduleKeys = {
  all: ["agentic-schedules"] as const,
  list: (projectId: string) => [...scheduleKeys.all, "list", projectId] as const,
  item: (projectId: string, id: string) => [...scheduleKeys.all, projectId, { id }] as const
};

const automationKeys = {
  all: ["automation"] as const,
  run: (projectId: string, branchName: string) =>
    [...automationKeys.all, "run", projectId, branchName] as const,
  list: (projectId: string, branchName: string) =>
    [...automationKeys.all, "list", projectId, branchName] as const,
  get: (projectId: string, branchName: string, relative_path: string) =>
    [...automationKeys.all, "get", projectId, branchName, relative_path] as const,
  getLogs: (projectId: string, branchName: string, relative_path: string) =>
    [...automationKeys.all, "getLogs", projectId, branchName, relative_path] as const,
  getRuns: (
    projectId: string,
    branchName: string,
    relative_path: string,
    pagination: PaginationState
  ) =>
    [...automationKeys.all, "getRuns", projectId, branchName, relative_path, pagination] as const,
  getBlocks: (projectId: string, branchName: string, sourceId: string, runIndex?: number) =>
    [...automationKeys.all, "getBlocks", projectId, branchName, sourceId, runIndex] as const
};

const chartKeys = {
  all: ["chart"] as const,
  get: (projectId: string, branchName: string, file_path: string) =>
    [...chartKeys.all, "get", projectId, branchName, file_path] as const,
  // For AppPreview displays compiled in DuckDB-WASM. `dataKey` should uniquely
  // identify the source dataset (e.g. taskName + json file_path); avoid passing
  // the full data object — that thrashes the cache across renders.
  fromDisplay: (
    projectId: string,
    branchName: string,
    displayKey: string,
    dataKey: string,
    isDarkMode: boolean
  ) =>
    [
      ...chartKeys.all,
      "fromDisplay",
      projectId,
      branchName,
      displayKey,
      dataKey,
      isDarkMode
    ] as const
};

const fileKeys = {
  all: (projectId: string, branchName: string) => ["all", projectId, branchName],
  get: (projectId: string, branchName: string, pathb64: string) =>
    [...fileKeys.all(projectId, branchName), "get", pathb64] as const,
  getGit: (projectId: string, branchName: string, pathb64: string, commit: string) =>
    [...fileKeys.all(projectId, branchName), "getGit", pathb64, commit] as const,
  tree: (projectId: string, branchName: string) =>
    [...fileKeys.all(projectId, branchName), "tree"] as const,
  diffSummary: (projectId: string, branchName: string) =>
    [...fileKeys.all(projectId, branchName), "diffSummary"] as const
};

const databaseKeys = {
  all: ["database"] as const,
  list: (projectId: string, branchName: string) =>
    [...databaseKeys.all, "list", projectId, branchName] as const,
  schema: (projectId: string, branchName: string, dbName: string) =>
    [...databaseKeys.all, "schema", projectId, branchName, dbName] as const
};

const appKeys = {
  all: ["app"] as const,
  list: (projectId: string, branchName: string, publishedOnly = false) =>
    [...appKeys.all, "list", projectId, branchName, publishedOnly] as const,
  getAppData: (projectId: string, branchName: string, appPath: string) =>
    [...appKeys.all, "getAppData", projectId, branchName, appPath] as const,
  getData: (projectId: string, branchName: string, appPath: string) =>
    [...appKeys.all, "getData", projectId, branchName, appPath] as const,
  getDisplays: (projectId: string, branchName: string, appPath: string) =>
    [...appKeys.all, "getDisplays", projectId, branchName, appPath] as const,
  getAppDataCached: (projectId: string, branchName: string, appPath: string) =>
    [...appKeys.all, "getAppDataCached", projectId, branchName, appPath] as const
};

const onboardingKeys = {
  all: ["onboarding"] as const,
  readiness: (projectId: string) => [...onboardingKeys.all, "readiness", projectId] as const,
  githubSetup: (projectId: string) => [...onboardingKeys.all, "githubSetup", projectId] as const
};

const apiKeyKeys = {
  all: ["apiKey"] as const,
  list: (projectId: string) => [...apiKeyKeys.all, "list", projectId] as const,
  item: (projectId: string, id: string) => [...apiKeyKeys.all, projectId, { id }] as const
};

const secretKeys = {
  all: ["secret"] as const,
  list: (projectId: string) => [...secretKeys.all, "list", projectId] as const,
  item: (projectId: string, id: string) => [...secretKeys.all, projectId, { id }] as const,
  envList: (projectId: string) => [...secretKeys.all, "env", projectId] as const
};

const spApiKeys = {
  all: ["spApi"] as const,
  marketplaces: (projectId: string) => [...spApiKeys.all, "marketplaces", projectId] as const
};

const logsKeys = {
  all: ["logs"] as const,
  list: (projectId: string) => [...logsKeys.all, "list", projectId] as const
};

const settingsKeys = {
  all: ["settings"] as const,
  projectStatus: (project_id: string) =>
    [...settingsKeys.all, "project-status", { project_id }] as const
};

const repositoryKeys = {
  all: ["repositories"] as const,
  list: (projectId: string) => [...repositoryKeys.all, "list", projectId] as const,
  branch: (projectId: string, name: string) =>
    [...repositoryKeys.all, "branch", projectId, name] as const,
  diff: (projectId: string, name: string) =>
    [...repositoryKeys.all, "diff", projectId, name] as const,
  branches: (projectId: string, name: string) =>
    [...repositoryKeys.all, "branches", projectId, name] as const
};

const builderKeys = {
  all: ["builder"] as const,
  availability: (projectId: string) => [...builderKeys.all, "availability", projectId] as const
};

const configKeys = {
  all: ["config"] as const,
  validation: () => [...configKeys.all, "validation"] as const,
  status: () => [...configKeys.all, "status"] as const
};

const userKeys = {
  all: ["user"] as const,
  list: () => [...userKeys.all, "list"] as const,
  current: () => [...userKeys.all, "current"] as const
};

const workspaceKeys = {
  all: ["workspace"] as const,
  list: () => [...workspaceKeys.all, "list"] as const,
  listByOrg: (orgId: string | undefined) => [...workspaceKeys.list(), orgId] as const,
  item: (workspaceId: string) => [...workspaceKeys.all, "item", workspaceId] as const,
  branches: (workspaceId: string) => [...workspaceKeys.all, "branches", workspaceId] as const,

  revisionInfo: (workspaceId: string, branchName: string) =>
    [...workspaceKeys.all, "revisionInfo", workspaceId, branchName] as const,

  status: (workspaceId: string, branchName: string) =>
    [...workspaceKeys.all, "status", workspaceId, branchName] as const,

  members: (workspaceId: string) => [...workspaceKeys.all, "members", workspaceId] as const,

  recentCommits: (workspaceId: string, branchName: string) =>
    [...workspaceKeys.all, "recentCommits", workspaceId, branchName] as const,

  localSetup: (workspaceId: string) => [...workspaceKeys.all, "localSetup", workspaceId] as const
};

const artifactKeys = {
  all: ["artifact"] as const,
  get: (projectId: string, branchName: string, id: string) =>
    [...artifactKeys.all, "get", projectId, branchName, id] as const
};

const contextGraphKeys = {
  all: ["context-graph"] as const,
  graph: (projectId: string, branchName: string) =>
    [...contextGraphKeys.all, "graph", projectId, branchName] as const
};

const integrationKeys = {
  all: ["integration"] as const,
  looker: (projectId: string, branchName: string) =>
    [...integrationKeys.all, "looker", projectId, branchName] as const
};

const slackKeys = {
  all: ["slack"] as const,
  installation: (orgId: string) => [...slackKeys.all, "installation", orgId] as const
};

const testFileKeys = {
  all: ["testFile"] as const,
  list: (projectId: string, branchName: string) =>
    [...testFileKeys.all, "list", projectId, branchName] as const,
  get: (pathb64: string, projectId: string, branchName: string) =>
    [...testFileKeys.all, "get", pathb64, projectId, branchName] as const
};

const testProjectRunKeys = {
  all: ["testProjectRun"] as const,
  list: (projectId: string) => [...testProjectRunKeys.all, "list", projectId] as const
};

const testRunKeys = {
  all: ["testRun"] as const,
  list: (projectId: string, pathb64: string) =>
    [...testRunKeys.all, "list", projectId, pathb64] as const,
  detail: (projectId: string, pathb64: string, runIndex: number) =>
    [...testRunKeys.all, "detail", projectId, pathb64, runIndex] as const
};

const humanVerdictKeys = {
  all: ["humanVerdict"] as const,
  list: (projectId: string, pathb64: string, runIndex: number) =>
    [...humanVerdictKeys.all, "list", projectId, pathb64, runIndex] as const
};

const modelingKeys = {
  all: ["modeling"] as const,
  projects: (projectId: string, branchName: string) =>
    [...modelingKeys.all, "projects", projectId, branchName] as const,
  project: (projectId: string, modelingProjectName: string, branchName: string) =>
    [...modelingKeys.all, "project", projectId, modelingProjectName, branchName] as const,
  nodes: (projectId: string, modelingProjectName: string, branchName: string) =>
    [...modelingKeys.all, "nodes", projectId, modelingProjectName, branchName] as const,
  lineage: (projectId: string, modelingProjectName: string, branchName: string) =>
    [...modelingKeys.all, "lineage", projectId, modelingProjectName, branchName] as const,
  columnLineage: (projectId: string, modelingProjectName: string, branchName: string) =>
    [...modelingKeys.all, "columnLineage", projectId, modelingProjectName, branchName] as const
};

const orgKeys = {
  all: ["org"] as const,
  list: () => [...orgKeys.all, "list"] as const,
  item: (orgId: string) => [...orgKeys.all, "item", orgId] as const,
  members: (orgId: string) => [...orgKeys.all, "members", orgId] as const,
  invitations: (orgId: string) => [...orgKeys.all, "invitations", orgId] as const,
  myInvitations: () => [...orgKeys.all, "my-invitations"] as const,
  teams: (orgId: string) => [...orgKeys.all, "teams", orgId] as const,
  team: (orgId: string, teamId: string) => [...orgKeys.all, "teams", orgId, teamId] as const,
  // Crew (frontline) admin. Under `org`, not `frontline`: those keys are the
  // kiosk's own unauthenticated reads, and a worker list must never share a
  // cache lineage with what a shared tablet fetches.
  frontlineWorkers: (orgId: string) => [...orgKeys.all, "frontline-workers", orgId] as const,
  frontlineDevices: (orgId: string) => [...orgKeys.all, "frontline-devices", orgId] as const,
  // The operating graph: places, the position vocabulary, and who holds what
  // where. `assignments` without a filter is the prefix every filtered read
  // shares, so one invalidation reaches the per-location and per-person views.
  locations: (orgId: string) => [...orgKeys.all, "locations", orgId] as const,
  roles: (orgId: string) => [...orgKeys.all, "roles", orgId] as const,
  assignments: (orgId: string, filter?: { user_id?: string; location_id?: string }) =>
    filter
      ? ([...orgKeys.all, "assignments", orgId, filter] as const)
      : ([...orgKeys.all, "assignments", orgId] as const)
};

/**
 * App access is edited from three surfaces with three different gates, so the key
 * carries the SCOPE it was fetched through. Without that, an admin-console read and
 * an org-settings read of the same app would share a cache entry and one surface's
 * 403 would poison the other.
 */
const appAccessKeys = {
  all: ["app-access"] as const,
  detail: (scope: string, appId: string) => [...appAccessKeys.all, scope, appId] as const,
  grantableTeams: (scope: string, key: string) =>
    [...appAccessKeys.all, "teams", scope, key] as const,
  grantablePeople: (scope: string, key: string) =>
    [...appAccessKeys.all, "people", scope, key] as const,
  /** Prefix — invalidate to refresh every org's app list at once. */
  orgAppsAll: () => [...appAccessKeys.all, "org-apps"] as const,
  orgApps: (orgId: string) => [...appAccessKeys.orgAppsAll(), orgId] as const
};

const githubKeys = {
  all: ["github"] as const,
  namespaces: (orgId: string) => ["github", "namespaces", orgId] as const,
  installAppUrl: (orgId: string) => ["github", "install-app-url", orgId] as const,
  appInstallations: (orgId: string) => ["github", "app-installations", orgId] as const,
  account: ["github", "account"] as const,
  userInstallations: ["github", "user-installations"] as const
};

const coordinatorKeys = {
  all: ["coordinator"] as const,
  activeRuns: (projectId: string, includeSystem: boolean) =>
    [...coordinatorKeys.all, "activeRuns", projectId, { includeSystem }] as const,
  runHistory: (
    projectId: string,
    params: {
      limit: number;
      offset: number;
      status?: string;
      source_type?: string;
      schedule_id?: string;
      include_system?: boolean;
    }
  ) => [...coordinatorKeys.all, "runHistory", projectId, params] as const,
  runTree: (projectId: string, runId: string) =>
    [...coordinatorKeys.all, "runTree", projectId, runId] as const,
  recovery: (projectId: string) => [...coordinatorKeys.all, "recovery", projectId] as const,
  queue: (projectId: string) => [...coordinatorKeys.all, "queue", projectId] as const
};

const airhouseKeys = {
  all: ["airhouse"] as const,
  connection: (workspaceId: string) => [...airhouseKeys.all, "connection", workspaceId] as const,
  version: () => [...airhouseKeys.all, "version"] as const,
  catalogIndexes: (workspaceId: string) =>
    [...airhouseKeys.all, "catalog-indexes", workspaceId] as const,
  // The admin console's fleet read. Separate root from the member-facing keys
  // above because it is a different resource — every workspace, not mine.
  adminFleet: () => ["admin", "airhouse", "fleet"] as const
};

const oltpKeys = {
  all: ["oltp"] as const,
  connection: (workspaceId: string) => [...oltpKeys.all, "connection", workspaceId] as const,
  erd: (workspaceId: string) => [...oltpKeys.all, "erd", workspaceId] as const,
  // The admin console's own reads. Separate root from the member-facing keys
  // above because they are a different resource — org-keyed, not
  // workspace-keyed — and an `invalidate(["oltp"])` should not sweep both.
  admin: (orgId: string) => ["admin", "oltp", orgId] as const,
  adminTenants: () => ["admin", "oltp", "tenants"] as const
};

const cameraKeys = {
  all: ["cameras"] as const,
  sites: (workspaceId: string) => [...cameraKeys.all, "sites", workspaceId] as const,
  site: (workspaceId: string, siteId: string) =>
    [...cameraKeys.all, "site", workspaceId, siteId] as const,
  edgeBoxes: (workspaceId: string) => [...cameraKeys.all, "edgeBoxes", workspaceId] as const,
  fleetMembers: (workspaceId: string) => [...cameraKeys.all, "fleetMembers", workspaceId] as const,
  rolloutPlans: (workspaceId: string) => [...cameraKeys.all, "rolloutPlans", workspaceId] as const,
  rolloutConvergence: (workspaceId: string, planId: string) =>
    [...cameraKeys.all, "rolloutConvergence", workspaceId, planId] as const,
  edgeBox: (workspaceId: string, boxId: string) =>
    [...cameraKeys.all, "edgeBox", workspaceId, boxId] as const,
  cameras: (workspaceId: string) => [...cameraKeys.all, "cameras", workspaceId] as const,
  camera: (workspaceId: string, cameraId: string) =>
    [...cameraKeys.all, "camera", workspaceId, cameraId] as const,
  complianceReports: (
    workspaceId: string,
    cameraId: string,
    since: string | undefined,
    limit: number | undefined
  ) =>
    [
      ...cameraKeys.all,
      "complianceReports",
      workspaceId,
      cameraId,
      since ?? null,
      limit ?? null
    ] as const,
  complianceSummary: (workspaceId: string, siteId: string, since: string | undefined) =>
    [...cameraKeys.all, "complianceSummary", workspaceId, siteId, since ?? null] as const,
  activePack: (workspaceId: string) => [...cameraKeys.all, "activePack", workspaceId] as const,
  packs: (workspaceId: string) => [...cameraKeys.all, "packs", workspaceId] as const,
  starterPacks: (workspaceId: string) => [...cameraKeys.all, "starterPacks", workspaceId] as const,
  starterUpdates: (workspaceId: string) =>
    [...cameraKeys.all, "starterUpdates", workspaceId] as const,
  edgeBoxCost: (workspaceId: string, boxId: string, range: string) =>
    [...cameraKeys.all, "edgeBoxCost", workspaceId, boxId, range] as const,
  workspaceCost: (workspaceId: string, range: string) =>
    [...cameraKeys.all, "workspaceCost", workspaceId, range] as const,
  edgeBoxLogs: (
    workspaceId: string,
    boxId: string,
    minSeverity: string | undefined,
    eventContains: string | undefined
  ) =>
    [
      ...cameraKeys.all,
      "edgeBoxLogs",
      workspaceId,
      boxId,
      minSeverity ?? null,
      eventContains ?? null
    ] as const,
  budgetStatus: (workspaceId: string) => [...cameraKeys.all, "budgetStatus", workspaceId] as const,
  fleetDevices: (workspaceId: string) => [...cameraKeys.all, "fleetDevices", workspaceId] as const,
  auditEvents: (
    workspaceId: string,
    actionPrefix: string | undefined,
    targetId: string | undefined
  ) =>
    [
      ...cameraKeys.all,
      "auditEvents",
      workspaceId,
      actionPrefix ?? null,
      targetId ?? null
    ] as const,
  cameraHealthSummary: (workspaceId: string) =>
    [...cameraKeys.all, "cameraHealthSummary", workspaceId] as const,
  dashboardRollup: (workspaceId: string, siteId: string | null, since: string | undefined) =>
    [...cameraKeys.all, "dashboardRollup", workspaceId, siteId ?? null, since ?? null] as const,
  recentAlerts: (workspaceId: string, siteId: string | null, limit: number) =>
    [...cameraKeys.all, "recentAlerts", workspaceId, siteId ?? null, limit] as const,
  fleetComplianceReports: (
    workspaceId: string,
    siteId: string | null,
    since: string | undefined,
    limit: number
  ) =>
    [
      ...cameraKeys.all,
      "fleetComplianceReports",
      workspaceId,
      siteId ?? null,
      since ?? null,
      limit
    ] as const,
  fleetComplianceSummary: (workspaceId: string, siteId: string | null, since: string | undefined) =>
    [
      ...cameraKeys.all,
      "fleetComplianceSummary",
      workspaceId,
      siteId ?? null,
      since ?? null
    ] as const,
  recordingSegments: (workspaceId: string, cameraId: string) =>
    [...cameraKeys.all, "recordingSegments", workspaceId, cameraId] as const,
  arbitration: (workspaceId: string, reportId: string) =>
    [...cameraKeys.all, "arbitration", workspaceId, reportId] as const,
  unifiCredentialStatus: (workspaceId: string) =>
    [...cameraKeys.all, "unifiCredentialStatus", workspaceId] as const
};

const billingKeys = {
  all: ["billing"] as const,
  org: (orgId: string) => [...billingKeys.all, "org", orgId] as const,
  status: (orgId: string) => [...billingKeys.all, "status", orgId] as const,
  invoices: (orgId: string) => [...billingKeys.all, "invoices", orgId] as const,
  checkoutSession: (orgId: string, sessionId: string) =>
    [...billingKeys.all, "checkoutSession", orgId, sessionId] as const
};

const adminBillingKeys = {
  all: ["admin", "billing"] as const,
  orgs: (status?: string) => [...adminBillingKeys.all, "orgs", status ?? "all"] as const,
  prices: () => [...adminBillingKeys.all, "prices"] as const,
  subscription: (orgId: string) => [...adminBillingKeys.all, "subscription", orgId] as const,
  pendingCheckout: (orgId: string) => [...adminBillingKeys.all, "pendingCheckout", orgId] as const
};

const customAppKeys = {
  // Broad union root — invalidating this refreshes every custom-app query,
  // including the workspace-scoped `list` used by the HQ launcher + rail.
  all: () => ["customApps"] as const,
  manage: () => ["customApps", "manage"] as const,
  debug: (orgSlug: string, appSlug: string) => ["customApps", "debug", orgSlug, appSlug] as const,
  listdir: (path: string, showHidden: boolean) =>
    ["customApps", "listdir", path, showHidden] as const,
  probe: (path: string) => ["customApps", "probe", path] as const,
  builds: (id: string) => ["customApps", "builds", id] as const,
  functions: (id: string) => ["customApps", "functions", id] as const,
  functionInvocations: (id: string, name: string) =>
    ["customApps", "functions", id, name, "invocations"] as const,
  functionRun: (id: string, runId: string) => ["customApps", "functionRun", id, runId] as const,
  /** The app's declared ∪ stored secrets. Never holds a value — reveal is its
   *  own uncached call, so a decrypted secret never sits in the query cache. */
  secrets: (id: string) => ["customApps", "secrets", id] as const,
  activitySummary: (id: string) => ["customApps", "activity", id, "summary"] as const,
  availability: (orgSlug: string, appSlug: string) =>
    ["customApps", "availability", orgSlug, appSlug] as const,
  logs: (orgSlug: string, appSlug: string, hours: number) =>
    ["customApps", "logs", orgSlug, appSlug, hours] as const,
  clientErrors: (orgSlug: string, appSlug: string, hours: number) =>
    ["customApps", "clientErrors", orgSlug, appSlug, hours] as const,
  activityVisitors: (id: string, days: number) =>
    ["customApps", "activity", id, "visitors", days] as const,
  activityEvents: (id: string, days: number, eventName: string | null) =>
    ["customApps", "activity", id, "events", days, eventName] as const,
  // Key includes a version segment so any cache entry recorded
  // against the pre-fix URL (`/api/admin/customer-apps/templates`,
  // which SPA-fell-back to HTML) gets invalidated when this code
  // ships. Bump again if the response shape changes.
  templates: () => ["customApps", "templates", "v2"] as const,
  // Workspace-scoped published list (HQ launcher + workspace rail).
  list: (workspaceId: string) => ["customApps", "list", workspaceId] as const,
  // Storage: fleet rollup vs per-app live S3 listing (two sources, two keys).
  storageFleet: (sort: string) => ["customApps", "storage", "fleet", sort] as const,
  storageObjects: (id: string, prefix: string) =>
    ["customApps", "storage", "objects", id, prefix] as const,
  storageHistory: (days: number, appId?: string) =>
    ["customApps", "storage", "history", days, appId ?? "fleet"] as const
};

const appAdminKeys = {
  all: ["appAdmins"] as const,
  list: () => [...appAdminKeys.all, "list"] as const
};

const publishTokenKeys = {
  all: ["publishTokens"] as const,
  list: () => [...publishTokenKeys.all, "list"] as const
};

const orgSubdomainKeys = {
  all: ["orgSubdomain"] as const,
  status: (workspaceId: string) => [...orgSubdomainKeys.all, "status", workspaceId] as const
};

const oxyAccessKeys = {
  all: ["oxyAccess"] as const,
  status: (workspaceId: string) => [...oxyAccessKeys.all, "status", workspaceId] as const,
  // Platform-wide list of granted workspaces for the admin org/project browser.
  grants: () => [...oxyAccessKeys.all, "grants"] as const
};

const featureFlagKeys = {
  all: ["admin", "feature-flags"] as const,
  list: () => [...featureFlagKeys.all, "list"] as const
};

const internalJobsKeys = {
  all: ["admin", "internal-jobs"] as const,
  queueStats: () => [...internalJobsKeys.all, "queue-stats"] as const,
  recentFailures: (limit?: number) =>
    [...internalJobsKeys.all, "recent-failures", limit ?? 50] as const,
  deadLetter: (limit?: number, offset?: number) =>
    [...internalJobsKeys.all, "dead-letter", limit ?? 50, offset ?? 0] as const,
  workers: () => [...internalJobsKeys.all, "workers"] as const,
  scheduled: () => [...internalJobsKeys.all, "scheduled"] as const
};

const preaggKeys = {
  all: ["preagg-status"] as const,
  status: (projectId: string, branchName?: string) =>
    [...preaggKeys.all, projectId, branchName ?? ""] as const
};

const compilesKeys = {
  all: ["admin", "compiles"] as const,
  list: (params: { limit?: number; workspace_id?: string; org_id?: string; status?: string }) =>
    [
      ...compilesKeys.all,
      "list",
      params.limit ?? 50,
      params.workspace_id ?? "",
      params.org_id ?? "",
      params.status ?? ""
    ] as const,
  detail: (revisionId: string) => [...compilesKeys.all, "detail", revisionId] as const,
  workspaces: (params: { limit?: number; offset?: number; q?: string; status?: string }) =>
    [
      ...compilesKeys.all,
      "workspaces",
      params.limit ?? 50,
      params.offset ?? 0,
      params.q ?? "",
      params.status ?? ""
    ] as const
};

const compileKeys = {
  all: ["compile"] as const,
  status: (workspaceId: string, branch: string) =>
    [...compileKeys.all, "status", workspaceId, branch] as const
};

const adminOrgsKeys = {
  all: ["admin", "orgs"] as const,
  list: (search?: string) => [...adminOrgsKeys.all, "list", search ?? ""] as const,
  detail: (orgId: string) => [...adminOrgsKeys.all, "detail", orgId] as const,
  subdomain: (orgId: string) => [...adminOrgsKeys.all, "subdomain", orgId] as const,
  logo: (orgId: string) => [...adminOrgsKeys.all, "logo", orgId] as const
};

const adminMetricsKeys = {
  all: ["admin", "metrics"] as const,
  llmUsage: (days: number) => [...adminMetricsKeys.all, "llm-usage", days] as const,
  orgLlmUsage: (orgId: string, days: number) =>
    [...adminMetricsKeys.all, "llm-usage", "org", orgId, days] as const
};

interface ExplorerQueryKeyParams {
  search?: string;
  status?: string;
  sourceType?: string;
  orgId?: string;
  page?: number;
  pageSize?: number;
}

const adminExplorerKeys = {
  all: ["admin", "explorer"] as const,
  threads: (params: ExplorerQueryKeyParams) =>
    [...adminExplorerKeys.all, "threads", params] as const,
  runs: (params: ExplorerQueryKeyParams) => [...adminExplorerKeys.all, "runs", params] as const
};

const adminUsersKeys = {
  all: ["admin", "users"] as const,
  // `role` is part of the key: without it, switching the role filter serves the
  // previous filter's page from cache and the list silently disagrees with the control.
  list: (search?: string, status?: string, role?: string) =>
    [...adminUsersKeys.all, "list", search ?? "", status ?? "", role ?? ""] as const,
  detail: (userId: string) => [...adminUsersKeys.all, "detail", userId] as const
};

const adminWorkspacesKeys = {
  all: ["admin", "workspaces"] as const,
  list: (search?: string, status?: string, orgId?: string) =>
    [...adminWorkspacesKeys.all, "list", search ?? "", status ?? "", orgId ?? ""] as const,
  detail: (workspaceId: string) => [...adminWorkspacesKeys.all, "detail", workspaceId] as const
};

const workspaceHealthKeys = {
  all: ["admin", "workspace-health"] as const,
  list: () => [...workspaceHealthKeys.all, "list"] as const
};

const airwayConfigKeys = {
  all: ["admin", "airway-config"] as const,
  config: () => [...airwayConfigKeys.all, "config"] as const,
  /** The deployment (operational) tier — one singleton row, so no parameters. */
  deployment: () => [...airwayConfigKeys.all, "deployment"] as const,
  /**
   * Keyed on **both** admission axes. `undefined` on either means airway's
   * default (`permissive` / `production`), keyed as `null` so it's stable.
   *
   * `environment` belongs in the key even though it once looked cosmetic: a
   * scan computed under `production` says nothing about a save that sets
   * `sandbox` (the source factory refuses connectors there), so a key that
   * omitted it served a stale-but-`isSuccess` body the save gate then read as
   * "confirmed clean". Adding it makes that a cache miss — a refetch, and a
   * gate that honestly says "loading" until the real answer lands.
   */
  preview: (
    sourceKind: string,
    contractPolicy: string | undefined,
    environment: string | undefined
  ) =>
    [
      ...airwayConfigKeys.all,
      "preview",
      sourceKind,
      contractPolicy ?? null,
      environment ?? null
    ] as const
};

const authConfigKeys = {
  all: ["authConfig"] as const,
  current: () => [...authConfigKeys.all] as const
};

const frontlineKeys = {
  all: ["frontline"] as const,
  device: () => [...frontlineKeys.all, "device"] as const,
  roster: (org: string) => [...frontlineKeys.all, "roster", org] as const
};

const appIntegrationKeys = {
  all: ["app-integrations"] as const,
  list: (projectId: string, branchName: string) =>
    [...appIntegrationKeys.all, "list", projectId, branchName] as const
};

const semanticKeys = {
  all: ["semantic"] as const,
  topicDetails: (projectId: string, filePathB64: string | undefined, branchName?: string) =>
    [...semanticKeys.all, "topicDetails", projectId, filePathB64, branchName] as const,
  viewDetails: (projectId: string, filePathB64: string | undefined, branchName?: string) =>
    [...semanticKeys.all, "viewDetails", projectId, filePathB64, branchName] as const
};

const simulationKeys = {
  all: ["simulation"] as const,
  list: (projectId: string, branch: string) =>
    [...simulationKeys.all, "list", projectId, branch] as const,
  runs: (projectId: string) => [...simulationKeys.all, "runs", projectId] as const,
  run: (projectId: string, runId: string) =>
    [...simulationKeys.all, "run", projectId, runId] as const
};

const metricTreeKeys = {
  all: ["metric-tree"] as const,
  tree: (projectId: string, branch: string, root: string | undefined) =>
    [...metricTreeKeys.all, "tree", projectId, branch, root ?? null] as const,
  sensitivity: (projectId: string, branch: string, measureId: string | undefined) =>
    [...metricTreeKeys.all, "sensitivity", projectId, branch, measureId] as const,
  /** Cached period-over-period explain (used by the Insights inbox drawer
   *  so reopening the same anomaly reuses the result instead of re-running
   *  airlayer's recursive search). Keyed by the full request payload. */
  explain: (
    projectId: string,
    branch: string,
    target: string,
    timeDimension: string,
    currentPeriod: readonly [string, string],
    previousPeriod: readonly [string, string],
    deep: boolean
  ) =>
    [
      ...metricTreeKeys.all,
      "explain",
      projectId,
      branch,
      target,
      timeDimension,
      currentPeriod[0],
      currentPeriod[1],
      previousPeriod[0],
      previousPeriod[1],
      deep
    ] as const,
  /** Time dimensions available per view — discovery endpoint that lets the
   *  metric-tree UI offer a real select instead of a hardcoded fallback. */
  timeDimensions: (projectId: string, branch: string) =>
    [...metricTreeKeys.all, "time-dimensions", projectId, branch] as const,
  /** Single-period distribution — same `ExplainResult` shape as `explain`
   *  but the baseline is auto-derived server-side. Keyed by request payload. */
  distribution: (
    projectId: string,
    branch: string,
    target: string,
    timeDimension: string,
    period: readonly [string, string]
  ) =>
    [
      ...metricTreeKeys.all,
      "distribution",
      projectId,
      branch,
      target,
      timeDimension,
      period[0],
      period[1]
    ] as const,
  /** Segment opportunity sizing — per-dimension upside of lifting each segment
   *  to its benchmark peer. Keyed by request payload so the world-model panel
   *  reuses the result when a measure is reselected.
   *
   *  `instance` is part of the key, not decoration: a scoped scan answers a
   *  question about one instance, so sharing a cache entry across instances
   *  would serve one store's upside under another store's name. `null` (the
   *  population-level scan) keys separately from any instance, as it must. */
  opportunity: (
    projectId: string,
    target: string,
    timeDimension: string,
    period: readonly [string, string],
    instance: { entity: string; key: string } | null
  ) =>
    [
      ...metricTreeKeys.all,
      "opportunity",
      projectId,
      target,
      timeDimension,
      period[0],
      period[1],
      instance?.entity ?? null,
      instance?.key ?? null
    ] as const,
  /** Recursive opportunity decomposition — same instance-scoping rationale as
   *  `opportunity`: a scoped drill answers a question about one instance, so
   *  it must key separately from the population-level scan (and from any
   *  other instance). */
  drill: (
    projectId: string,
    target: string,
    timeDimension: string,
    period: readonly [string, string],
    instance: { entity: string; key: string } | null,
    root: { dimension: string; segment: string } | null
  ) =>
    [
      ...metricTreeKeys.all,
      "drill",
      projectId,
      target,
      timeDimension,
      period[0],
      period[1],
      instance?.entity ?? null,
      instance?.key ?? null,
      root?.dimension ?? null,
      root?.segment ?? null
    ] as const,
  /** Baseline values for a lever set. Keyed by the full request because the
   *  warehouse query is expensive: it must refetch when the levers, period or
   *  scope change, and only then. */
  baseline: (
    projectId: string,
    branch: string,
    roots: readonly string[],
    timeDimension: string,
    period: readonly [string, string],
    instanceKey: string | null
  ) =>
    [
      ...metricTreeKeys.all,
      "baseline",
      projectId,
      branch,
      [...roots].sort().join(","),
      timeDimension,
      period[0],
      period[1],
      instanceKey
    ] as const,
  /** Bucketed history + forward forecast for a lever set. Keyed on everything
   *  the warehouse query depends on — granularity and horizon included, since
   *  a coarser bucket is a different query and a longer horizon a different
   *  fit, not a slice of the same answer. */
  projection: (
    projectId: string,
    branch: string,
    roots: readonly string[],
    timeDimension: string,
    period: readonly [string, string],
    instanceKey: string | null,
    granularity: string,
    horizon: number
  ) =>
    [
      ...metricTreeKeys.all,
      "projection",
      projectId,
      branch,
      [...roots].sort().join(","),
      timeDimension,
      period[0],
      period[1],
      instanceKey,
      granularity,
      horizon
    ] as const
};

const worldModelKeys = {
  all: ["world-model"] as const,
  graph: (projectId: string, branch: string) =>
    [...worldModelKeys.all, "graph", projectId, branch] as const,
  instances: (projectId: string, branch: string, entityId: string, search: string) =>
    [...worldModelKeys.all, "instances", projectId, branch, entityId, search] as const,
  filterInstances: (
    projectId: string,
    branch: string,
    seedEntityId: string,
    seedKey: string,
    entityId: string,
    search: string,
    offset: number
  ) =>
    [
      ...worldModelKeys.all,
      "filter-instances",
      projectId,
      branch,
      seedEntityId,
      seedKey,
      entityId,
      search,
      offset
    ] as const,
  filterCounts: (projectId: string, branch: string, entityId: string, keyValue: string) =>
    [...worldModelKeys.all, "filter-counts", projectId, branch, entityId, keyValue] as const,
  instanceDetail: (projectId: string, branch: string, entityId: string, keyValue: string) =>
    [...worldModelKeys.all, "instance-detail", projectId, branch, entityId, keyValue] as const,
  measureBreakdown: (
    projectId: string,
    branch: string,
    entityId: string,
    keyValue: string,
    measure: string
  ) =>
    [
      ...worldModelKeys.all,
      "measure-breakdown",
      projectId,
      branch,
      entityId,
      keyValue,
      measure
    ] as const
};

const metricAnomaliesKeys = {
  all: ["metric-anomalies"] as const,
  /** Every anomaly *list* for one project, whatever its filter or page.
   *
   *  Narrower than `all` in two directions. `all` covers the per-anomaly
   *  `explain` decompositions, and invalidating those on a status write throws
   *  away a 20-30s server-side computation the write cannot have changed; and
   *  it spans projects, so a scan in one would sweep another's cached lists. */
  lists: (projectId: string) => [...metricAnomaliesKeys.all, "list", projectId] as const,
  list: (
    projectId: string,
    status: string | undefined,
    order?: string,
    page?: { limit: number; offset: number }
  ) =>
    [
      ...metricAnomaliesKeys.all,
      "list",
      projectId,
      status ?? null,
      order ?? null,
      page ?? null
    ] as const,
  monitors: (projectId: string) => [...metricAnomaliesKeys.all, "monitors", projectId] as const,
  /** Per-anomaly explain decomposition (cached server-side on the row).
   *  Distinct from `metricTree.explain` because the cache lifecycle is
   *  tied to the anomaly itself, not to the request payload. */
  explain: (projectId: string, anomalyId: string) =>
    [...metricAnomaliesKeys.all, "explain", projectId, anomalyId] as const
};

const queryKeys = {
  airhouse: airhouseKeys,
  oltp: oltpKeys,
  camera: cameraKeys,
  billing: billingKeys,
  adminBilling: adminBillingKeys,
  customApps: customAppKeys,
  appAdmins: appAdminKeys,
  publishTokens: publishTokenKeys,
  partnerPublishConsent: {
    status: (orgId: string) => ["partner-publish-consent", orgId] as const
  },
  oxyAccess: oxyAccessKeys,
  orgSubdomain: orgSubdomainKeys,
  featureFlags: featureFlagKeys,
  internalJobs: internalJobsKeys,
  compiles: compilesKeys,
  compile: compileKeys,
  adminOrgs: adminOrgsKeys,
  adminMetrics: adminMetricsKeys,
  adminExplorer: adminExplorerKeys,
  adminUsers: adminUsersKeys,
  adminWorkspaces: adminWorkspacesKeys,
  authConfig: authConfigKeys,
  frontline: frontlineKeys,
  semantic: semanticKeys,
  metricTree: metricTreeKeys,
  simulation: simulationKeys,
  worldModel: worldModelKeys,
  metricAnomalies: metricAnomaliesKeys,
  org: orgKeys,
  appAccess: appAccessKeys,
  agent: agentKeys,
  builder: builderKeys,
  coordinator: coordinatorKeys,
  analytics: analyticsKeys,
  thread: threadKeys,
  apiKey: apiKeyKeys,
  secret: secretKeys,
  spApi: spApiKeys,
  logs: logsKeys,
  user: userKeys,
  workspaces: workspaceKeys,
  automation: automationKeys,
  agenticAutomation: agenticAutomationKeys,
  airway: airwayKeys,
  schedule: scheduleKeys,
  chart: chartKeys,
  file: fileKeys,
  database: databaseKeys,
  app: appKeys,
  appIntegrations: appIntegrationKeys,
  onboarding: onboardingKeys,
  settings: settingsKeys,
  repositories: repositoryKeys,
  config: configKeys,
  artifact: artifactKeys,
  contextGraph: contextGraphKeys,
  integration: integrationKeys,
  slack: slackKeys,
  trace: traceKeys,
  executionAnalytics: executionAnalyticsKeys,
  testFile: testFileKeys,
  testProjectRun: testProjectRunKeys,
  testRun: testRunKeys,
  humanVerdict: humanVerdictKeys,
  modeling: modelingKeys,
  github: githubKeys,
  preagg: preaggKeys,
  workspaceHealth: workspaceHealthKeys,
  airwayConfig: airwayConfigKeys,
  partner: partnerKeys,
  audit: auditKeys,
  adminAssume: adminAssumeKeys,
  adminPartner: adminPartnerKeys
};

export default queryKeys;
