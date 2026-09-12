use std::sync::atomic::{AtomicBool, Ordering};

use crate::constants::{AUTHENTICATION_HEADER_KEY, AUTHENTICATION_SECRET_KEY, SESSION_COOKIE_NAME};
use oxy_shared::errors::OxyError;

use crate::{api_key_infra::authenticate_header, authenticator::Authenticator, types::Identity};
use jsonwebtoken::{DecodingKey, Validation, decode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    sub: String,
    email: String,
    exp: usize,
    iat: usize,
}

/// Process-wide flag toggled by the host (typically `oxy-app` at startup,
/// after parsing the OxyConfig) to tell `BuiltInAuthenticator` whether any
/// auth provider is configured. Defaults to `false` so zero-config installs
/// keep working in guest mode without the host having to call this.
///
/// This indirection exists so `oxy-auth` does not depend on the `oxy` crate
/// (the parsed OxyConfig lives there). Host calls
/// [`set_auth_configured`] once after config load.
static AUTH_CONFIGURED: AtomicBool = AtomicBool::new(false);

/// Tell `BuiltInAuthenticator` whether at least one auth provider (Google,
/// Okta, magic link, …) is configured. Call once at startup from the host.
pub fn set_auth_configured(value: bool) {
    AUTH_CONFIGURED.store(value, Ordering::Relaxed);
}

fn auth_configured() -> bool {
    AUTH_CONFIGURED.load(Ordering::Relaxed)
}

pub struct BuiltInAuthenticator;

impl Default for BuiltInAuthenticator {
    fn default() -> Self {
        Self
    }
}

impl BuiltInAuthenticator {
    pub fn new() -> Self {
        Self
    }
}

impl Authenticator for BuiltInAuthenticator {
    type Error = OxyError;

    async fn authenticate(&self, header: &axum::http::HeaderMap) -> Result<Identity, Self::Error> {
        // Check if any authentication methods are configured.
        // If YES: enforce authentication.
        // If NO: use guest user (backward compatibility for zero-config local installs).
        if !auth_configured() {
            return Ok(Identity {
                // No id: this sentinel exists so `get_or_create_user` MINTS the
                // guest row on a zero-config install. Naming an id would turn
                // the first request on a fresh database into a hard failure.
                user_id: None,
                picture: None,
                name: Some("Local User".to_string()),
                email: crate::user::LOCAL_GUEST_EMAIL.to_string(),
            });
        }

        match self.extract_token(header) {
            Ok(token) => match self.validate(&token) {
                Ok(identity) => return Ok(identity),
                Err(err) => tracing::debug!("JWT validation failed, will try API key: {}", err),
            },
            Err(err) => tracing::debug!("No JWT token extracted: {}", err),
        }

        // Fallback to X-API-Key header authentication.
        authenticate_header(header).await
    }
}

impl BuiltInAuthenticator {
    fn extract_token(&self, header: &axum::http::HeaderMap) -> Result<String, OxyError> {
        tracing::debug!("Extracting JWT token from header");
        if let Some(raw) = header
            .get(AUTHENTICATION_HEADER_KEY)
            .and_then(|v| v.to_str().ok())
        {
            // Accept both forms: the web app's axios sends the bare JWT with
            // no scheme (`Authorization: <jwt>`), while the CLI / `oxyc login`
            // and every standard HTTP client send `Authorization: Bearer <jwt>`.
            // Strip an optional (case-insensitive) `Bearer ` prefix before
            // decoding so a bearer-scheme client isn't rejected with the whole
            // "Bearer …" string treated as the token.
            let token = raw
                .strip_prefix("Bearer ")
                .or_else(|| raw.strip_prefix("bearer "))
                .unwrap_or(raw)
                .trim();
            if !token.is_empty() {
                return Ok(token.to_string());
            }
        }

        // Fallback: pull JWT from the session cookie set by /auth/* login
        // endpoints. The cookie carries the same JWT as the bearer header so
        // `validate()` accepts it identically. Used by browser traffic on
        // `*.oxygen-hq.com` subdomains that the external auth proxy gates.
        extract_session_cookie(header).ok_or(OxyError::AuthenticationError(
            "Missing or invalid authentication header".to_string(),
        ))
    }

    fn validate(&self, value: &str) -> Result<Identity, OxyError> {
        let token_data = decode::<Claims>(
            value,
            &DecodingKey::from_secret(AUTHENTICATION_SECRET_KEY.as_bytes()),
            &Validation::default(),
        )
        .map_err(|err| {
            tracing::error!("JWT validation failed: {}", err);
            OxyError::AuthenticationError(format!("Invalid JWT token: {err}"))
        })?;

        // `sub` has carried the user id since tokens were introduced, and it
        // is what makes a session resolvable for a user with no address. Parsed
        // leniently: a token whose `sub` is not a uuid falls back to the email
        // claim rather than being rejected, so nothing minted before this
        // change stops working mid-deploy.
        Ok(Identity {
            user_id: uuid::Uuid::parse_str(&token_data.claims.sub).ok(),
            picture: None,
            name: None,
            email: token_data.claims.email,
        })
    }
}

