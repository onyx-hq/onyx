//! Workspace context trait.
//!
//! `agentic-automation` needs workspace capabilities (file listing, database
//! access, integration access, path resolution) but does NOT depend on `oxy`.
//! The pipeline layer implements this trait for `oxy::WorkspaceManager`.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use agentic_connector::DatabaseConnector;

use crate::refresh_key_cache::RefreshKeyCache;

/// Why a workspace read failed, when the caller has to tell the two apart.
///
/// Three answers, because a read can fail three ways and they want three
/// statuses:
///
/// - `Missing` — not in the workspace. 404.
/// - `Invalid` — it IS here and does not parse. 422. A permanent condition;
///   calling it retryable tells a client to come back for a file that will
///   never load, and removes the only status that could say "your YAML is
///   broken".
/// - `Unavailable` — this node could not look. 503.
///
/// Reporting `Unavailable` as `Missing` tells a caller their automation does
/// not exist because a database blipped; reporting `Invalid` as `Unavailable`
/// tells them to retry a typo. The host must label by the SHAPE of the
/// failure, never by which call site produced it — the first version of this
/// inferred `Unavailable` from position and swept both other shapes into it.
///
/// A plain enum, not `oxy`'s `ArtifactError`: `agentic-automation` is a domain
/// crate and must not depend on the platform (see `backend-architecture.md`).
///
/// `From<String>` / `From<&str>` land on `Missing`, so a host that cannot tell
/// the two apart keeps today's behaviour rather than over-claiming that a
/// failure is worth retrying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceReadError {
    Missing(String),
    Invalid(String),
    Unavailable(String),
}

impl WorkspaceReadError {
    /// Whether the caller should come back rather than conclude anything.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable(_))
    }

    /// Whether the artifact is present and unusable — the caller's content
    /// problem, and not worth a retry.
    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid(_))
    }
}

