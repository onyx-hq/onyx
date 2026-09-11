//! Copy LLM provider keys from the environment into each seeded workspace's
//! secrets store, so chat works on a seeded box.
//!
//! Cloud mode — which is what local dev runs — resolves a model's `key_var`
//! from the **workspace secrets store**, never the process env (see
//! product-context.md, "Mode-dependent LLM-key check"). A developer with
//! `OPENAI_API_KEY` exported would otherwise get a seeded workspace whose first
//! chat message fails on a missing key.
//!
//! Names come from the workspace's own `config.yml` (`models[].key_var`), so
//! only keys the workspace will actually ask for are copied. Never overwrites
//! an existing secret: one set through the panel is the developer's choice.
//! Stricter than the tenant seed's guard: this writes the developer's real
//! credentials, so it runs only when `OXY_DATABASE_URL` itself looks local.
//! `OXY_SEED_ALLOW_REMOTE` consents to demo users and orgs on another database
//! — it does not carry anyone's API keys there with them.

use std::collections::BTreeSet;
use std::path::Path;

use entity::org_members::{self, OrgRole};
use entity::prelude::{OrgMembers, Secrets};
use entity::secrets;
use oxy::service::secret_manager::{CreateSecretParams, SecretManagerService};
use oxy::theme::StyledText;
use oxy_shared::errors::OxyError;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder};
use uuid::Uuid;

use super::seed_compile::SeededWorkspace;

const DESCRIPTION: &str = "Copied from the environment by `oxy seed`";

/// What happened, by secret NAME only — values never reach the output.
#[derive(Default)]
struct Report {
    stored: BTreeSet<String>,
    workspaces_written: usize,
    already_set: BTreeSet<String>,
    not_in_env: BTreeSet<String>,
    no_owner: Vec<String>,
    failed: Vec<String>,
}

pub(crate) async fn store_llm_keys(
    conn: &DatabaseConnection,
    targets: &[SeededWorkspace],
) -> Result<(), OxyError> {
    let allow_remote = super::seed_partners::remote_seeding_allowed();
    if !keys_may_be_written(
        allow_remote,
        super::seed_partners::database_url_looks_local(),
    ) {
        println!(
            "{} skipping LLM keys — not an unambiguously local database{}",
            "⏭️".info(),
            if allow_remote {
                " (OXY_SEED_ALLOW_REMOTE covers demo rows, not credentials)"
            } else {
                ""
            }
        );
        return Ok(());
    }
    let mut report = Report::default();
    for target in targets {
        store_for_workspace(conn, target, &mut report).await?;
    }
    report.print();
    Ok(())
}

/// Real credentials go only to a database that is local by its URL, and never
/// under `OXY_SEED_ALLOW_REMOTE`: that hatch is consent to demo rows, and an
/// engineer who gave it for users and orgs did not also agree to ship keys.
fn keys_may_be_written(allow_remote: bool, url_looks_local: bool) -> bool {
    !allow_remote && url_looks_local
}

async fn store_for_workspace(
    conn: &DatabaseConnection,
    target: &SeededWorkspace,
    report: &mut Report,
) -> Result<(), OxyError> {
    let names = referenced_key_vars(target.path.as_deref());
    let present = keys_in_env(&names, |name| std::env::var(name).ok());
    report.not_in_env.extend(
        names
            .iter()
            .filter(|name| !present.iter().any(|(n, _)| n == *name))
            .cloned(),
    );
    if present.is_empty() {
        return Ok(());
    }
    // `secrets.created_by` is a foreign key to a real user.
    let Some(owner) = org_owner(conn, target.org_id).await? else {
        report.no_owner.push(target.label.clone());
        return Ok(());
    };

    let store = SecretManagerService::new(target.id);
    let mut wrote = false;
    for (name, value) in present {
        if secret_exists(conn, target.id, &name).await? {
            report.already_set.insert(name);
            continue;
        }
        let params = CreateSecretParams {
            name: name.clone(),
            value,
            description: Some(DESCRIPTION.to_string()),
            created_by: owner,
        };
        match store.create_secret(conn, params).await {
            Ok(_) => {
                wrote = true;
                report.stored.insert(name);
            }
            Err(e) => report
                .failed
                .push(format!("{name} on {}: {e}", target.label)),
        }
    }
    report.workspaces_written += usize::from(wrote);
    Ok(())
}

