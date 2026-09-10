//! [`WorkspaceContext`] implementation for [`OxyProjectContext`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use agentic_automation::workspace::IntegrationConfig;
use agentic_automation::{ContextRoot, WorkspaceContext, WorkspaceReadError};
use agentic_connector::DatabaseConnector;
use async_trait::async_trait;
use oxy::config::model::IntegrationType;

use super::{OxyProjectContext, resolve_workspace_relative};

#[async_trait]
impl WorkspaceContext for OxyProjectContext {
    /// `Some` only when this process owns the workspace files. The manager
    /// carries the capability, so this is the manager's answer, not a probe.
    ///
    /// It is NOT a test for "this node holds the files". `OxyProjectContext`
    /// holds a `WorkspaceManager<WorkingCopy>`, whose slot is always full, and
    /// `effective_workspace_path` hands back the database column without
    /// stat-ing it — so this is `Some` on a replica too, naming a directory
    /// that is not there. `ConfigManager::disk()` is what turns that into an
    /// error; anything branching on presence-of-files must ask the role
    /// (`context_root` below) or the filesystem, not this.
    fn workspace_path(&self) -> Option<&Path> {
        self.workspace_manager
            .config_manager
            .working_copy()
            .map(|wc| wc.root())
    }

    /// On the stateless fleet roles (`Serve` and `Worker`) there's no working
    /// copy, so resolve an agent's `context:` globs from the compile boundary
    /// (materialised into a tempdir) instead of globbing an absent filesystem —
    /// otherwise context resolution finds nothing and the run fails "no
    /// databases configured". `Ide` / `All` keep the FS path (they hold the
    /// working copy), so context the boundary doesn't serve yet (verified
    /// `.sql`, `.md`, automations) isn't lost there.
    async fn context_root(&self) -> ContextRoot {
        use crate::server::role_manifest::{Role, current_process_role};
        if matches!(current_process_role(), Role::Serve | Role::Worker) {
            let workspace_id = self.workspace_manager.workspace_id;
            match crate::server::api::semantic_scan::materialise_agent_context(
                &self.workspace_manager.config_manager,
            )
            .await
            {
                Ok(Some(materialised)) => {
                    let root = materialised.root.clone();
                    // The revision travels with the root because anything a
                    // caller caches off this path must be keyed by it. A
                    // materialised root is revision-shaped, not working-copy
                    // shaped, and caching an engine built from revision A under
                    // a working-copy key would serve it for revision B — a
                    // promote that appears not to take effect.
                    //
                    // `materialise_agent_context` only returns `Some` under
                    // `Origin::Compiled`, so this is always `Some` here; the
                    // `unwrap_or_else` is a belt-and-braces that degrades to the
                    // working-copy key rather than panicking.
                    let revision_id = self
                        .workspace_manager
                        .config_manager
                        .revision_id()
                        .unwrap_or_else(uuid::Uuid::nil);
                    return ContextRoot::materialised(root, Box::new(materialised), revision_id);
                }
                // On a stateless node the FS fall-through below is doomed (no
                // working copy), so make the miss LOUD and specific rather than
                // letting it surface later as a confusing "no databases
                // configured". Control flow is unchanged — we still fall
                // through, which stays correct for Ide/All and any edge case.
                //
                // These two arms give an operator OPPOSITE instructions, which
                // is why it matters that they are now actually distinct. Until
                // the reads stopped swallowing their errors, a Postgres fault
                // arrived here as `Ok(None)` and told someone to recompile a
                // workspace that was compiled fine — while the arm below, named
                // for exactly that fault, was unreachable.
                Ok(None) => {
                    tracing::error!(
                        workspace_id = %workspace_id,
                        role = ?current_process_role(),
                        "context_root: no compiled agent context on a stateless node — workspace not promoted / not yet compiled; the run will have no usable context. Recompile the workspace."
                    );
                }
                Err(e) => {
                    tracing::error!(
                        workspace_id = %workspace_id,
                        error = ?e,
                        retryable = e.retryable(),
                        "context_root: could not read the compile boundary on a stateless node; the run will fall through to an absent FS and fail. The workspace may be fine — retry before recompiling."
                    );
                }
            }
        }
        // Ide / All, or a stateless node whose boundary read failed above. The
        // second is already logged as an error; an empty root there resolves no
        // globs, which is the same outcome as the absent directory it replaced.
        ContextRoot::fs(
            self.workspace_path()
                .map(|p| p.to_path_buf())
                .unwrap_or_default(),
        )
    }