/// Look up the `oxy_session` cookie value in the request's `Cookie` header.
/// Returns `None` if the header is absent or the cookie is missing/empty.
/// Cookie headers are formatted as `name1=value1; name2=value2; ...` per
/// RFC 6265.
///
/// The canonical `oxy_session` parser — reuse this rather than re-implementing
/// the RFC 6265 split (callers drifted on the empty-value guard before).
pub fn extract_session_cookie(header: &axum::http::HeaderMap) -> Option<String> {
    let prefix = format!("{SESSION_COOKIE_NAME}=");
    for value in header.get_all("cookie").iter() {
        let raw = match value.to_str() {
            Ok(v) => v,
            Err(_) => continue,
        };
        for part in raw.split(';') {
            let trimmed = part.trim();
            if let Some(token) = trimmed.strip_prefix(prefix.as_str())
                && !token.is_empty()
            {
                return Some(token.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn make_headers(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("cookie", cookie.parse().unwrap());
        h
    }

    #[test]
    fn extract_session_cookie_returns_value_when_alone() {
        let h = make_headers("oxy_session=jwt-token");
        assert_eq!(extract_session_cookie(&h).as_deref(), Some("jwt-token"));
    }

    #[test]
    fn extract_session_cookie_returns_value_when_with_others() {
        let h = make_headers("foo=bar; oxy_session=jwt-token; baz=qux");
        assert_eq!(extract_session_cookie(&h).as_deref(), Some("jwt-token"));
    }

    #[test]
    fn extract_session_cookie_returns_none_when_missing() {
        let h = make_headers("foo=bar; baz=qux");
        assert!(extract_session_cookie(&h).is_none());
    }

    #[test]
    fn extract_session_cookie_returns_none_when_empty_value() {
        let h = make_headers("oxy_session=; foo=bar");
        assert!(extract_session_cookie(&h).is_none());
    }

    #[test]
    fn extract_token_prefers_authorization_header() {
        let mut h = HeaderMap::new();
        h.insert("authorization", "header-jwt".parse().unwrap());
        h.insert("cookie", "oxy_session=cookie-jwt".parse().unwrap());
        let auth = BuiltInAuthenticator::new();
        assert_eq!(auth.extract_token(&h).unwrap(), "header-jwt");
    }

    #[test]
    fn extract_token_strips_bearer_prefix() {
        // CLI / standard clients send `Authorization: Bearer <jwt>`; the token
        // must come back without the scheme so it decodes as a JWT.
        let mut h = HeaderMap::new();
        h.insert("authorization", "Bearer header-jwt".parse().unwrap());
        let auth = BuiltInAuthenticator::new();
        assert_eq!(auth.extract_token(&h).unwrap(), "header-jwt");

        let mut h2 = HeaderMap::new();
        h2.insert("authorization", "bearer header-jwt".parse().unwrap());
        assert_eq!(auth.extract_token(&h2).unwrap(), "header-jwt");
    }

    #[test]
    fn extract_token_falls_back_to_cookie() {
        let h = make_headers("oxy_session=cookie-jwt");
        let auth = BuiltInAuthenticator::new();
        assert_eq!(auth.extract_token(&h).unwrap(), "cookie-jwt");
    }

    #[test]
    fn extract_token_errors_when_neither_present() {
        let h = HeaderMap::new();
        let auth = BuiltInAuthenticator::new();
        assert!(auth.extract_token(&h).is_err());
    }

    #[test]
    fn extract_session_cookie_handles_quoted_value() {
        let h = make_headers(r#"oxy_session="quoted-value""#);
        // We deliberately don't unwrap quotes in v1 — store treats quoted as part
        // of the JWT, which then fails validation. Test documents this behavior.
        assert_eq!(
            extract_session_cookie(&h).as_deref(),
            Some(r#""quoted-value""#)
        );
    }
}

#[cfg(test)]
mod session_identity_tests {
    use super::*;
    use crate::authenticator::Authenticator;
    use jsonwebtoken::{EncodingKey, Header, encode};

    fn token(sub: &str, email: &str) -> String {
        let now = chrono::Utc::now().timestamp() as usize;
        encode(
            &Header::default(),
            &Claims {
                sub: sub.to_string(),
                email: email.to_string(),
                exp: now + 3600,
                iat: now,
            },
            &EncodingKey::from_secret(AUTHENTICATION_SECRET_KEY.as_bytes()),
        )
        .expect("sign")
    }

    #[test]
    fn a_session_names_its_user_by_id() {
        // The property frontline sign-in depends on. A worker enrolled by PIN
        // has a NULL `users.email`, so resolving a session by the email claim
        // finds nobody — `sub` is the only identifier that works, and it has
        // carried the user id since tokens were introduced.
        let id = uuid::Uuid::new_v4();
        let identity = BuiltInAuthenticator
            .validate(&token(&id.to_string(), ""))
            .expect("validate");
        assert_eq!(identity.user_id, Some(id));
    }

    #[test]
    fn a_token_minted_before_this_change_still_resolves() {
        // Deploy safety: a `sub` that is not a uuid must fall back to the email
        // claim rather than being rejected, or every session in flight breaks
        // the moment this ships.
        let identity = BuiltInAuthenticator
            .validate(&token("legacy-subject", "ada@acme.com"))
            .expect("validate");
        assert_eq!(identity.user_id, None);
        assert_eq!(identity.email, "ada@acme.com");
    }
}