impl std::fmt::Display for WorkspaceReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(m) | Self::Invalid(m) | Self::Unavailable(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for WorkspaceReadError {}

impl From<String> for WorkspaceReadError {
    fn from(message: String) -> Self {
        Self::Missing(message)
    }
}

impl From<&str> for WorkspaceReadError {
    fn from(message: &str) -> Self {
        Self::Missing(message.to_string())
    }
}

/// Resolved integration credentials ready for use.
///
/// The pipeline layer resolves secrets and passes plain values.
#[derive(Debug, Clone)]
pub enum IntegrationConfig {
    Omni {
        base_url: String,
        api_key: String,
    },
    Looker {
        base_url: String,
        client_id: String,
        client_secret: String,
    },
}

/// The directory an agent's `context:` globs resolve against.
///
/// FS-backed hosts (the IDE working copy) return their workspace path with no
/// guard. A stateless host (the serve fleet, which has no working copy)
/// materialises the compiled context entities into a tempdir and returns a
/// guard that owns it. `path()` is valid only while the `ContextRoot` is alive,
/// so the caller MUST keep it in scope until context resolution (glob +
/// semantic-catalog load) has finished.
pub struct ContextRoot {
    path: PathBuf,
    // Opaque so this crate needn't depend on `tempfile`; the host boxes the
    // `TempDir` guard in here and it drops (cleaning up) with the `ContextRoot`.
    _guard: Option<Box<dyn std::any::Any + Send + Sync>>,
    /// The compiled revision this root was materialised from, or `None` for a
    /// working copy. See [`ContextRoot::revision`].
    revision: Option<uuid::Uuid>,
}

impl ContextRoot {
    /// Resolve context from an on-disk workspace path (IDE / shared-FS).
    pub fn fs(path: PathBuf) -> Self {
        Self {
            path,
            _guard: None,
            revision: None,
        }
    }

    /// Resolve context from a materialised tempdir; `guard` owns it for the
    /// `ContextRoot`'s lifetime (typically a `tempfile::TempDir`).
    ///
    /// `revision_id` is the revision the tempdir was materialised FROM, and it
    /// is required rather than optional because a caller that caches anything
    /// derived from this root must key it by that revision — see
    /// [`Self::revision`].
    pub fn materialised(
        path: PathBuf,
        guard: Box<dyn std::any::Any + Send + Sync>,
        revision_id: uuid::Uuid,
    ) -> Self {
        Self {
            path,
            _guard: Some(guard),
            revision: Some(revision_id),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Which layer source this root represents: `Some(revision)` for a
    /// materialised compiled revision, `None` for a working copy.
    ///
    /// This is the bit [`WorkspaceContext::semantic_engine_cache`] deliberately
    /// does NOT hand out — its doc says the source "only the caller knows,
    /// because it depends on the path that caller scans". This accessor is how
    /// the caller knows: it scanned *this* root, so this root reports the
    /// source. Feed it to `EngineKey::for_source` / `LayerKey::for_source`.
    ///
    /// Getting it wrong is a staleness bug rather than a crash: caching an
    /// engine built from revision A under a working-copy key serves it for
    /// revision B, so a promote appears not to take effect.
    pub fn revision(&self) -> Option<uuid::Uuid> {
        self.revision
    }
}

/// Minimal workspace interface needed by the automation engine.
///
/// Implemented by the pipeline layer for `oxy::adapters::workspace::WorkspaceManager`.
#[async_trait::async_trait]
pub trait WorkspaceContext: Send + Sync {
    /// Root path of the workspace/project, when this process holds one.
    ///
    /// `None` on a stateless replica. The callers that resolve a ref against it
    /// and then OPEN the result — `export`, `sql_file`, `resolve_pipeline_ref`,
    /// the builder revert — genuinely cannot run there, and saying so beats
    /// resolving against a directory that is not present and failing with
    /// "No such file or directory" on a path the customer never wrote.
    ///
    /// It is NOT the semantic scan directory. [`Self::context_root`] is, and it
    /// serves the compiled boundary on a node with no files.
    fn workspace_path(&self) -> Option<&Path>;

    /// Directory an agent's `context:` globs resolve against. Defaults to the
    /// on-disk workspace path; the host adapter overrides it to materialise the
    /// compiled context from the boundary on a stateless replica that has no
    /// working copy. The returned guard must outlive context resolution.
    async fn context_root(&self) -> ContextRoot {
        ContextRoot::fs(self.workspace_path().unwrap_or(Path::new("")).to_path_buf())
    }

    /// Database configurations for dialect mapping.
    fn database_configs(&self) -> Vec<oxy_airlayer_compat::DatabaseConfig>;

    async fn get_connector(&self, name: &str) -> Result<Arc<dyn DatabaseConnector>, String>;

    async fn get_integration(&self, name: &str) -> Result<IntegrationConfig, String>;

    /// Fetch a project secret by name (Oxy Secrets, falling back to env var).
    ///
    /// Used by the `http_request` task to inject credentials into headers/body.
    /// Defaults to `None` so existing impls (test fakes) need no change; the real
    /// host adapter overrides it to forward to the `SecretsManager`. (Named
    /// distinctly from the pipeline `ProjectContext::resolve_secret` so a type
    /// implementing both traits has no ambiguous call.)
    async fn fetch_secret(&self, _name: &str) -> Option<String> {
        None
    }

    /// Store (upsert) a project secret — used by `http_request`'s
    /// `persist_to_secret` to write a rotated OAuth refresh token back to the
    /// secret store (mirrors the Airway Intuit source). Defaults to an error so
    /// only hosts with real secret storage allow it.
    async fn store_secret(&self, _name: &str, _value: &str) -> Result<(), String> {
        Err("secret persistence is not supported in this context".to_string())
    }

    async fn list_automation_files(&self) -> Result<Vec<PathBuf>, String>;

    /// Read the raw YAML content of an automation file.
    ///
    /// The error distinguishes "not in this workspace" from "this node could
    /// not look" — see [`WorkspaceReadError`]. The HTTP route answers 404 for
    /// the first and 503 for the second.
    async fn resolve_automation_yaml(
        &self,
        automation_ref: &str,
    ) -> Result<String, WorkspaceReadError>;

    /// Return the shared in-process refresh key cache, if the server has one configured.
    ///
    /// Returns `None` in contexts without a long-lived cache (e.g. CLI, tests).
    fn refresh_key_cache(&self) -> Option<Arc<RwLock<RefreshKeyCache>>> {
        None
    }

    /// Renewal threshold (seconds) used on the query read-path to decide
    /// whether a cached refresh-key is still fresh. Must match the worker's
    /// `pre_aggregations.refresh_worker.renewal_threshold`. Defaults to 120s.
    fn preagg_renewal_threshold_secs(&self) -> u64 {
        120
    }

    /// The workspace this context belongs to — the pre-aggregation cache key.
    /// `None` in contexts with no workspace row (CLI, tests), which disables
    /// the local-rollup short-circuit rather than guessing a key.
    fn preagg_workspace_id(&self) -> Option<uuid::Uuid> {
        None
    }

    /// Where to read a rollup this node did not build. `None` keeps single-node
    /// behavior: the local file is the only copy.
    fn preagg_blob(&self) -> Option<agentic_semantic::BlobConfig> {
        None
    }

    /// Whether this context's queries must decline a stale rollup rather than
    /// serve it. `false` is the read-surface posture every automation and Data
    /// App task wants; see `PreaggContext::require_fresh` for who sets it.
    fn preagg_require_fresh(&self) -> bool {
        false
    }

    /// The shared semantic-engine cache and the workspace to key it by.
    ///
    /// `None` — the default — compiles every query against an engine built for
    /// that call, which re-reads the semantic directory and rebuilds the join
    /// graph each time. The real host adapter overrides it; test fakes and CLI
    /// contexts need no change.
    ///
    /// Deliberately NOT derived from [`Self::preagg_workspace_id`], though both
    /// name a workspace: a host that returns `None` there to disable the
    /// pre-aggregation short-circuit must not silently disable engine caching
    /// too. One override, one meaning.
    ///
    /// No revision is offered, on purpose. The rest of an engine's identity is
    /// the layer SOURCE, which only the caller knows — it depends on the path
    /// that caller scans, not on which revision the context is pinned to. See
    /// `oxy_airlayer_compat::engine_cache::LayerSource`; handing a pinned
    /// revision out here is how it would get used as a key again.
    fn semantic_engine_cache(
        &self,
    ) -> Option<(
        std::sync::Arc<oxy_airlayer_compat::SemanticEngineCache>,
        uuid::Uuid,
    )> {
        None
    }

    /// The assembled local-rollup short-circuit, or `None` to compile every
    /// query to warehouse SQL. Assembled here, from one set of accessors, so
    /// no caller has to remember which pieces belong together.
    fn preagg_context(&self) -> Option<agentic_semantic::PreaggContext> {
        Some(agentic_semantic::PreaggContext {
            workspace_id: self.preagg_workspace_id()?,
            cache: self.refresh_key_cache()?,
            renewal_threshold_secs: self.preagg_renewal_threshold_secs(),
            blob: self.preagg_blob(),
            require_fresh: self.preagg_require_fresh(),
        })
    }

    /// List all `.airway.yml` pipeline files in the workspace. Default
    /// returns empty so existing impls (test fakes) need no change; the
    /// real host adapter overrides it.
    async fn list_airway_files(&self) -> Result<Vec<PathBuf>, String> {
        Ok(vec![])
    }

    /// Compile-boundary hook for an airway pipeline (`.airway.yml`) body,
    /// keyed by its workspace-relative `pipeline_ref`.
    ///
    /// `Ok(Some(yaml))` — the host served the pipeline from its compiled
    /// `airway_pipelines` rows; the caller parses that string and never
    /// touches the filesystem. This is what lets the durable worker fleet
    /// (stateless, no working copy) run a pipeline at all.
    ///
    /// `Ok(None)` (the default) — "read the workspace filesystem". Covers hosts
    /// that don't participate in the compile boundary (test fakes, CLI) *and*
    /// the host's own fall-through cases (unpromoted workspace, draft branch,
    /// no matching row). The FS read + its containment guard live in one place
    /// on the caller side:
    /// [`agentic_pipeline::pipeline_ref::load_pipeline_yaml`].
    ///
    /// `Err` — the boundary could not be *asked*: a lookup error, not an
    /// answer. This is a `Result` rather than an `Option` for exactly that
    /// distinction. While it was one, a host had no way to say "I don't know"
    /// and had to report a database blip as `None`, which reads as "nothing is
    /// compiled here" — so a retryable condition became a terminal run on a
    /// replica with no working copy to fall back to. An `Option` cannot carry
    /// the difference between *absent* and *unknown*, and every caller that
    /// has to guess gets it wrong in the direction of the cheerful answer.
    ///
    /// Mirrors `ProjectContext::resolve_agent_yaml`, the same hook shape for
    /// `.agentic.yml`.
    async fn resolve_pipeline_yaml(&self, _pipeline_ref: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    /// Read a `.sql` file's body from the compile boundary, keyed by its
    /// workspace-relative path.
    ///
    /// Same three-way contract as [`Self::resolve_pipeline_yaml`] above, and
    /// for the same reasons — see that doc for the full argument about why
    /// `Err` is distinct from `Ok(None)`. In short: `Ok(Some(sql))` came from
    /// Postgres and touched no filesystem; `Ok(None)` means "read the working
    /// copy"; `Err` means the boundary could not be *asked*, and must not fall
    /// through to a filesystem this node may not have.
    ///
    /// **The key must be the walker's normalized rel-path** — forward slashes,
    /// no `./` prefix — because that is what `verified_queries.file_path`
    /// holds. `step_executor` normalizes before calling; a `./`-prefixed ref
    /// reaching here would miss a row that exists and silently degrade to the
    /// instance affinity this hook removes.
    ///
    /// **The `Err` arm trades availability for consistency, deliberately.**
    /// Unlike the pipeline path's typed `PipelineRefError::Unavailable`, this
    /// returns a bare `String` with no retryable signal, and the root
    /// automation task is enqueued with `policy: None` (see this crate's
    /// `CLAUDE.md`), so nothing re-attempts it. A transient DB blip on a node
    /// that *does* hold the working copy therefore fails a step that would
    /// previously have read the file successfully. That is the intended
    /// direction: a run that silently read a stale or instance-local file is
    /// worse than one that fails visibly. If this becomes a practical problem,
    /// the fix is a typed error here, not a fall-through.
    ///
    /// Defaults to `Ok(None)` so test fakes and the CLI keep their existing
    /// filesystem behaviour without implementing it.
    async fn resolve_sql_file(&self, _sql_ref: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
}