    fn refresh_key_cache(
        &self,
    ) -> Option<Arc<RwLock<agentic_semantic::refresh_key_cache::RefreshKeyCache>>> {
        self.preagg_cache.clone()
    }

    fn preagg_renewal_threshold_secs(&self) -> u64 {
        // The trait wants a number; `None` here means nobody set one, which is
        // the trait's own documented 120s default.
        self.preagg_renewal_threshold_secs
            .unwrap_or(oxy::config::preagg_check::DEFAULT_RENEWAL_SECS)
    }

    fn preagg_workspace_id(&self) -> Option<uuid::Uuid> {
        Some(self.workspace_manager().workspace_id)
    }

    fn semantic_engine_cache(
        &self,
    ) -> Option<(Arc<oxy_airlayer_compat::SemanticEngineCache>, uuid::Uuid)> {
        let cache = self.semantic_engine_cache.clone()?;
        Some((cache, self.workspace_manager().workspace_id))
    }

    fn preagg_blob(&self) -> Option<agentic_semantic::BlobConfig> {
        crate::server::preagg_context::blob_config()
    }

    fn database_configs(&self) -> Vec<oxy_airlayer_compat::DatabaseConfig> {
        self.workspace_manager
            .config_manager
            .list_databases()
            .iter()
            .map(|db| oxy_airlayer_compat::database_config(db.name.clone(), db.dialect()))
            .collect()
    }

    async fn get_connector(&self, name: &str) -> Result<Arc<dyn DatabaseConnector>, String> {
        self.build_connector_lazy(name).await
    }

    // Forward the `http_request` task's secret read/write to the real secret
    // manager (the `WorkspaceContext` trait defaults to None/Err). Mirrors the
    // pipeline `ProjectContext::resolve_secret`/`persist_secret` impls above so a
    // workflow and a pipeline resolve and rotate the same secret store. (Named
    // `fetch_secret`/`store_secret` to avoid colliding with that trait.)
    async fn fetch_secret(&self, var_name: &str) -> Option<String> {
        match self
            .workspace_manager
            .secrets_manager
            .resolve_secret(var_name)
            .await
        {
            Ok(Some(v)) => return Some(v),
            Ok(None) => {}
            Err(e) => tracing::warn!(
                key_var = %var_name,
                error = %e,
                "secrets_manager.resolve_secret failed; falling back to std::env::var"
            ),
        }
        std::env::var(var_name).ok()
    }

    async fn store_secret(&self, var_name: &str, value: &str) -> Result<(), String> {
        self.workspace_manager
            .secrets_manager
            .upsert_secret(
                var_name,
                value,
                self.subject.unwrap_or_else(uuid::Uuid::nil),
            )
            .await
            .map_err(|e| format!("persist secret `{var_name}`: {e}"))
    }

