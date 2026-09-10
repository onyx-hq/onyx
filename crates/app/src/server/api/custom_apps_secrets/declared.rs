//! Which env keys an app *declares*, and reconciling those against the secrets
//! actually stored under `apps/<app_id>/`.
//!
//! Two sources of declarations, merged:
//!
//! - The app-level `env` block in `oxy-app.json` — what the author says the app
//!   needs. App-level rather than per-function because a secret is app-scoped by
//!   construction (`apps/<app_id>/`), so two functions sharing `STRIPE_API_KEY`
//!   must not be able to declare it twice with conflicting descriptions.
//! - Every function's `webhook.secretVar`, which already names a key. Folding
//!   these in costs the author nothing and closes the bootstrap footgun: a
//!   `webhook:` block whose secret was never set answers 401 to every delivery,
//!   and nothing in the product said which key to fill.
//!
//! Reconciliation is a pure function over (declarations, stored rows) so the
//! interesting cases — declared-but-missing, set-but-undeclared — are unit
//! testable without a database.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// One key an app declares it needs.
///
/// No `value` field, ever. The manifest *names* secrets and never holds one —
/// the same rule `webhook.secretVar` follows, and the reason a bundle can be
/// committed to a public repo.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OxyAppEnvDecl {
    /// Surfaced as **Missing** rather than merely absent when unset.
    ///
    /// Defaults to false: a declaration is documentation first, and defaulting
    /// to required would turn every newly-declared key into an alarm on apps
    /// that are running fine.
    #[serde(default)]
    pub required: bool,
    /// Shown next to the key in the management UI — what it is and where to get
    /// one ("Stripe restricted key, dashboard → Developers → API keys").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Where a declaration came from. Drives how the UI labels a row, and whether
/// "undeclared" is worth flagging at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvSource {
    /// The `env` block in `oxy-app.json`.
    Manifest,
    /// A function's `webhook.secretVar`.
    Webhook,
    /// Stored, but nothing in the active build asks for it. Not an error — a
    /// key written by `ctx.secrets.set` (a refreshed OAuth token) is legitimately
    /// undeclared, and so is one left behind by a build that no longer reads it.
    Undeclared,
}

/// A declaration after merging both sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclaredKey {
    pub key: String,
    pub required: bool,
    pub description: Option<String>,
    pub source: EnvSource,
}

/// A secret that actually exists under `apps/<app_id>/`, with the prefix already
/// stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredSecret {
    pub key: String,
    pub secret_id: Uuid,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub updated_by_email: Option<String>,
}

/// One row in the reconciled view the API returns.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AppSecretEntry {
    /// Bare key (`STRIPE_API_KEY`), never the `apps/<uuid>/` storage name. The
    /// prefix is an implementation detail of where it lives; showing it is what
    /// made the existing settings table unreadable.
    pub key: String,
    /// A value is stored right now.
    pub is_set: bool,
    /// The active build asks for this key.
    pub declared: bool,
    /// Declared with `required: true`. Always false for an undeclared key.
    pub required: bool,
    pub source: EnvSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Only present when `is_set` — the row id, for reveal/delete by id on the
    /// existing project-secrets routes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_by_email: Option<String>,
}

impl AppSecretEntry {
    /// Needs attention: declared, required, and nothing stored.
    pub fn is_missing_required(&self) -> bool {
        self.required && !self.is_set
    }
}

/// Read the app-level `env` block out of a raw `oxy-app.json`.
///
/// **Lenient, like [`retention_policy_from_build_manifest`] and unlike
/// [`migrations_config`].** A malformed `env` block degrades to "declares
/// nothing", which costs a display: the app still runs, `ctx.env` still resolves
/// whatever is stored, and no data moves. Failing the publish instead would
/// break shipping code over a documentation field.
///
/// It is not silent, though — the parse error comes back as the second half of
/// the tuple and is rendered in the UI, so a misspelled block is diagnosable
/// rather than looking like an app that declares nothing.
///
/// [`retention_policy_from_build_manifest`]: super::super::custom_apps_manifest::retention_policy_from_build_manifest
/// [`migrations_config`]: super::super::custom_apps_manifest::migrations_config
pub(crate) fn declared_env(
    manifest_json: Option<&serde_json::Value>,
    app_id: Uuid,
) -> (BTreeMap<String, OxyAppEnvDecl>, Option<String>) {
    let Some(raw) = manifest_json.and_then(|m| m.get("env")) else {
        return (BTreeMap::new(), None);
    };
    // An explicit `null` declares nothing — `JSON.stringify` emits it for an
    // optional field a generator left unset. Same reading as `migrations`.
    if raw.is_null() {
        return (BTreeMap::new(), None);
    }
    match serde_json::from_value::<BTreeMap<String, OxyAppEnvDecl>>(raw.clone()) {
        Ok(map) => (map, None),
        Err(e) => {
            let msg = format!(
                "the `env` block in oxy-app.json is not usable ({e}); it must be an object \
                 keyed by env-var name, e.g. \"env\": {{ \"STRIPE_API_KEY\": {{ \"required\": \
                 true, \"description\": \"…\" }} }}"
            );
            tracing::warn!(%app_id, "oxy-app.json `env`: {msg}");
            (BTreeMap::new(), Some(msg))
        }
    }
}

