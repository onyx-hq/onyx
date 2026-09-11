export interface GoogleAuthRequest {
  code: string;
  /** Opaque JWT issued by `POST /auth/oauth/state`. Required; backend rejects with 422 if absent and 401 if invalid. */
  state: string;
}

export interface OktaAuthRequest {
  code: string;
  /** See `GoogleAuthRequest.state`. */
  state: string;
}

export interface GitHubAuthRequest {
  code: string;
  /** See `GoogleAuthRequest.state`. */
  state: string;
}

export interface OAuthStateResponse {
  state: string;
}

export interface MagicLinkRequest {
  email: string;
  /**
   * Optional post-login redirect target. The server forwards this through the
   * magic-link email so the verify-callback page can navigate back to it after
   * the cookie is set. Validated against the allowlist on both server (when
   * embedded into the email) and client (before `window.location.href` redirect).
   */
  return_to?: string;
}

export interface MagicLinkVerifyRequest {
  token: string;
}

export interface AuthResponse {
  token: string;
  user: UserInfo;
  orgs: OrgInfo[];
}

export interface OrgInfo {
  id: string;
  name: string;
  slug: string;
  role: string;
}

/**
 * Global profile fields. Role / admin status are per-org; read from the
 * `orgs` array in the login response or via `GET /orgs`. Two system-wide
 * flags: `is_owner` mirrors `OXY_OWNER` (Oxy staff, admin shell);
 * `is_app_admin` mirrors `OXY_GLOBAL_ADMINS` (gates the customer-apps surface).
 * Both are UX-only — the server enforces independently.
 */
export interface PartnerCapabilities {
  manage_members: boolean;
  /** Publish / unpublish apps only — NOT data access. */
  manage_apps: boolean;
  /** The custom-app data plane (query / semantic-query / agent runs). Default OFF. */
  develop_apps: boolean;
  view_audit: boolean;
  manage_billing: boolean;
  manage_secrets: boolean;
  /** Onboard client orgs. Sensitive — it mints billable tenants. Default OFF. */
  create_orgs: boolean;
  manage_org_settings: boolean;
}

/**
 * A partner this user operates. Non-empty `partner_memberships` means the user
 * should see the partner console; `capabilities` — the partner's **ceiling** — lets
 * the UI hide surfaces this operator can't use. UX-only: the server re-checks on
 * every partner route.
 */
export interface PartnerMembership {
  /** The partner IS an org, so this is an org id. */
  partner_id: string;
  slug: string;
  capabilities: PartnerCapabilities;
}

export interface UserInfo {
  id: string;
  email: string;
  name: string;
  picture?: string;
  status?: string;
  is_owner: boolean;
  is_app_admin: boolean;
  /**
   * What this staff member's platform grant lets them do (`Cap::as_str` in
   * `oxy-authz`). Empty for non-staff; a Global Owner reports every capability.
   *
   * `is_app_admin` says only *that* someone is staff — an App Operator and a Global
   * Admin both report `true` — so nav must gate on this instead. UX only: the server
   * re-decides on every admin route, and hiding an item is not a security control.
   *
   * Present on `GET /user`; the login response omits it, so default to empty.
   */
  platform_capabilities?: PlatformCapability[];
  /**
   * Present on `GET /user` (the canonical role source, `useCurrentUser`); the
   * login response omits it, so treat as optional and default to empty.
   */
  partner_memberships?: PartnerMembership[];
}

/**
 * The platform capability vocabulary, mirroring `oxy_authz::Cap::as_str`. These strings
 * are a wire contract — they are also persisted in the grant table, so renaming one
 * orphans stored grants.
 */
export type PlatformCapability =
  | "manage_members"
  | "manage_apps"
  | "develop_apps"
  | "view_audit"
  | "manage_billing"
  | "manage_secrets"
  | "create_orgs"
  | "manage_org_settings"
  | "view_tenants"
  | "manage_partners"
  | "operate_platform"
  | "manage_platform_grants";

export interface MessageResponse {
  message: string;
}

type ServeMode = "local" | "cloud";

export interface AuthConfigResponse {
  auth_enabled: boolean;
  /**
   * Deployment mode set by the backend. In `local` mode the server skips auth,
   * exposes a reduced route surface, and uses the nil-UUID workspace — the
   * frontend must mirror that by hiding org/auth/workspace-management UI.
   */
  mode: ServeMode;
  google?: {
    client_id: string;
  };
  okta?: {
    client_id: string;
    domain: string;
  };
  magic_link?: boolean;
  github?: { client_id: string };
  enterprise?: boolean;
  /**
   * True when the observability backend is wired up (OXY_OBSERVABILITY_BACKEND
   * set to clickhouse — the sole backend). When false on an enterprise build,
   * observability pages render a "not configured" banner and record nothing.
   * Always present — server serializes the bool unconditionally.
   */
  observability_enabled: boolean;
  /**
   * Mirror of the backend `billing` feature flag. When false the FE hides
   * the org Billing settings tab and the admin Billing queue renders a
   * "Billing is disabled" notice instead of calling endpoints that 503.
   */
  billing_enabled: boolean;
  /**
   * Whether the dev-only sign-in bypass is reachable **by this caller** — the
   * server computes it per request, not once per process. An allow-list
   * inferred from the staff roster (debug builds, no `OXY_DEV_LOGIN_EMAILS`)
   * is honored for loopback callers only, so this is false off-box even
   * though the server has one. Renders the "Dev sign-in" button; `/dev-login`
   * itself is always routable and explains the refusal. Never true in a real
   * deployment.
   */
  dev_login: boolean;
}

/**
 * `POST /auth/dev-login`. Omit `email` to sign in as the first identity in
 * `OXY_DEV_LOGIN_EMAILS`; any email passed must be on that list or the server
 * returns 403. `as` names a seeded persona (`staff`, `owner`, `member`,
 * `operator`, `partner`) instead — exclusive with `email`.
 */
export interface DevLoginRequest {
  email?: string;
  as?: string;
}