    async fn get_integration(&self, name: &str) -> Result<IntegrationConfig, String> {
        let integration = self
            .workspace_manager
            .config_manager
            .get_integration_by_name(name)
            .ok_or_else(|| format!("integration '{name}' not found"))?;

        match &integration.integration_type {
            IntegrationType::Omni(omni_cfg) => {
                let api_key = self
                    .workspace_manager
                    .secrets_manager
                    .resolve_secret(&omni_cfg.api_key_var)
                    .await
                    .map_err(|e| format!("failed to resolve omni api_key: {e}"))?
                    .ok_or_else(|| {
                        format!("omni api_key_var '{}' not found", omni_cfg.api_key_var)
                    })?;
                Ok(IntegrationConfig::Omni {
                    base_url: omni_cfg.base_url.clone(),
                    api_key,
                })
            }
            IntegrationType::Looker(looker_cfg) => {
                let client_id = self
                    .workspace_manager
                    .secrets_manager
                    .resolve_secret(&looker_cfg.client_id_var)
                    .await
                    .map_err(|e| format!("failed to resolve looker client_id: {e}"))?
                    .ok_or_else(|| {
                        format!(
                            "looker client_id_var '{}' not found",
                            looker_cfg.client_id_var
                        )
                    })?;
                let client_secret = self
                    .workspace_manager
                    .secrets_manager
                    .resolve_secret(&looker_cfg.client_secret_var)
                    .await
                    .map_err(|e| format!("failed to resolve looker client_secret: {e}"))?
                    .ok_or_else(|| {
                        format!(
                            "looker client_secret_var '{}' not found",
                            looker_cfg.client_secret_var
                        )
                    })?;
                Ok(IntegrationConfig::Looker {
                    base_url: looker_cfg.base_url.clone(),
                    client_id,
                    client_secret,
                })
            }
            // World-model "Apps" integrations (Toast, OpenWeatherMap, BestTime,
            // UniFi) are pure HTTP integrations consumed by the world-model
            // dashboard. They don't participate in the agentic pipeline, so
            // there's no corresponding `IntegrationConfig` variant — surface a
            // clear error if a pipeline ever asks for one by name.
            IntegrationType::Toast(_)
            | IntegrationType::ToastAnalytics(_)
            | IntegrationType::OpenWeatherMap(_)
            | IntegrationType::BestTime(_)
            | IntegrationType::Unifi(_) => Err(format!(
                "integration '{name}' is a world-model app integration and is not exposed to the agentic pipeline"
            )),
        }
    }

    async fn list_automation_files(&self) -> Result<Vec<PathBuf>, String> {
        // Boundary-first (mirrors `resolve_automation_yaml` just below): a stateless
        // fleet replica has no working copy, so list runnable automations from
        // `procedure_definitions` instead of globbing an absent filesystem —
        // otherwise `/agentic-workflows/files` returns `[]` and the automations
        // sidebar is empty. Fall through to the FS walk on a miss (workspace not
        // promoted / non-default branch).
        //
        // Workspace-RELATIVE, like `list_airway_files` below. Both consumers
        // relativise anyway and both tolerate a path that already is:
        // `make_workspace_relative` returns early on `is_relative()`
        // (`automation/src/runner.rs`), and the HTTP lister does
        // `strip_prefix(root).unwrap_or(&abs)`, which passes a relative path
        // through unchanged. The runner's own comment calls making it relative
        // "the correct fix" — the downstream contract rejects absolute
        // `workflow_ref`s as a `..`-traversal guard, so every caller was
        // undoing this join.
        //
        // Joining the root also made this the only reason the method needed a
        // workspace root at all, on a context whose compiled arm has none.
        Ok(self
            .workspace_manager
            .config_manager
            .list_automations()
            .await
            .map_err(|e| format!("{e}"))?
            .into_iter()
            .map(|a| PathBuf::from(a.file_path))
            .collect())
    }

    async fn list_airway_files(&self) -> Result<Vec<PathBuf>, String> {
        // Workspace-relative. The one consumer strips the root with
        // `strip_prefix(..).unwrap_or(&abs)`, so a relative path passes through
        // unchanged — and relative is what the compiled arm carries.
        Ok(self
            .workspace_manager
            .config_manager
            .list_pipelines()
            .await
            .map_err(|e| format!("{e}"))?
            .into_iter()
            .map(|p| PathBuf::from(p.file_path))
            .collect())
    }

