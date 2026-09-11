use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
pub struct ValidateReturnToQuery {
    pub url: String,
}

#[derive(Deserialize)]
pub struct GoogleAuthRequest {
    pub code: String,
    /// Opaque token issued by `POST /auth/oauth/state`. Required to defend
    /// against CSRF-style cross-user auth injection on the OAuth callback.
    pub state: String,
}

#[derive(Deserialize)]
pub struct OktaAuthRequest {
    pub code: String,
    /// See `GoogleAuthRequest::state`.
    pub state: String,
}

#[derive(Deserialize)]
pub struct MagicLinkRequest {
    pub email: String,
    /// Optional post-login redirect target. Forwarded into the magic-link
    /// email so the user lands back on the requesting page (an
    /// `<app>.oxygen-hq.com` subdomain gated by the external auth proxy). The
    /// web-app, not the server, performs the redirect after `verify`; the
    /// server validates the target via `validate_return_to` before the
    /// browser follows it.
    #[serde(default)]
    pub return_to: Option<String>,
}

#[derive(Deserialize)]
pub struct MagicLinkVerifyRequest {
    pub token: String,
}

/// Body of `POST /auth/dev-login` and query of its `GET` twin. Both fields
/// default, so `{}` (or no query at all) means "the first identity in
/// `OXY_DEV_LOGIN_EMAILS`" — the caller never has to know the allow-list.
#[derive(Deserialize, Default)]
pub struct DevLoginRequest {
    #[serde(default)]
    pub email: Option<String>,
    /// `as=<persona>` — a named seeded identity instead of an email. Exclusive
    /// with `email`; see `dev_login::personas`.
    #[serde(default, rename = "as")]
    pub persona: Option<String>,
}

#[derive(Serialize)]
pub struct AuthResponse {
    pub token: String,
    pub user: UserInfo,
    pub orgs: Vec<OrgInfo>,
}

#[derive(Serialize)]
pub struct OrgInfo {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub role: String,
}

/// Global profile fields. Role / admin status are per-org and live on
/// `OrgInfo` in the login response or on `GET /orgs`. `is_owner` reflects
/// the `OXY_OWNER` allow-list (Oxy staff). `is_app_admin` reflects
/// membership in the `app_admins` table and gates the customer-apps
/// surface.
#[derive(Serialize)]
pub struct UserInfo {
    pub id: String,
    pub email: String,
    pub name: String,
    pub picture: Option<String>,
    pub is_owner: bool,
    pub is_app_admin: bool,
}

#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Claims {
    pub(super) sub: String,
    pub(super) email: String,
    pub(super) exp: usize,
    pub(super) iat: usize,
}

#[derive(Serialize, Deserialize)]
pub(super) struct OAuthStateClaims {
    /// Random nonce — the state is single-use only in practice because the
    /// frontend generates a new one per login attempt.
    pub(super) nonce: String,
    /// Marker claim so a JWT intended for user auth cannot be repurposed as
    /// an OAuth state and vice versa.
    pub(super) purpose: String,
    pub(super) exp: usize,
    pub(super) iat: usize,
}

#[derive(Serialize)]
pub struct OAuthStateResponse {
    pub state: String,
}

#[derive(Serialize)]
pub struct AuthConfigResponse {
    pub auth_enabled: bool,
    pub google: Option<GoogleConfig>,
    pub okta: Option<OktaConfig>,
    pub magic_link: Option<bool>,
    pub enterprise: bool,
    /// True when observability has a backend wired up. False when `--enterprise`
    /// is on but `OXY_OBSERVABILITY_BACKEND` is unset — the UI should surface a
    /// "not configured" banner on observability pages in that case.
    pub observability_enabled: bool,
    /// Present when `GITHUB_CLIENT_ID` is set — enables "Login with GitHub".
    pub github: Option<GitHubAuthConfig>,
    /// "local" when the server is running `oxy serve --local`, "cloud" otherwise.
    /// Frontend uses this to pick a route tree.
    pub mode: &'static str,
    /// Mirror of the backend `billing` feature flag. When false the FE hides
    /// the org Billing settings tab and the admin Billing queue surfaces a
    /// "Billing is disabled" notice instead of a misleading empty state.
    pub billing_enabled: bool,
    /// True when `OXY_DEV_LOGIN_EMAILS` is set, i.e. the dev sign-in bypass is
    /// available. Drives the "Dev sign-in" button on the login page — the one
    /// affordance a browser-automation tool can click without OAuth or email.
    pub dev_login: bool,
}

#[derive(Serialize)]
pub struct GoogleConfig {
    pub client_id: String,
}

#[derive(Serialize)]
pub struct OktaConfig {
    pub client_id: String,
    pub domain: String,
}

#[derive(Serialize)]
pub struct GitHubAuthConfig {
    pub client_id: String,
}

#[derive(Deserialize)]
pub struct GitHubAuthRequest {
    pub code: String,
    /// See `GoogleAuthRequest::state`.
    pub state: String,
}

#[derive(Deserialize)]
pub(super) struct GitHubUserInfo {
    pub(super) name: Option<String>,
    pub(super) login: String,
    pub(super) avatar_url: Option<String>,
    pub(super) email: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct GitHubEmailEntry {
    pub(super) email: String,
    pub(super) primary: bool,
    pub(super) verified: bool,
}

#[derive(Deserialize)]
pub(super) struct OAuthUserInfo {
    pub(super) email: String,
    pub(super) name: String,
    pub(super) picture: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct OktaUserInfo {
    pub(super) email: String,
    pub(super) name: String,
    pub(super) picture: Option<String>,
}
