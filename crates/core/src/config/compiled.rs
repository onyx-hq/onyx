//! Reads served from the compile boundary, keyed by a revision the caller
//! already holds.
//!
//! These are pure queries: no process role, no branch gate, no ambient pin.
//! Deciding *which* revision a request reads happens once in the app's request
//! middleware and arrives here as `Origin::Compiled { revision_id }` on the
//! manager, so nothing in this module needs to know which pod it runs on.

use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde_json::Value;
use uuid::Uuid;

use super::artifacts::{
    AgentEntry, AppEntry, ArtifactError, AutomationEntry, CompiledArtifact, PipelineEntry,
    SimulationEntry, VerifiedQueryEntry,
};

async fn conn() -> Result<DatabaseConnection, ArtifactError> {
    crate::database::client::establish_connection()
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))
}

/// The workspace's `config.yml` at a revision, reassembled from its columns.
///
/// `Ok(None)` means the revision has no config row — a workspace mid-compile,
/// or one whose compile failed before the config landed. That is a miss, not a
/// fault, and the caller falls through to the working copy.
pub(super) async fn load_config_at(revision_id: Uuid) -> Result<Option<Value>, ArtifactError> {
    Ok(
        entity::workspace_compiled_configs::Entity::find_by_id(revision_id)
            .one(&conn().await?)
            .await
            .map_err(|e| ArtifactError::Backend(e.to_string()))?
            .map(|row| row.merged_config()),
    )
}

pub(super) async fn list_apps_at(
    revision_id: Uuid,
    published_only: bool,
) -> Result<Vec<AppEntry>, ArtifactError> {
    apps_at(&conn().await?, revision_id, published_only).await
}

async fn apps_at(
    db: &DatabaseConnection,
    revision_id: Uuid,
    published_only: bool,
) -> Result<Vec<AppEntry>, ArtifactError> {
    let mut find = entity::app_definitions::Entity::find()
        .filter(entity::app_definitions::Column::RevisionId.eq(revision_id));
    if published_only {
        find = find.filter(entity::app_definitions::Column::Published.eq(true));
    }
    Ok(find
        .all(db)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| AppEntry {
            title: extract_title(&m.definition),
            file_path: m.file_path,
            name: m.name,
            published: m.published,
        })
        .collect())
}

/// `title` out of the JSONB without a full struct deserialize. The strict parse
/// already happened at compile time; a listing only wants the label.
fn extract_title(definition: &Value) -> Option<String> {
    extract_str(definition, "title")
}