    /// Serve a `.airway.yml` body from `airway_pipelines`. `Ok(None)` = "read
    /// the FS", which the caller (`pipeline_ref::load_pipeline_yaml`) then does
    /// under its containment guard; `Err` = "I could not look", which the
    /// caller turns into a retryable `Unavailable` instead of an FS read.
    ///
    /// That last distinction is why this returns a `Result`. Reporting a
    /// lookup error as `Ok(None)` sent the caller to a filesystem that does not
    /// exist on a stateless replica, where the failure resurfaced as
    /// `Invalid("workspace root is not accessible")` — a caller-input shape for
    /// a database blip, and terminal on the automation dispatch path that
    /// retries only transient errors.
    ///
    /// This is what makes an airway run executable on the durable worker
    /// fleet: the executor claims a queued `TaskSpec::Airway` on a stateless
    /// replica with no working copy, so an FS read there is the
    /// instance-affinity failure the compile boundary exists to close.
    ///
    /// The branch carve-out is the manager's: `resolve_request_revision`
    /// yields no revision for a draft branch on a node that owns files, so the
    /// manager reads `Origin::Disk` there and the IDE's edit-then-run loop is
    /// unchanged.
    async fn resolve_pipeline_yaml(&self, pipeline_ref: &str) -> Result<Option<String>, String> {
        // `ConfigManager` owns the compiled-vs-disk choice, and the middleware
        // already pinned this request's revision — including the branch carve
        // out that routes a draft branch back to the working copy, which the
        // `branch` lookup below used to re-derive by hand.
        match self
            .workspace_manager
            .config_manager
            .pipeline_definition(pipeline_ref)
            .await
        {
            // Round-trip JSONB → YAML so the downstream parser (which renders
            // `variables` with minijinja and then deserialises) is unchanged.
            Ok(Some(definition)) => match serde_yaml::to_string(&definition) {
                Ok(yaml) => {
                    tracing::debug!(
                        workspace_id = %self.workspace_manager.workspace_id,
                        pipeline_ref,
                        "resolve_pipeline_yaml served from compile boundary"
                    );
                    Ok(Some(yaml))
                }
                // `Ok(None)`, not `Err`: the row was found and is unusable, so
                // this is a content problem with that revision rather than a
                // question we failed to ask. A host with a working copy has a
                // legitimately better answer, and on a host without one the
                // caller's own no-working-copy branch reports it as
                // `Unavailable` anyway.
                Err(e) => {
                    tracing::warn!(
                        workspace_id = %self.workspace_manager.workspace_id,
                        pipeline_ref,
                        error = ?e,
                        "compile boundary pipeline YAML re-serialise failed; falling through to FS"
                    );
                    Ok(None)
                }
            },
            // Draft branch, workspace not promoted, local workspace, or no
            // matching row — fall through to FS.
            Ok(None) => Ok(None),
            // The lookup itself failed. Propagated rather than laundered into
            // `Ok(None)`: this is "unknown", and the caller must not read it as
            // "not compiled here" and go to a filesystem this node may not have.
            Err(e) => {
                tracing::warn!(
                    workspace_id = %self.workspace_manager.workspace_id,
                    pipeline_ref,
                    error = ?e,
                    "compile boundary pipeline lookup error; reporting unavailable"
                );
                Err(e.to_string())
            }
        }
    }

    /// Serve `execute_sql`'s `sql_file` from the compile boundary.
    ///
    /// The reason this exists is a live failure, not tidiness: with airway
    /// pipelines moved onto the worker fleet (#3056), an `airway ingest ->
    /// execute_sql rollup` chain ran on a pod with no `/workspace` volume and
    /// died on `failed to read SQL file …: No such file or directory`. The
    /// pipeline's own definition already came from Postgres; the `.sql` it
    /// delegated to did not.
    ///
    /// `.sql` files were already compiled — `oxy_compile`'s `VerifiedQuery`
    /// kind walks them and the writer stores the body — so this is the missing
    /// by-path read, not a new entity type.
    async fn resolve_sql_file(&self, sql_ref: &str) -> Result<Option<String>, String> {
        match self
            .workspace_manager
            .config_manager
            .verified_query_content(sql_ref)
            .await
        {
            Ok(Some(sql)) => {
                tracing::debug!(
                    workspace_id = %self.workspace_manager.workspace_id,
                    sql_ref,
                    "resolve_sql_file served from compile boundary"
                );
                Ok(Some(sql))
            }
            // Draft branch, not promoted, local workspace, or simply not a
            // compiled path — the caller may read the working copy.
            Ok(None) => Ok(None),
            // Propagated rather than laundered into `Ok(None)`: this is
            // "unknown", and the caller must not read it as "not compiled here"
            // and go to a filesystem this node may not have.
            Err(e) => {
                tracing::warn!(
                    workspace_id = %self.workspace_manager.workspace_id,
                    sql_ref,
                    error = ?e,
                    "compile boundary lookup failed for sql_file"
                );
                Err(format!(
                    "compile boundary unavailable for sql_file `{sql_ref}`: {e}"
                ))
            }
        }
    }

