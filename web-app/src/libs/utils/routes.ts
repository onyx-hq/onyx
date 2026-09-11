const ROUTES = {
  ROOT: "/",
  ONBOARDING: "/onboarding",
  /** Partner console — a partner's home. Mirrors the ADMIN surface. */
  PARTNERS: {
    ROOT: "/partners",
    APPS: "/partners/apps",
    TEAM: "/partners/team",
    ACTIVITY: "/partners/activity"
  },

  AUTH: {
    LOGIN: "/login",
    GOOGLE_CALLBACK: "/auth/google/callback",
    OKTA_CALLBACK: "/auth/okta/callback",
    MAGIC_LINK_CALLBACK: "/auth/magic-link/callback",
    GITHUB_AUTH_CALLBACK: "/auth/github/callback",
    /** Dev-only bypass; mounted only when `authConfig.dev_login` is true. */
    DEV_LOGIN: "/dev-login"
  },
  GITHUB: {
    CALLBACK: "/github/callback"
  },

  INVITE: (token: string) => `/invite/${token}`,

  ADMIN: {
    BILLING_QUEUE: "/admin/billing/queue",
    FEATURE_FLAGS: "/admin/feature-flags",
    INTERNAL_JOBS: "/admin/internal-jobs",
    COMPILES: "/admin/compiles",
    EXPLORER: "/admin/explorer",
    AUDIT: "/admin/audit",
    CUSTOMER_APPS: "/admin/apps",
    AIRHOUSE: "/admin/airhouse",
    APP_ADMINS: "/admin/app-admins",
    PUBLISH_TOKENS: "/admin/publish-tokens",
    TENANTS: "/admin/tenants",
    ORGS: "/admin/orgs",
    OLTP: "/admin/oltp",
    ORG_DETAIL: (orgId: string) => `/admin/orgs/${orgId}`,
    USERS: "/admin/users",
    USER_DETAIL: (userId: string) => `/admin/users/${userId}`,
    WORKSPACES: "/admin/workspaces",
    WORKSPACE_DETAIL: (workspaceId: string) => `/admin/workspaces/${workspaceId}`,
    WORKSPACE_HEALTH: "/admin/workspace-health",
    AIRWAY: "/admin/airway"
  },

  // Org-scoped routes. Passing an empty `orgSlug` degrades to flat local-mode
  // paths (e.g. `/home` instead of `/acme/workspaces/<id>/home`) — in local
  // mode there's no org and a single implicit workspace, and every call site
  // already defaults to `useCurrentOrg(...).slug ?? ""`, so this makes URL
  // building mode-agnostic without touching the ~100 consumers.
  ORG: (orgSlug: string) => {
    const isLocal = orgSlug === "";
    const base = isLocal ? "" : `/${orgSlug}`;
    return {
      ROOT: isLocal ? "/" : base,
      WORKSPACES: `${base}/workspaces`,
      MEMBERS: `${base}/members`,
      SETTINGS: `${base}/settings`,
      BILLING: {
        CHECKOUT_SUCCESS: `${base}/billing/checkout-success`,
        CHECKOUT_CANCELLED: `${base}/billing/checkout-cancelled`
      },

      WORKSPACE: (wsId: string) => {
        const wsBase = isLocal ? "" : `${base}/workspaces/${wsId}`;
        return {
          ROOT: wsBase,
          HOME: `${wsBase}/home`,
          NEW: `${wsBase}/new`,

          THREADS: `${wsBase}/threads`,
          THREAD: (threadId: string) => `${wsBase}/threads/${threadId}`,

          // Canonical "Automations" routes (formerly Automations / Automations).
          AUTOMATIONS: `${wsBase}/automations`,
          AUTOMATION: (pathb64: string) => ({
            ROOT: `${wsBase}/automations/${pathb64}`
          }),

          // Back-compat aliases — same key names kept so existing call sites
          // compile unchanged, but they now resolve to the canonical
          // `/automations` paths. The legacy `/automations` URL still renders
          // (see App.tsx) for old bookmarks/deep links.
          WORKFLOWS: `${wsBase}/automations`,
          WORKFLOW: (pathb64: string) => ({
            ROOT: `${wsBase}/automations/${pathb64}`
          }),

          PIPELINE: (pathb64: string) => ({
            ROOT: `${wsBase}/pipelines/${pathb64}`,
            RUN: (runId: string) => `${wsBase}/pipelines/${pathb64}/runs/${runId}`
          }),

          APP: (pathb64: string) => `${wsBase}/apps/${pathb64}`,

          IDE: {
            ROOT: `${wsBase}/ide`,
            FILES: {
              ROOT: `${wsBase}/ide/files`,
              FILE: (pathb64: string) => `${wsBase}/ide/files/${pathb64}`,
              LOOKER_EXPLORE: (integrationName: string, model: string, exploreName: string) =>
                `${wsBase}/ide/files/looker/${encodeURIComponent(integrationName)}/${encodeURIComponent(model)}/${encodeURIComponent(exploreName)}`
            },
            DATABASE: {
              ROOT: `${wsBase}/ide/database`
            },
            TESTS: {
              ROOT: `${wsBase}/ide/tests`,
              RUNS: `${wsBase}/ide/tests/runs`,
              TEST_FILE: (pathb64: string) => `${wsBase}/ide/tests/${pathb64}`
            },
            COORDINATOR: {
              ROOT: `${wsBase}/ide/coordinator`,
              OVERVIEW: `${wsBase}/ide/coordinator/overview`,
              JOBS: `${wsBase}/ide/coordinator/jobs`,
              JOB_DETAIL: (scheduleId: string) => `${wsBase}/ide/coordinator/jobs/${scheduleId}`,
              RUNS: `${wsBase}/ide/coordinator/runs`,
              RUN_DETAIL: (runId: string) => `${wsBase}/ide/coordinator/runs/${runId}`,
              RECOVERY: `${wsBase}/ide/coordinator/recovery`,
              QUEUE: `${wsBase}/ide/coordinator/queue`
            },
            OBSERVABILITY: {
              ROOT: `${wsBase}/ide/observability`,
              TRACES: `${wsBase}/ide/observability/traces`,
              TRACE: (traceId: string) => `${wsBase}/ide/observability/traces/${traceId}`,
              CLUSTERS: `${wsBase}/ide/observability/clusters`,
              CLUSTERS_V2: `${wsBase}/ide/observability/clusters-v2`,
              METRICS: `${wsBase}/ide/observability/metrics`,
              METRIC: (metricName: string) =>
                `${wsBase}/ide/observability/metrics/${encodeURIComponent(metricName)}`,
              EXECUTION_ANALYTICS: `${wsBase}/ide/observability/execution-analytics`
            },
            MODELING: {
              ROOT: `${wsBase}/ide/modeling`
            },
            SEMANTIC: {
              ROOT: `${wsBase}/ide/semantic`
            },
            WORLD_MODEL: {
              ROOT: `${wsBase}/ide/world-model`
            },
            EDGE: {
              ROOT: `${wsBase}/ide/edge`,
              DASHBOARD: `${wsBase}/ide/edge`,
              PLAYBACK: `${wsBase}/ide/edge/playback`,
              DETECTIONS: `${wsBase}/ide/edge/detections`,
              TOPOLOGY: `${wsBase}/ide/edge/topology`,
              DEVICES: `${wsBase}/ide/edge/devices`,
              BOX: (boxId: string) => `${wsBase}/ide/edge/boxes/${boxId}`,
              ROLLOUTS: `${wsBase}/ide/edge/rollouts`,
              ROLLOUT: (planId: string) => `${wsBase}/ide/edge/rollouts/${planId}`,
              AUDIT: `${wsBase}/ide/edge/audit`,
              PACK: `${wsBase}/ide/edge/pack`
            }
          },

          CONTEXT_GRAPH: `${wsBase}/context-graph`,
          CUSTOMER_APPS: `${wsBase}/apps`,
          ONBOARDING: `${wsBase}/onboarding`
        };
      }
    };
  }
} as const;

export default ROUTES;
