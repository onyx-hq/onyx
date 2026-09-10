import type {
  AppAvailability,
  AppBuildHistory,
  AppFunctionSummary,
  AppSecrets,
  BatchAppResult,
  ClientError,
  CreateAppRequest,
  CustomApp,
  CustomAppDebug,
  CustomAppSummary,
  FunctionInvocation,
  FunctionLogLine,
  FunctionRunDetail,
  ListdirResponse,
  OxyAccessRow,
  ProbeResponse,
  Template,
  UpdateAppRequest
} from "@/types/apps";
import { apiClient } from "./axios";

/**
 * Customer-apps registry — gated by OXY_GLOBAL_ADMINS on the server. CRUD is
 * uuid-keyed (internal callers always know the uuid); sync uses the pretty
 * `<org_slug>/<app_slug>` path because CI calls it without ever seeing the
 * uuid.
 */
export const CustomAppsService = {
  /**
   * Paged admin list. The server orders rows by `updated_at` DESC so
   * page 0 is "what got touched most recently"; `next_offset` is null
   * when we've walked off the end. Callers without an explicit page
   * use the server default size (50) — fine for the first paint, and
   * the infinite-query hook walks back from there.
   */
  async list(
    options: { limit?: number; offset?: number } = {}
  ): Promise<{ items: CustomApp[]; next_offset: number | null }> {
    const params = new URLSearchParams();
    if (options.limit != null) params.set("limit", String(options.limit));
    if (options.offset != null) params.set("offset", String(options.offset));
    const suffix = params.toString();
    const response = await apiClient.get(`/customer-apps${suffix ? `?${suffix}` : ""}`);
    return response.data;
  },

  /**
   * Workspace-scoped published list — the lighter `CustomAppSummary` shape
   * the HQ launcher + workspace rail render, hitting the workspace router's
   * `/{workspaceId}/custom-apps`. Distinct from the paged admin `list` above.
   */
  async listForWorkspace(workspaceId: string): Promise<CustomAppSummary[]> {
    const response = await apiClient.get(`/${workspaceId}/custom-apps`);
    return response.data;
  },

  /**
   * Every workspace that granted Oxy access, platform-wide, for the admin
   * org/project browser. App-admin gated (same surface as the registry).
   */
  async listOxyAccess(): Promise<OxyAccessRow[]> {
    const response = await apiClient.get("/customer-apps/oxy-access");
    return response.data;
  },

  async create(req: CreateAppRequest): Promise<CustomApp> {
    const response = await apiClient.post("/customer-apps", req);
    return response.data;
  },

  async update(id: string, req: UpdateAppRequest): Promise<CustomApp> {
    const response = await apiClient.patch(`/customer-apps/${id}`, req);
    return response.data;
  },

  async delete(id: string): Promise<void> {
    await apiClient.delete(`/customer-apps/${id}`);
  },

  /**
   * Publish the app. Until published, only Oxy staff (with workspace
   * oxy-access) can reach the URL; once published, it surfaces in
   * the owning workspace's sidebar.
   *
   * Hits the `/customer-apps/*` mount (app-admin gated) rather than
   * `/admin/apps/*` (OXY_OWNER gated) — publishing is a normal
   * Oxy-engineer automation.
   */
  async publish(id: string): Promise<CustomApp> {
    const response = await apiClient.post(`/customer-apps/${id}/publish`);
    return response.data;
  },

  async unpublish(id: string): Promise<CustomApp> {
    const response = await apiClient.delete(`/customer-apps/${id}/publish`);
    return response.data;
  },

  /**
   * Batch publish / unpublish / delete for the admin apps table. Each is
   * best-effort per-id server-side: one app failing never aborts the rest,
   * and the response carries a per-app outcome so the UI can report
   * "published 4, 1 failed" from a single call. `delete` is a POST (not
   * DELETE) because the id set travels in the request body.
   */
  async batchPublish(ids: string[]): Promise<BatchAppResult> {
    const response = await apiClient.post("/customer-apps/batch/publish", { ids });
    return response.data;
  },

  async batchPromoteLatest(ids: string[]): Promise<BatchAppResult> {
    const response = await apiClient.post("/customer-apps/batch/promote-latest", { ids });
    return response.data;
  },

  async batchUnpublish(ids: string[]): Promise<BatchAppResult> {
    const response = await apiClient.post("/customer-apps/batch/unpublish", { ids });
    return response.data;
  },

  async batchDelete(ids: string[]): Promise<BatchAppResult> {
    const response = await apiClient.post("/customer-apps/batch/delete", { ids });
    return response.data;
  },

  /**
   * Versioned build history for the new publish pipeline, newest first.
   * Empty for legacy s3/local/v0 rows never published via `oxy publish`.
   */
  async listBuilds(id: string): Promise<AppBuildHistory> {
    const response = await apiClient.get(`/customer-apps/${id}/builds`);
    return response.data;
  },

  /**
   * Roll the published channel back to a retained build. Pure pointer
   * move server-side — the build's bytes already live in S3.
   */
  async rollback(id: string, buildId: string): Promise<CustomApp> {
    const response = await apiClient.post(`/customer-apps/${id}/rollback`, {
      build_id: buildId
    });
    return response.data;
  },

  // ── Oxy Functions (manage / debug) ─────────────────────────────────────────

  /** The app's Oxy Functions in its active build, with their manifest config. */
  async listFunctions(id: string): Promise<AppFunctionSummary[]> {
    const response = await apiClient.get(`/customer-apps/${id}/functions`);
    return response.data;
  },

  /** Recent invocations of one function (newest first), for the debug history. */
  async listFunctionInvocations(id: string, name: string): Promise<FunctionInvocation[]> {
    const response = await apiClient.get(
      `/customer-apps/${id}/functions/${encodeURIComponent(name)}/invocations`
    );
    return response.data;
  },

  /** A single function-job run's status + persisted logs (trigger-and-watch). */
  async getFunctionRun(id: string, runId: string): Promise<FunctionRunDetail> {
    const response = await apiClient.get(`/customer-apps/${id}/function-runs/${runId}`);
    return response.data;
  },

  /** Trigger a one-off background run of a function as a job, optionally with
   *  JSON input params handed to the function as its request body. Returns the
   *  run id to watch via `getFunctionRun`. */
  async runFunction(id: string, name: string, input?: unknown): Promise<{ run_id: string }> {
    const response = await apiClient.post(
      `/customer-apps/${id}/functions/${encodeURIComponent(name)}/runs`,
      // `undefined` → axios sends no body → the function runs with no params.
      input ?? undefined
    );
    return response.data;
  },

  // ── App-scoped secrets (`apps/<app_id>/<KEY>` — what `ctx.env` reads) ──────

  /** The app's secrets: what its active build declares, unioned with what is
   *  actually stored. Values are never included. */
  async listSecrets(id: string): Promise<AppSecrets> {
    const response = await apiClient.get(`/customer-apps/${id}/secrets`);
    return response.data;
  },

  /** Create or rotate one key. The only write path there is — the project
   *  secrets API rejects the `/` in an app-scoped name. */
  async setSecret(id: string, key: string, value: string): Promise<void> {
    await apiClient.post(`/customer-apps/${id}/secrets`, { key, value });
  },

  async deleteSecret(id: string, key: string): Promise<void> {
    await apiClient.delete(`/customer-apps/${id}/secrets/${encodeURIComponent(key)}`);
  },

  /** Tenant-side create/rotate, on the workspace's own route. Same handler as
   *  `setSecret`, reached with a `WorkspaceAdmin` guard instead of staff
   *  standing — this is the only app-secret verb the workspace surface needs,
   *  since the project-secrets routes already list, reveal, rotate and delete
   *  these rows by id. */
  async setWorkspaceAppSecret(
    workspaceId: string,
    appId: string,
    key: string,
    value: string
  ): Promise<void> {
    await apiClient.post(`/${workspaceId}/custom-apps/${appId}/secrets`, { key, value });
  },

  /** Read one value back. Deliberately not a query — a decrypted secret should
   *  never land in the react-query cache, so this is called on demand and the
   *  result held only as long as the row is expanded. */
  async revealSecret(id: string, key: string): Promise<string> {
    const response = await apiClient.get<{ key: string; value: string }>(
      `/customer-apps/${id}/secrets/${encodeURIComponent(key)}/value`
    );
    return response.data.value;
  },

  /**
   * Flip this staff session into draft-preview mode. Sets an HttpOnly
   * cookie that the serve + data-products handlers read to route to
   * the draft channel. App-admin gated.
   */
  async enablePreviewDraft(): Promise<void> {
    await apiClient.post("/customer-apps/preview-draft");
  },

  async disablePreviewDraft(): Promise<void> {
    await apiClient.delete("/customer-apps/preview-draft");
  },

  /**
   * Diagnostic snapshot — what the server currently sees about the app's
   * bundle dir + resolved manifest + producer wiring. Powers the
   * Info tab in the admin detail pane.
   */
  async debug(app: Pick<CustomApp, "slug" | "org_slug">): Promise<CustomAppDebug> {
    const response = await apiClient.get(
      `/customer-apps/${encodeURIComponent(app.org_slug)}/${encodeURIComponent(app.slug)}/debug`
    );
    return response.data;
  },

  /**
   * Server-side folder picker. Local-mode only — returns 404 in cloud
   * and the dialog hides the picker on that surface. Pass an absolute
   * path or an empty string for the server-chosen default landing
   * (`$OXY_STATE_DIR/customer-apps` if present, else `$HOME`).
   */
  async listdir(path: string, showHidden = false): Promise<ListdirResponse> {
    const params = new URLSearchParams();
    if (path) params.set("path", path);
    if (showHidden) params.set("show_hidden", "true");
    const response = await apiClient.get(`/customer-apps/fs/listdir?${params.toString()}`);
    return response.data;
  },

  /**
   * Bundle identity probe — reads `oxy-app.json` + `index.html` from
   * the picked folder so the dialog can lock the slug to whatever the
   * bundle declares. Only available in local mode; returns 404 in cloud.
   */
  async probe(path: string): Promise<ProbeResponse> {
    const params = new URLSearchParams({ path });
    const response = await apiClient.get(`/customer-apps/fs/probe?${params.toString()}`);
    return response.data;
  },

  /**
   * List the curated scaffold templates available on this server.
   * Templates are baked into the binary and never change at runtime.
   *
   * Defensive: validate the response is an array before returning.
   * The endpoint sits behind the SPA fallback in oxy's router, so a
   * routing regression / stale dev-server proxy could return
   * `index.html` (HTML string) instead of JSON. Without this check
   * the consumer's `.map()` would crash the entire dialog.
   */
  async listTemplates(): Promise<Template[]> {
    const { data } = await apiClient.get<unknown>("/customer-apps/templates");
    if (!Array.isArray(data)) {
      throw new Error(
        `Templates endpoint returned ${typeof data} instead of an array — ` +
          `is /customer-apps/templates falling through to the SPA fallback? ` +
          `Sample: ${JSON.stringify(data).slice(0, 80)}`
      );
    }
    return data as Template[];
  },

  // ── Availability (derived SLI) ─────────────────────────────────────────

  /**
   * Slug-addressed, not id-addressed like the Activity calls below — the
   * endpoint is a sibling of `/health` and `/debug`, which authenticate inline
   * and resolve the app from `<org>/<app>` so one monitor can watch every app
   * from the admin host.
   *
   * A 501 here is not a failure: it means `OXY_OBSERVABILITY_BACKEND` is unset,
   * which is the default on a dev box. It is surfaced as a distinct state
   * rather than an error toast.
   */
  async availability(orgSlug: string, appSlug: string): Promise<AppAvailability> {
    const r = await apiClient.get(`/customer-apps/${orgSlug}/${appSlug}/availability`);
    return r.data as AppAvailability;
  },

  /** Persisted Oxy Function output. Slug-addressed, like its `/availability` sibling. */
  async logs(orgSlug: string, appSlug: string, hours = 24): Promise<FunctionLogLine[]> {
    const r = await apiClient.get(
      `/customer-apps/${orgSlug}/${appSlug}/logs?hours=${hours}&limit=200`
    );
    return (r.data?.logs ?? []) as FunctionLogLine[];
  },

  /** Browser errors, grouped by stack, with source maps applied server-side. */
  async clientErrors(orgSlug: string, appSlug: string, hours = 24): Promise<ClientError[]> {
    const r = await apiClient.get(
      `/customer-apps/${orgSlug}/${appSlug}/errors?hours=${hours}&limit=100`
    );
    return (r.data?.errors ?? []) as ClientError[];
  },

  // ── Activity (usage tracking) ──────────────────────────────────────────

  async activitySummary(id: string): Promise<ActivitySummary> {
    const r = await apiClient.get(`/customer-apps/${id}/activity/summary`);
    return r.data as ActivitySummary;
  },

  async activityVisitors(id: string, days = 7, limit = 50): Promise<VisitorRow[]> {
    const r = await apiClient.get(
      `/customer-apps/${id}/activity/visitors?days=${days}&limit=${limit}`
    );
    return (r.data?.rows ?? []) as VisitorRow[];
  },

  async activityEventGroups(id: string, days = 7): Promise<EventGroupRow[]> {
    const r = await apiClient.get(`/customer-apps/${id}/activity/events?days=${days}`);
    return (r.data?.groups ?? []) as EventGroupRow[];
  },

  async activityEventOccurrences(
    id: string,
    eventName: string,
    days = 7,
    limit = 50
  ): Promise<EventOccurrenceRow[]> {
    const r = await apiClient.get(
      `/customer-apps/${id}/activity/events?days=${days}&limit=${limit}&event_name=${encodeURIComponent(eventName)}`
    );
    return (r.data?.rows ?? []) as EventOccurrenceRow[];
  }
};

export interface ActivitySummary {
  total_views_7d: number;
  unique_users_7d: number;
  total_events_7d: number;
  last_viewed_at: string | null;
}

export interface VisitorRow {
  user_id: string;
  user_email: string;
  sessions: number;
  views: number;
  first_seen_at: string;
  last_seen_at: string;
  /** Role in this app on their latest view that recorded one — a snapshot, not
   *  their role today. `null` for views predating role capture. */
  app_role: "admin" | "member" | null;
  /** Role in the owning org, same basis. */
  org_role: "owner" | "admin" | "member" | null;
}

export interface EventGroupRow {
  event_name: string;
  count: number;
  last_fired_at: string;
}

export interface EventOccurrenceRow {
  id: string;
  event_name: string;
  user_email: string;
  payload: Record<string, unknown>;
  occurred_at: string;
}