    async fn resolve_automation_yaml(
        &self,
        workflow_ref: &str,
    ) -> Result<String, WorkspaceReadError> {
        // Serve the automation YAML from `procedure_definitions`; falls through
        // to the filesystem read below on any miss. Round-trips JSONB →
        // strict-typed Workflow → YAML so the downstream parser (which expects
        // YAML) is unchanged.
        match self.compiled_automation(workflow_ref).await {
            Ok(Some(definition)) => match serde_yaml::to_string(&definition) {
                Ok(yaml) => return Ok(yaml),
                // Row found, contents unusable: a content problem with that
                // revision, not a question we failed to ask. A host with a
                // working copy has a better answer, so fall through.
                Err(e) => tracing::warn!(
                    workspace_id = %self.workspace_manager.workspace_id,
                    workflow_ref,
                    error = ?e,
                    "compile boundary automation re-serialise failed; falling through to FS"
                ),
            },
            // Draft branch, workspace not promoted, or no matching row.
            Ok(None) => {}
            // The lookup itself failed. Propagated rather than laundered into
            // a miss — same rule `resolve_pipeline_yaml` above already states:
            // this is "unknown", and the caller must not read it as "not
            // compiled here" and go to a filesystem this node may not have.
            // Labelled by the SHAPE of the failure, not by where the call sat.
            // An earlier version called every error here `Unavailable` on the
            // stated grounds that `definition` only fails without a working
            // copy — which is false. It also propagates a `definition_from_disk`
            // error on `Origin::Disk`, on the `Ok(None)` arm for any variant it
            // does not fold into a miss, and on the compiled-`Err`-with-disk
            // arm. `definition_from_disk` returns `ConfigurationError` for
            // unparseable YAML, so a typo in `orders.automation.yml` opened in
            // the IDE answered 503 + `Retry-After` — a retryable status for a
            // permanent condition, told to a client that will never see it
            // resolve.
            //
            // `ArtifactError::retryable()` is exactly `!matches!(self,
            // Config(_))`, i.e. the line between "could not look" and "looked,
            // and the content is bad". Reuse it rather than restating it.
            Err(e) => {
                let retryable = e.retryable();
                tracing::warn!(
                    workspace_id = %self.workspace_manager.workspace_id,
                    workflow_ref,
                    error = ?e,
                    retryable,
                    "automation definition read failed"
                );
                return Err(if retryable {
                    WorkspaceReadError::Unavailable(e.to_string())
                } else {
                    WorkspaceReadError::Invalid(e.to_string())
                });
            }
        }

        // Authenticated callers can supply an arbitrary `workflow_ref` via
        // the `path_b64` route param (and the queued workflow spec), so we
        // must contain it to the workspace root before reading. A raw
        // `workspace_path().join(workflow_ref)` would happily resolve
        // `../../etc/passwd` or replace the prefix entirely with an
        // absolute path — `ConfigManager::resolve_file` runs
        // `validate_path_within_project` which canonicalises and rejects
        // anything outside the workspace.
        //
        // Both failures below are `Missing`: past the boundary arm we are
        // reading this node's own filesystem, where "not there" is the answer
        // and not a symptom.
        let resolved = resolve_workspace_relative(&self.workspace_manager, workflow_ref)
            .await
            .map_err(WorkspaceReadError::Missing)?;
        std::fs::read_to_string(&resolved).map_err(|e| {
            WorkspaceReadError::Missing(format!("failed to read workflow {workflow_ref:?}: {e}"))
        })
    }
}