/// One airway pipeline's compiled `definition`, by workspace-relative path.
pub(super) async fn resolve_pipeline_at(
    revision_id: Uuid,
    file_path: &str,
) -> Result<Option<serde_json::Value>, ArtifactError> {
    Ok(entity::airway_pipelines::Entity::find()
        .filter(entity::airway_pipelines::Column::RevisionId.eq(revision_id))
        .filter(entity::airway_pipelines::Column::FilePath.eq(file_path))
        .one(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .map(|m| m.definition))
}

pub(super) async fn list_pipelines_at(
    revision_id: Uuid,
) -> Result<Vec<PipelineEntry>, ArtifactError> {
    Ok(entity::airway_pipelines::Entity::find()
        .filter(entity::airway_pipelines::Column::RevisionId.eq(revision_id))
        .all(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| PipelineEntry {
            source_kind: crate::config::artifacts::pipeline_source_kind(&m.definition),
            name: m.name,
            file_path: m.file_path,
        })
        .collect())
}

pub(super) async fn list_agents_at(revision_id: Uuid) -> Result<Vec<AgentEntry>, ArtifactError> {
    Ok(entity::agent_definitions::Entity::find()
        .filter(entity::agent_definitions::Column::RevisionId.eq(revision_id))
        .all(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| AgentEntry {
            model_ref: extract_model_ref(&m.definition),
            timezone: extract_str(&m.definition, "timezone"),
            file_path: m.file_path,
            name: m.name,
        })
        .collect())
}

fn extract_model_ref(definition: &Value) -> Option<String> {
    definition
        .as_object()?
        .get("llm")?
        .as_object()?
        .get("ref")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn extract_str(definition: &Value, key: &str) -> Option<String> {
    definition
        .as_object()?
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

pub(super) async fn list_automations_at(
    revision_id: Uuid,
) -> Result<Vec<AutomationEntry>, ArtifactError> {
    Ok(entity::automation_definitions::Entity::find()
        .filter(entity::automation_definitions::Column::RevisionId.eq(revision_id))
        .all(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| AutomationEntry {
            name: m.name,
            file_path: m.file_path,
            extension: m.extension,
        })
        .collect())
}

/// One app's compiled `definition`, keyed by workspace-relative path.
///
/// Path rather than name: the UI addresses apps that way (`/apps/<pathb64>`)
/// and a workspace can carry duplicate `name`s in different folders.
pub(super) async fn resolve_app_at(
    revision_id: Uuid,
    file_path: &str,
) -> Result<Option<Value>, ArtifactError> {
    Ok(
        entity::app_definitions::Entity::find_by_id((revision_id, file_path.to_string()))
            .one(&conn().await?)
            .await
            .map_err(|e| ArtifactError::Backend(e.to_string()))?
            .map(|m| m.definition),
    )
}

/// One automation's compiled `definition`, keyed by workspace-relative path.
pub(super) async fn resolve_automation_at(
    revision_id: Uuid,
    file_path: &str,
) -> Result<Option<Value>, ArtifactError> {
    Ok(
        entity::automation_definitions::Entity::find_by_id((revision_id, file_path.to_string()))
            .one(&conn().await?)
            .await
            .map_err(|e| ArtifactError::Backend(e.to_string()))?
            .map(|m| m.definition),
    )
}

/// One verified query's compiled SQL body, keyed by `file_path`.
///
/// Returns the `content` column rather than a `definition`: a `.sql` file has
/// no JSONB shape, its body *is* the artifact. Otherwise identical to
/// [`resolve_automation_at`].
///
/// Exists because `execute_sql`'s `sql_file` used to read the workspace
/// filesystem directly, which is fine on the `ide` singleton and fails on any
/// node without the working copy. That became reachable when
/// `oxy-hq/oxygen-internal#3056` moved airway pipelines onto the worker fleet:
/// an `airway ingest -> execute_sql rollup` chain — the standard medallion
/// gold-layer shape — failed with ENOENT on a pod that has no `/workspace`.
/// `list_verified_queries_at` below already served the whole set to the
/// analytics scan; this is the by-path lookup that read path needed.
pub(super) async fn resolve_verified_query_at(
    revision_id: Uuid,
    file_path: &str,
) -> Result<Option<String>, ArtifactError> {
    Ok(
        entity::verified_queries::Entity::find_by_id((revision_id, file_path.to_string()))
            .one(&conn().await?)
            .await
            .map_err(|e| ArtifactError::Backend(e.to_string()))?
            .map(|m| m.content),
    )
}

/// One analytics agent's compiled `definition`, keyed by `name`.
///
/// Name rather than path, because the analytics pipeline references agents by
/// `AgenticAgent.name`.
pub(super) async fn resolve_agent_at(
    revision_id: Uuid,
    name: &str,
) -> Result<Option<Value>, ArtifactError> {
    Ok(entity::agent_definitions::Entity::find()
        .filter(entity::agent_definitions::Column::RevisionId.eq(revision_id))
        .filter(entity::agent_definitions::Column::Name.eq(name))
        .one(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .map(|m| m.definition))
}

/// The three root singletons: one row per revision, keyed by revision alone,
/// whose `definition` is the whole file ready to round-trip back into its
/// strict type.
pub(super) async fn resolve_monitor_config_at(
    revision_id: Uuid,
) -> Result<Option<Value>, ArtifactError> {
    Ok(entity::monitor_configs::Entity::find_by_id(revision_id)
        .one(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .map(|m| m.definition))
}

pub(super) async fn resolve_reconcile_config_at(
    revision_id: Uuid,
) -> Result<Option<Value>, ArtifactError> {
    Ok(entity::reconcile_configs::Entity::find_by_id(revision_id)
        .one(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .map(|m| m.definition))
}

pub(super) async fn resolve_world_model_config_at(
    revision_id: Uuid,
) -> Result<Option<Value>, ArtifactError> {
    Ok(entity::world_model_configs::Entity::find_by_id(revision_id)
        .one(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .map(|m| m.definition))
}

pub(super) async fn list_semantic_views_at(
    revision_id: Uuid,
) -> Result<Vec<CompiledArtifact>, ArtifactError> {
    Ok(entity::semantic_views::Entity::find()
        .filter(entity::semantic_views::Column::RevisionId.eq(revision_id))
        .all(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| CompiledArtifact {
            name: m.name,
            file_path: m.file_path,
            definition: m.definition,
            blob_key: m.compiled_sql_blob_key,
        })
        .collect())
}

pub(super) async fn list_semantic_topics_at(
    revision_id: Uuid,
) -> Result<Vec<CompiledArtifact>, ArtifactError> {
    Ok(entity::semantic_topics::Entity::find()
        .filter(entity::semantic_topics::Column::RevisionId.eq(revision_id))
        .all(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| CompiledArtifact {
            name: m.name,
            file_path: m.file_path,
            definition: m.definition,
            blob_key: m.compiled_sql_blob_key,
        })
        .collect())
}

pub(super) async fn list_automation_artifacts_at(
    revision_id: Uuid,
) -> Result<Vec<CompiledArtifact>, ArtifactError> {
    Ok(entity::automation_definitions::Entity::find()
        .filter(entity::automation_definitions::Column::RevisionId.eq(revision_id))
        .all(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| CompiledArtifact {
            name: m.name,
            file_path: m.file_path,
            definition: m.definition,
            blob_key: None,
        })
        .collect())
}

/// Every declared world (`simulation_definitions`) at a revision.
///
/// A workspace carries a *grid* of these, so listing is the primary access
/// pattern — the runs surface enumerates what can be run before anything is.
pub(super) async fn list_simulations_at(
    revision_id: Uuid,
) -> Result<Vec<SimulationEntry>, ArtifactError> {
    Ok(entity::simulation_definitions::Entity::find()
        .filter(entity::simulation_definitions::Column::RevisionId.eq(revision_id))
        .all(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| SimulationEntry {
            name: m.name,
            file_path: m.file_path,
            definition: m.definition,
        })
        .collect())
}

/// One declared world's compiled `definition`, keyed by `name`.
///
/// Name rather than path — a run references the world it is running, and that
/// reference has to survive the file being moved.
pub(super) async fn resolve_simulation_at(
    revision_id: Uuid,
    name: &str,
) -> Result<Option<Value>, ArtifactError> {
    Ok(
        entity::simulation_definitions::Entity::find_by_id((revision_id, name.to_string()))
            .one(&conn().await?)
            .await
            .map_err(|e| ArtifactError::Backend(e.to_string()))?
            .map(|m| m.definition),
    )
}

pub(super) async fn list_verified_queries_at(
    revision_id: Uuid,
) -> Result<Vec<VerifiedQueryEntry>, ArtifactError> {
    Ok(entity::verified_queries::Entity::find()
        .filter(entity::verified_queries::Column::RevisionId.eq(revision_id))
        .all(&conn().await?)
        .await
        .map_err(|e| ArtifactError::Backend(e.to_string()))?
        .into_iter()
        .map(|m| VerifiedQueryEntry {
            file_path: m.file_path,
            content_sha256: m.content_sha256,
            content: m.content,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `llm.ref` is nested, so a caller reaching for it must not trip over an
    /// agent that declares no `llm:` block at all — the listing still has to
    /// render, with no model rather than no agent.
    #[test]
    fn model_ref_is_read_from_the_nested_llm_block() {
        let with_llm = serde_json::json!({ "llm": { "ref": "gpt-4o" } });
        assert_eq!(extract_model_ref(&with_llm).as_deref(), Some("gpt-4o"));

        for absent in [
            serde_json::json!({}),
            serde_json::json!({ "llm": {} }),
            serde_json::json!({ "llm": "not-an-object" }),
            serde_json::json!("not-an-object"),
        ] {
            assert_eq!(extract_model_ref(&absent), None, "{absent}");
        }
    }
}