/// Every key named by a `webhook.secretVar` across a build's function manifests.
///
/// **Comma-split**, because `secretVar` may hold two live signing keys during a
/// provider rotation (Uber's `BASIC_HMAC` issues a pair). Both halves are real
/// keys someone has to set, so both belong in the view.
pub(crate) fn webhook_secret_vars<'a>(
    function_manifests: impl IntoIterator<Item = &'a serde_json::Value>,
) -> BTreeSet<String> {
    function_manifests
        .into_iter()
        .filter_map(|m| m.get("webhook")?.get("secretVar")?.as_str())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Merge both declaration sources. A key in the `env` block keeps its
/// `required` / `description`; a webhook-only key is required by construction —
/// a declared `webhook:` with no resolvable secret is a 401 on every delivery,
/// which is exactly the "must be set" case.
pub(crate) fn merge_declared(
    env: BTreeMap<String, OxyAppEnvDecl>,
    webhook_vars: BTreeSet<String>,
) -> BTreeMap<String, DeclaredKey> {
    let mut out: BTreeMap<String, DeclaredKey> = webhook_vars
        .into_iter()
        .map(|key| {
            (
                key.clone(),
                DeclaredKey {
                    key,
                    required: true,
                    description: Some(
                        "Signing key for this app's webhook. Deliveries are rejected with 401 \
                         until it is set."
                            .to_string(),
                    ),
                    source: EnvSource::Webhook,
                },
            )
        })
        .collect();
    // The explicit block wins: an author who wrote a description for the key
    // said more about it than we can infer from the webhook block.
    for (key, decl) in env {
        out.insert(
            key.clone(),
            DeclaredKey {
                key,
                required: decl.required,
                description: decl.description,
                source: EnvSource::Manifest,
            },
        );
    }
    out
}

/// Reconcile declarations against what is stored.
///
/// The union of both sides, so the three states are all visible: declared and
/// set, declared and **missing**, set but undeclared. Sorted so the rows that
/// need action come first — missing-required, then missing-optional, then the
/// rest — because a fresh deploy's whole question is "what do I still have to
/// fill in".
pub(crate) fn reconcile(
    declared: BTreeMap<String, DeclaredKey>,
    stored: Vec<StoredSecret>,
) -> Vec<AppSecretEntry> {
    let mut stored: BTreeMap<String, StoredSecret> =
        stored.into_iter().map(|s| (s.key.clone(), s)).collect();

    let mut entries: Vec<AppSecretEntry> = declared
        .into_values()
        .map(|d| {
            let row = stored.remove(&d.key);
            AppSecretEntry {
                key: d.key,
                is_set: row.is_some(),
                declared: true,
                required: d.required,
                source: d.source,
                description: d.description,
                secret_id: row.as_ref().map(|r| r.secret_id),
                updated_at: row.as_ref().map(|r| r.updated_at),
                updated_by_email: row.and_then(|r| r.updated_by_email),
            }
        })
        .collect();

    // Whatever is left is stored but unasked-for.
    entries.extend(stored.into_values().map(|row| AppSecretEntry {
        key: row.key,
        is_set: true,
        declared: false,
        required: false,
        source: EnvSource::Undeclared,
        description: None,
        secret_id: Some(row.secret_id),
        updated_at: Some(row.updated_at),
        updated_by_email: row.updated_by_email,
    }));

    entries.sort_by(|a, b| {
        sort_rank(a)
            .cmp(&sort_rank(b))
            .then_with(|| a.key.cmp(&b.key))
    });
    entries
}

/// 0 = missing and required, 1 = missing, 2 = set. Ties break alphabetically.
fn sort_rank(e: &AppSecretEntry) -> u8 {
    match (e.is_set, e.required) {
        (false, true) => 0,
        (false, false) => 1,
        (true, _) => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn app() -> Uuid {
        Uuid::nil()
    }

    fn stored(key: &str) -> StoredSecret {
        StoredSecret {
            key: key.to_string(),
            secret_id: Uuid::nil(),
            updated_at: chrono::Utc::now(),
            updated_by_email: Some("someone@oxy.tech".to_string()),
        }
    }

    #[test]
    fn no_env_block_declares_nothing_and_is_not_an_error() {
        let (map, err) = declared_env(Some(&json!({ "slug": "x" })), app());
        assert!(map.is_empty());
        assert!(err.is_none(), "an absent block is not a misconfiguration");
    }

    #[test]
    fn explicit_null_declares_nothing() {
        let (map, err) = declared_env(Some(&json!({ "env": null })), app());
        assert!(map.is_empty());
        assert!(err.is_none());
    }

    #[test]
    fn parses_required_and_description() {
        let (map, err) = declared_env(
            Some(&json!({ "env": {
                "STRIPE_API_KEY": { "required": true, "description": "refund lookups" },
                "SLACK_WEBHOOK_URL": {},
            }})),
            app(),
        );
        assert!(err.is_none());
        assert!(map["STRIPE_API_KEY"].required);
        assert_eq!(
            map["STRIPE_API_KEY"].description.as_deref(),
            Some("refund lookups")
        );
        assert!(
            !map["SLACK_WEBHOOK_URL"].required,
            "required defaults to false — a declaration is documentation first"
        );
    }

    /// The lenience contract: a malformed block must not take the app's whole
    /// secrets view down with it, but must not be silent either.
    #[test]
    fn malformed_env_degrades_to_nothing_but_reports_why() {
        let (map, err) = declared_env(Some(&json!({ "env": ["STRIPE_API_KEY"] })), app());
        assert!(map.is_empty());
        assert!(
            err.expect("a parse failure must be reported")
                .contains("env"),
            "the message has to name the block so it is actionable"
        );
    }

    #[test]
    fn webhook_secret_vars_are_comma_split_for_rotation() {
        let manifests = vec![
            json!({ "webhook": { "secretVar": "UBER_KEY_A, UBER_KEY_B" } }),
            json!({ "webhook": { "secretVar": "TOAST_KEY" } }),
            json!({ "route": true }),
        ];
        let vars = webhook_secret_vars(manifests.iter());
        assert_eq!(
            vars,
            ["TOAST_KEY", "UBER_KEY_A", "UBER_KEY_B"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        );
    }

    #[test]
    fn webhook_secret_vars_ignores_blanks() {
        let manifests = vec![json!({ "webhook": { "secretVar": "A,, ,B" } })];
        let vars = webhook_secret_vars(manifests.iter());
        assert_eq!(vars.len(), 2, "empty segments are not keys");
    }

    #[test]
    fn a_webhook_key_is_required_by_construction() {
        let merged = merge_declared(BTreeMap::new(), ["SIG".to_string()].into_iter().collect());
        assert!(
            merged["SIG"].required,
            "an unset webhook secret 401s every delivery, so it is not optional"
        );
        assert_eq!(merged["SIG"].source, EnvSource::Webhook);
    }

    #[test]
    fn an_explicit_declaration_wins_over_the_inferred_one() {
        let env = BTreeMap::from([(
            "SIG".to_string(),
            OxyAppEnvDecl {
                required: false,
                description: Some("authored".to_string()),
            },
        )]);
        let merged = merge_declared(env, ["SIG".to_string()].into_iter().collect());
        assert_eq!(merged["SIG"].description.as_deref(), Some("authored"));
        assert_eq!(merged["SIG"].source, EnvSource::Manifest);
        assert!(!merged["SIG"].required, "the author's word is the one kept");
    }

    #[test]
    fn reconcile_covers_all_three_states() {
        let declared = merge_declared(
            BTreeMap::from([
                (
                    "SET_KEY".to_string(),
                    OxyAppEnvDecl {
                        required: true,
                        description: None,
                    },
                ),
                (
                    "MISSING_KEY".to_string(),
                    OxyAppEnvDecl {
                        required: true,
                        description: None,
                    },
                ),
            ]),
            BTreeSet::new(),
        );
        let entries = reconcile(declared, vec![stored("SET_KEY"), stored("LEFTOVER")]);

        let by_key = |k: &str| entries.iter().find(|e| e.key == k).unwrap().clone();

        let set = by_key("SET_KEY");
        assert!(set.is_set && set.declared);

        let missing = by_key("MISSING_KEY");
        assert!(!missing.is_set && missing.declared);
        assert!(missing.is_missing_required());
        assert!(
            missing.secret_id.is_none(),
            "a key with nothing stored has no row to reveal or delete"
        );

        let leftover = by_key("LEFTOVER");
        assert!(leftover.is_set && !leftover.declared);
        assert_eq!(leftover.source, EnvSource::Undeclared);
        assert!(
            !leftover.required,
            "nothing asks for it, so it cannot be missing-required"
        );
    }

    #[test]
    fn missing_required_sorts_first() {
        let declared = merge_declared(
            BTreeMap::from([
                (
                    "AAA_SET".to_string(),
                    OxyAppEnvDecl {
                        required: true,
                        description: None,
                    },
                ),
                (
                    "ZZZ_MISSING".to_string(),
                    OxyAppEnvDecl {
                        required: true,
                        description: None,
                    },
                ),
                (
                    "MMM_OPTIONAL".to_string(),
                    OxyAppEnvDecl {
                        required: false,
                        description: None,
                    },
                ),
            ]),
            BTreeSet::new(),
        );
        let entries = reconcile(declared, vec![stored("AAA_SET")]);
        let order: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(
            order,
            vec!["ZZZ_MISSING", "MMM_OPTIONAL", "AAA_SET"],
            "action first, alphabetical second — a fresh deploy's question is \
             what still needs filling in"
        );
    }

    #[test]
    fn an_app_with_no_declarations_still_lists_what_is_stored() {
        // The pre-existing world: keys written by `ctx.secrets.set`, nothing
        // declared anywhere. The view must not be empty.
        let entries = reconcile(BTreeMap::new(), vec![stored("QB_ACCESS_TOKEN")]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].key, "QB_ACCESS_TOKEN");
        assert!(entries[0].is_set);
    }
}