impl Report {
    fn print(&self) {
        if !self.stored.is_empty() {
            println!(
                "{} stored LLM key{} as workspace secrets: {} (on {} workspace{})",
                "🔑".info(),
                if self.stored.len() == 1 { "" } else { "s" },
                join(&self.stored),
                self.workspaces_written,
                if self.workspaces_written == 1 {
                    ""
                } else {
                    "s"
                }
            );
        }
        if !self.already_set.is_empty() {
            println!("   already set, left alone: {}", join(&self.already_set));
        }
        if !self.not_in_env.is_empty() {
            println!(
                "   referenced by config.yml but not in the environment: {}",
                join(&self.not_in_env)
            );
        }
        if !self.no_owner.is_empty() {
            println!(
                "{} no org owner to attribute keys to, skipped: {}",
                "⚠️".warning(),
                self.no_owner.join(", ")
            );
        }
        for failure in &self.failed {
            println!("{} LLM key not stored — {failure}", "⚠️".warning());
        }
    }
}

fn join(names: &BTreeSet<String>) -> String {
    names.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// `models[].key_var` names in the working copy's `config.yml`. A missing or
/// unreadable config yields nothing — the compile step reports broken files.
fn referenced_key_vars(workspace: Option<&Path>) -> BTreeSet<String> {
    workspace
        .and_then(|dir| std::fs::read_to_string(dir.join("config.yml")).ok())
        .map(|yaml| key_vars_in_config(&yaml))
        .unwrap_or_default()
}

fn key_vars_in_config(yaml: &str) -> BTreeSet<String> {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return BTreeSet::new();
    };
    doc.get("models")
        .and_then(serde_yaml::Value::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|model| model.get("key_var")?.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// The referenced names that `lookup` has a non-blank value for.
fn keys_in_env(
    names: &BTreeSet<String>,
    lookup: impl Fn(&str) -> Option<String>,
) -> Vec<(String, String)> {
    names
        .iter()
        .filter_map(|name| {
            let value = lookup(name)?;
            (!value.trim().is_empty()).then(|| (name.clone(), value))
        })
        .collect()
}

async fn org_owner(
    conn: &DatabaseConnection,
    org_id: Option<Uuid>,
) -> Result<Option<Uuid>, OxyError> {
    let Some(org_id) = org_id else {
        return Ok(None);
    };
    let owner = OrgMembers::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .filter(org_members::Column::Role.eq(OrgRole::Owner))
        .order_by_asc(org_members::Column::CreatedAt)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query owner of org {org_id}: {e}")))?;
    Ok(owner.map(|m| m.user_id))
}

async fn secret_exists(
    conn: &DatabaseConnection,
    workspace_id: Uuid,
    name: &str,
) -> Result<bool, OxyError> {
    let existing = Secrets::find()
        .filter(secrets::Column::ProjectId.eq(workspace_id))
        .filter(secrets::Column::Name.eq(name))
        .filter(secrets::Column::IsActive.eq(true))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query secret {name}: {e}")))?;
    Ok(existing.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_stay_home_unless_the_url_itself_is_local() {
        assert!(keys_may_be_written(false, true));
        assert!(!keys_may_be_written(false, false));
        // The escape hatch lets demo rows reach a remote DB; keys never ride it,
        // even when the URL happens to look local.
        assert!(!keys_may_be_written(true, false));
        assert!(!keys_may_be_written(true, true));
    }

    #[test]
    fn collects_model_key_vars_once_each() {
        let yaml = r#"
databases:
  - name: warehouse
    key_var: NOT_A_MODEL_KEY
models:
  - name: a
    vendor: openai
    key_var: OPENAI_API_KEY
  - name: b
    vendor: openai
    key_var: " OPENAI_API_KEY "
  - name: c
    vendor: anthropic
    key_var: ANTHROPIC_API_KEY
  - name: local
    vendor: ollama
    api_key: secret
"#;
        let names: Vec<_> = key_vars_in_config(yaml).into_iter().collect();
        assert_eq!(names, vec!["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]);
    }

    #[test]
    fn unparseable_or_modelless_config_yields_nothing() {
        assert!(key_vars_in_config("models: [").is_empty());
        assert!(key_vars_in_config("defaults:\n  agent: x\n").is_empty());
        assert!(referenced_key_vars(None).is_empty());
    }

    #[test]
    fn only_names_with_a_non_blank_env_value_are_copied() {
        let names: BTreeSet<String> = ["A_KEY", "BLANK_KEY", "UNSET_KEY"]
            .into_iter()
            .map(String::from)
            .collect();
        let present = keys_in_env(&names, |name| match name {
            "A_KEY" => Some("sk-123".to_string()),
            "BLANK_KEY" => Some("   ".to_string()),
            _ => None,
        });
        assert_eq!(present, vec![("A_KEY".to_string(), "sk-123".to_string())]);
    }

    #[test]
    fn the_demo_config_references_the_provider_keys() {
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let names = referenced_key_vars(Some(&examples));
        assert!(names.contains("OPENAI_API_KEY"), "{names:?}");
        assert!(names.contains("ANTHROPIC_API_KEY"), "{names:?}");
    }
}
