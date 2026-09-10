//! Canonical oxy → airlayer semantic compatibility layer.
//!
//! This is the **single** parser/validator for oxy's `.view.yml` /
//! `.topic.yml` files. It exists because oxy's YAML format differs slightly
//! from airlayer's strict schema (optional `description`, the `data_source`
//! alias for `datasource`, defaulted collections). Before this crate the
//! same shim was duplicated across `agentic-analytics`, the host builder
//! validator, and partially diverged in the automation semantic bridge — a
//! file accepted by one path could be rejected by another. See
//! `internal-docs/semantic-validation-standardization.md`.
//!
//! Pure infrastructure: depends only on `airlayer` + `serde`. No `oxy-*`,
//! no `agentic-*`. Importable by any agentic domain, the host adapters,
//! and the CLI alike.
//!
//! It is also the **sole** crate in the workspace that may declare `airlayer`
//! as a dependency. Everything oxy uses is re-exported below, so an airlayer
//! upgrade lands in one manifest and is reviewed in one place instead of
//! rippling through nine. `tests::this_is_the_sole_airlayer_dependent`
//! enforces it — if you are here to add `airlayer` to another manifest, add a
//! re-export or a helper above instead.

pub mod engine_cache;
pub mod layer_cache;
pub mod lever_conflicts;
mod one_door_guard;

pub use engine_cache::{EngineKey, LayerSource, SemanticEngineCache, dialect_fingerprint};
pub use layer_cache::{LayerKey, SemanticLayerCache};
pub use lever_conflicts::{LeverConflict, lever_conflicts, reject_lever_conflicts};

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub use airlayer::{
    DatabaseConfig, DatasourceDialectMap, Dialect, Dimension, Entity, Measure, RefreshKey,
    SemanticEngine, SemanticLayer, Topic, View,
};
pub use airlayer::{dialect, engine, preagg, schema};

/// Error from parsing or validating oxy semantic files.
#[derive(Debug, thiserror::Error)]
pub enum SemanticError {
    #[error("failed to read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse semantic YAML: {0}")]
    Parse(#[from] serde_yaml::Error),
    /// airlayer engine construction / compile validation failure.
    #[error("semantic engine error: {0}")]
    Engine(String),
    /// A `binding:` on an entity that the operating graph cannot honour.
    #[error("entity binding: {0}")]
    Binding(String),
}

// ── YAML shim types ──────────────────────────────────────────────────────────
//
// Thin wrappers that add `#[serde(default)]` on optional fields and accept
// oxy's YAML aliases before converting into the real airlayer types.

/// Intermediate view representation for oxy YAML files.
#[derive(Debug, Deserialize)]
struct ViewShim {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    label: Option<String>,
    /// Oxy YAML may use `data_source` or `datasource`.
    #[serde(default, alias = "data_source")]
    datasource: Option<String>,
    #[serde(default)]
    dialect: Option<String>,
    #[serde(default)]
    table: Option<String>,
    #[serde(default)]
    sql: Option<String>,
    #[serde(default)]
    entities: Vec<EntityShim>,
    #[serde(default)]
    dimensions: Vec<airlayer::Dimension>,
    #[serde(default)]
    measures: Option<Vec<airlayer::Measure>>,
    #[serde(default)]
    segments: Vec<airlayer::schema::models::Segment>,
    #[serde(default)]
    refresh_key: Option<airlayer::schema::models::RefreshKey>,
    #[serde(default)]
    pre_aggregations: Option<Vec<airlayer::schema::models::PreAggregation>>,
    /// Free-form user metadata (e.g. the `freshness_*` contract keys read by
    /// the `check_data_freshness` tool). Must survive the shim round-trip.
    #[serde(default)]
    meta: Option<std::collections::HashMap<String, Vec<String>>>,
}

/// An entity as oxy YAML writes it: airlayer's shape plus `binding:`.
///
/// airlayer has no `binding` field and no `deny_unknown_fields`, so a bare
/// `airlayer::Entity` would accept the key and silently drop it — the worst
/// outcome for a declaration that decides which store a number lands on. The
/// shim captures it and carries it through `Entity::meta` (`binding:
/// [registry, system]`), which airlayer keeps verbatim, so no airlayer change
/// is needed and every reader goes through [`entity_binding`].
#[derive(Debug, Deserialize)]
struct EntityShim {
    #[serde(flatten)]
    inner: airlayer::Entity,
    #[serde(default)]
    binding: Option<EntityBinding>,
}

/// `binding: { registry: locations, system: toast }` on a view's primary
/// entity: this entity's key is what `system` calls one of the org's
/// locations, so the platform can resolve a warehouse key to a place through
/// `location_external_ids`. See `internal-docs/operating-graph.md` §3.6.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EntityBinding {
    /// The platform registry bound to. Only `locations` exists.
    pub registry: String,
    /// The integration whose ids the key carries — `toast`, `unifi`,
    /// `payroll`: a lowercase token, the same shape the registry accepts.
    pub system: String,
}

/// The `meta` key the binding travels under inside `airlayer::Entity`.
pub const BINDING_META_KEY: &str = "binding";

impl EntityBinding {
    fn validate(&self, entity: &airlayer::Entity) -> Result<(), SemanticError> {
        if self.registry != "locations" {
            return Err(SemanticError::Binding(format!(
                "entity `{}` binds to registry `{}`; only `locations` exists",
                entity.name, self.registry
            )));
        }
        let token_ok = !self.system.is_empty()
            && self.system.len() <= 32
            && self
                .system
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !token_ok {
            return Err(SemanticError::Binding(format!(
                "entity `{}` binds system `{}`; a system is a short lowercase token like `toast`",
                entity.name, self.system
            )));
        }
        if !matches!(
            entity.entity_type,
            airlayer::schema::models::EntityType::Primary
        ) {
            return Err(SemanticError::Binding(format!(
                "entity `{}` is bound but not primary; bind where the entity is defined",
                entity.name
            )));
        }
        if entity.get_keys().len() != 1 {
            return Err(SemanticError::Binding(format!(
                "entity `{}` is bound but has {} keys; a binding needs exactly one",
                entity.name,
                entity.get_keys().len()
            )));
        }
        Ok(())
    }
}

impl EntityShim {
    fn into_entity(self) -> Result<airlayer::Entity, SemanticError> {
        let EntityShim { mut inner, binding } = self;
        if let Some(binding) = binding {
            binding.validate(&inner)?;
            inner.meta.get_or_insert_with(Default::default).insert(
                BINDING_META_KEY.to_string(),
                vec![binding.registry, binding.system],
            );
        }
        Ok(inner)
    }
}

/// The binding an entity carries, if any. The one reader of the `meta` slot,
/// so the spelling lives in exactly one place.
pub fn entity_binding(entity: &airlayer::Entity) -> Option<EntityBinding> {
    let slot = entity.meta.as_ref()?.get(BINDING_META_KEY)?;
    match slot.as_slice() {
        [registry, system] => Some(EntityBinding {
            registry: registry.clone(),
            system: system.clone(),
        }),
        _ => None,
    }
}

/// A view whose primary entity is bound: the dimension its warehouse key
/// lives in, and what that key means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundView {
    pub view: String,
    pub entity: String,
    /// The primary entity's single key — the dimension a scope filter binds.
    pub key: String,
    pub binding: EntityBinding,
}

/// Every bound view in the layer.
pub fn bound_views(layer: &airlayer::SemanticLayer) -> Vec<BoundView> {
    layer
        .views
        .iter()
        .filter_map(|v| {
            let e = v
                .entities
                .iter()
                .find(|e| matches!(e.entity_type, airlayer::schema::models::EntityType::Primary))?;
            let binding = entity_binding(e)?;
            let key = e.get_keys().into_iter().next()?;
            Some(BoundView {
                view: v.name.clone(),
                entity: e.name.clone(),
                key,
                binding,
            })
        })
        .collect()
}

/// Intermediate topic representation for oxy YAML files.
#[derive(Debug, Deserialize)]
struct TopicShim {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    views: Vec<String>,
    #[serde(default)]
    base_view: Option<String>,
    #[serde(default)]
    retrieval: Option<airlayer::schema::models::TopicRetrievalConfig>,
    #[serde(default)]
    default_filters: Option<Vec<airlayer::schema::models::TopicFilter>>,
    #[serde(default)]
    meta: Option<std::collections::HashMap<String, Vec<String>>>,
}

// ── YAML parsing ─────────────────────────────────────────────────────────────

/// Parse an oxy `.view.yml` string into an `airlayer::View`.
///
/// Handles differences from airlayer's strict format:
/// - `description` defaults to `None` when absent
/// - `data_source` accepted as an alias for `datasource`
pub fn parse_view_yaml(yaml: &str) -> Result<airlayer::View, SemanticError> {
    let shim: ViewShim = serde_yaml::from_str(yaml)?;
    Ok(airlayer::View {
        name: shim.name,
        description: shim.description,
        label: shim.label,
        datasource: shim.datasource,
        dialect: shim.dialect,
        table: shim.table,
        sql: shim.sql,
        entities: shim
            .entities
            .into_iter()
            .map(EntityShim::into_entity)
            .collect::<Result<Vec<_>, _>>()?,
        dimensions: shim.dimensions,
        measures: shim.measures,
        segments: shim.segments,
        pre_aggregations: shim.pre_aggregations,
        refresh_key: shim.refresh_key,
        meta: shim.meta,
    })
}

/// Parse an oxy `.topic.yml` string into an `airlayer::Topic`.
pub fn parse_topic_yaml(yaml: &str) -> Result<airlayer::Topic, SemanticError> {
    let shim: TopicShim = serde_yaml::from_str(yaml)?;
    Ok(airlayer::Topic {
        name: shim.name,
        description: shim.description,
        views: shim.views,
        base_view: shim.base_view,
        retrieval: shim.retrieval,
        default_filters: shim.default_filters,
        meta: shim.meta,
    })
}

// ── File-set loading / validation ────────────────────────────────────────────

/// Returns `true` for a `.view.yml` / `.view.yaml` file name.
fn is_view_file(name: &str) -> bool {
    name.ends_with(".view.yml") || name.ends_with(".view.yaml")
}

/// Returns `true` for a `.topic.yml` / `.topic.yaml` file name.
fn is_topic_file(name: &str) -> bool {
    name.ends_with(".topic.yml") || name.ends_with(".topic.yaml")
}

/// Parse an explicit list of `.view.yml` / `.topic.yml` paths into an
/// [`airlayer::SemanticLayer`]. Files with other suffixes are ignored.
///
/// This is the one place that does the oxy-flavored parse loop; every
/// caller (analytics catalog load, builder validation, automation bridge,
/// `oxy validate`) should funnel through here so they cannot disagree.
pub fn build_layer<P: AsRef<Path>>(paths: &[P]) -> Result<airlayer::SemanticLayer, SemanticError> {
    let mut views = Vec::new();
    let mut topics = Vec::new();

    for path in paths {
        let path = path.as_ref();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !is_view_file(name) && !is_topic_file(name) {
            continue;
        }
        let content = std::fs::read_to_string(path).map_err(|source| SemanticError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if is_view_file(name) {
            views.push(parse_view_yaml(&content)?);
        } else {
            topics.push(parse_topic_yaml(&content)?);
        }
    }

    inject_row_count_measures(&mut views);

    let topic_opt = if topics.is_empty() {
        None
    } else {
        Some(topics)
    };
    Ok(airlayer::SemanticLayer::new(views, topic_opt))
}

/// Inject a `_row_count: count` measure into every view so fiber-count queries
/// always have a `SELECT COUNT(*)` handle via the semantic model.
fn inject_row_count_measures(views: &mut Vec<airlayer::View>) {
    for view in views.iter_mut() {
        view.measures
            .get_or_insert_with(Vec::new)
            .push(airlayer::schema::models::Measure {
                name: "__oxy_row_count".to_string(),
                measure_type: airlayer::schema::models::MeasureType::Count,
                description: None,
                expr: None,
                original_expr: None,
                filters: None,
                samples: None,
                synonyms: None,
                rolling_window: None,
                inherits_from: None,
                drivers: None,
                shift: None,
                // Both new in airlayer f0bacc8. `direction` defaults to
                // `HigherIsBetter` and is `skip_serializing_if` on that value,
                // so `Default::default()` keeps this injected measure
                // serializing byte-identically to before the bump. A row count
                // has no meaningful polarity to state, and airlayer never
                // infers one from a name.
                direction: Default::default(),
                // `__oxy_row_count` is injected, not authored, so it names no
                // peer cohort to be compared within.
                default_cohort: None,
                meta: None,
            });
    }
}

/// Directory names that are never descended into when discovering semantic
/// files: hidden dirs plus the usual build-output dirs.
///
/// The load-bearing entry is `.worktrees/` — a git worktree there holds a
/// *full* copy of the project (see `oxy_git`'s `get_or_create_worktree`), so a
/// blind recursive walk re-discovers every `.view.yml` through the worktree
/// copy and engine construction then fails with "Duplicate view name". This is
/// branch/prod-only: a workspace that has ever checked out a non-default branch
/// in the IDE has a worktree on disk, while a fresh local checkout does not.
/// `.git` / `.repositories` / `.oxy_state` are skipped for the same reason.
///
/// Mirrors the skip list in `oxy_semantic`'s parser and the IDE file-tree's
/// `HIDDEN_DIRS` so every walker over the workspace agrees on what is and isn't
/// project source.
fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "target" | "node_modules" | "dist" | "build")
}

/// Recursively collect `.view.yml` / `.topic.yml` files under `root`.
fn collect_semantic_paths(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_skipped_dir)
            {
                continue;
            }
            collect_semantic_paths(&path, out);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && (is_view_file(name) || is_topic_file(name))
        {
            out.push(path);
        }
    }
}

/// Discover and parse every `.view.yml` / `.topic.yml` under `root` (recursive).
///
/// The canonical replacement for `airlayer::SemanticEngine::load(dir, …)`:
/// using airlayer's native directory loader bypasses the oxy shim (it
/// rejects the `data_source` alias and lacks the defaulted-collection
/// leniency), which is exactly how the automation path used to disagree with
/// analytics. Funnel directory loads through here instead.
pub fn load_layer_from_dir(root: &Path) -> Result<airlayer::SemanticLayer, SemanticError> {
    // An unreadable ROOT is an infrastructure fault, not "this project models
    // nothing". `collect_semantic_paths` deliberately swallows `read_dir`
    // failures so an unreadable *subdirectory* can't sink the whole walk — but
    // applied to the root that turns "the workspace isn't on this disk" into an
    // empty layer, and callers then report it as a modelling mistake
    // (`Topic 'x' not found. Available: []`), sending people to audit YAML that
    // was never read. Check the root explicitly so that failure names itself.
    std::fs::read_dir(root).map_err(|source| SemanticError::Io {
        path: root.display().to_string(),
        source,
    })?;
    let mut paths = Vec::new();
    collect_semantic_paths(root, &mut paths);
    // Deterministic order so engine construction is reproducible.
    paths.sort();
    build_layer(&paths)
}

/// Parse + build the airlayer engine to surface compile/validation errors.
///
/// Dialect-agnostic (empty [`airlayer::DatasourceDialectMap`]) so it can be
/// used as a pure "is this semantic file set well-formed?" check by the
/// builder validator and `oxy validate`. Callers that need a dialect-aware
/// engine (analytics) build it from [`build_layer`] with their own map.
/// Parse + compile `paths` purely to surface errors. No DB, no connectors.
///
/// Private: callers that want a layer should take one from `build_layer`, and
/// the write gate is the only thing that wants the yes/no.
fn validate_files<P: AsRef<Path>>(paths: &[P]) -> Result<(), SemanticError> {
    let layer = build_layer(paths)?;
    build_engine(layer, &[])?;
    Ok(())
}

/// A datasource config for the dialect map.
///
/// Pass `dialect()`, never the raw `type:` string: airhouse and motherduck
/// speak an engine their type name does not name, and airlayer drops a
/// datasource it cannot classify — silently inheriting whichever dialect
/// `config.yml` happens to list first.
pub fn database_config(name: impl Into<String>, dialect: impl Into<String>) -> DatabaseConfig {
    DatabaseConfig {
        name: name.into(),
        db_type: dialect.into(),
    }
}

/// Build an engine over `layer`, resolving dialects from `databases`.
///
/// The dialect map must be derived from the same database list the query will
/// run against; building it separately is how call sites drifted apart.
pub fn build_engine(
    layer: SemanticLayer,
    databases: &[DatabaseConfig],
) -> Result<SemanticEngine, SemanticError> {
    let dialects = DatasourceDialectMap::from_config_databases(databases);
    SemanticEngine::from_semantic_layer(layer, dialects)
        .map_err(|e| SemanticError::Engine(e.to_string()))
}

/// Two paths refer to the same file. Canonicalizes when possible (handles
/// `..`, symlinks); falls back to literal comparison for not-yet-created
/// files.
fn same_path(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Build the project's semantic model with `target`'s content replaced by
/// `proposed` (or `proposed` added as a new file when `target` is not yet
/// on disk). The pre-write equivalent of "what analytics would load after
/// this edit lands".
fn build_layer_with_override(
    root: &Path,
    target: &Path,
    proposed: &str,
) -> Result<airlayer::SemanticLayer, SemanticError> {
    let mut paths = Vec::new();
    collect_semantic_paths(root, &mut paths);
    paths.sort();

    let mut views = Vec::new();
    let mut topics = Vec::new();
    let mut replaced = false;

    for p in &paths {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let is_target = same_path(p, target);
        let content = if is_target {
            replaced = true;
            proposed.to_string()
        } else {
            std::fs::read_to_string(p).map_err(|source| SemanticError::Io {
                path: p.display().to_string(),
                source,
            })?
        };
        if is_view_file(name) {
            views.push(parse_view_yaml(&content)?);
        } else if is_topic_file(name) {
            topics.push(parse_topic_yaml(&content)?);
        }
    }

    if !replaced {
        // New file not yet on disk — include the proposed content.
        let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if is_view_file(name) {
            views.push(parse_view_yaml(proposed)?);
        } else if is_topic_file(name) {
            topics.push(parse_topic_yaml(proposed)?);
        }
    }

    inject_row_count_measures(&mut views);

    let topic_opt = if topics.is_empty() {
        None
    } else {
        Some(topics)
    };
    Ok(airlayer::SemanticLayer::new(views, topic_opt))
}

/// Pre-write gate for `.view.yml` / `.topic.yml`.
///
/// Returns `Err(reason)` when the write must be refused:
/// - the proposed content does not parse (malformed YAML / wrong shape) —
///   **always** refused; an unparsable file is unambiguously the writer's
///   fault and analytics would fail-fast on it.
/// - the proposed content breaks the semantic engine **and the on-disk
///   layer compiled cleanly before this change** — a working→broken
///   regression.
///
/// When the layer was already broken (the delegated builder is mid-repair),
/// only the parse check applies: cross-file engine errors are allowed so the
/// agent can make progress instead of being stuck behind a pre-existing
/// breakage it is trying to fix.
///
/// Non-semantic paths are always `Ok` — this is a no-op for them.
pub fn gate_semantic_write(root: &Path, target_abs: &Path, proposed: &str) -> Result<(), String> {
    let name = target_abs
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if !is_view_file(name) && !is_topic_file(name) {
        return Ok(());
    }

    // 1. The proposed file itself must parse — always enforced.
    let parsed = if is_view_file(name) {
        parse_view_yaml(proposed).map(|_| ())
    } else {
        parse_topic_yaml(proposed).map(|_| ())
    };
    if let Err(e) = parsed {
        return Err(format!(
            "semantic validation failed for '{name}': {e}. \
             The change was not applied — fix the file and retry."
        ));
    }

    // 2. Regression guard: only block cross-file breakage if the layer was
    //    healthy before this edit.
    let mut on_disk = Vec::new();
    collect_semantic_paths(root, &mut on_disk);
    let before_ok = validate_files(&on_disk).is_ok();
    if !before_ok {
        // Already broken — the builder is presumably fixing it. The parse
        // check above still guarantees this file is well-formed.
        return Ok(());
    }

    match build_layer_with_override(root, target_abs, proposed) {
        Ok(layer) => airlayer::SemanticEngine::from_semantic_layer(
            layer,
            airlayer::DatasourceDialectMap::new(),
        )
        .map(|_| ())
        .map_err(|e| {
            format!(
                "semantic validation failed for '{name}': applying this change \
                 would break the semantic model ({e}). The change was not \
                 applied — adjust it so the layer still compiles."
            )
        }),
        // A parse error in some *other* file despite before_ok shouldn't
        // happen, but treat it as non-blocking rather than misattribute it.
        Err(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_accepts_data_source_alias_and_defaults() {
        let yaml = "name: orders\ndata_source: warehouse\ntable: orders\n";
        let v = parse_view_yaml(yaml).expect("valid view");
        assert_eq!(v.name, "orders");
        assert_eq!(v.datasource.as_deref(), Some("warehouse"));
        assert!(v.description.is_none());
        assert!(v.dimensions.is_empty());
    }

    #[test]
    fn a_binding_survives_parsing_and_reads_back_through_one_door() {
        let yaml = "name: sales\ntable: sales\nentities:\n  - name: store\n    type: primary\n    key: restaurant_id\n    binding: { registry: locations, system: toast }\n";
        let v = parse_view_yaml(yaml).expect("valid view");
        let store = &v.entities[0];
        assert_eq!(
            entity_binding(store),
            Some(EntityBinding {
                registry: "locations".into(),
                system: "toast".into()
            })
        );
        assert_eq!(store.key.as_deref(), Some("restaurant_id"));
        let layer = airlayer::SemanticLayer {
            views: vec![v],
            topics: None,
            motifs: None,
            saved_queries: None,
            metadata: None,
        };
        let bound = bound_views(&layer);
        assert_eq!(bound.len(), 1);
        assert_eq!(bound[0].view, "sales");
        assert_eq!(bound[0].key, "restaurant_id");
        assert_eq!(bound[0].binding.system, "toast");
    }

    #[test]
    fn an_unbound_entity_binds_nothing() {
        let yaml = "name: sales\ntable: sales\nentities:\n  - name: store\n    type: primary\n    key: restaurant_id\n";
        let v = parse_view_yaml(yaml).expect("valid view");
        assert_eq!(entity_binding(&v.entities[0]), None);
    }

    #[test]
    fn a_binding_that_cannot_be_honoured_is_refused_at_parse() {
        let cases = [
            ("registry: warehouses, system: toast", "only `locations`"),
            ("registry: locations, system: Toast", "lowercase"),
            (
                "registry: locations, system: toast }\n    keys: [a, b]\n    x: { y",
                "exactly one",
            ),
        ];
        for (binding, expect) in cases {
            let yaml = format!(
                "name: sales\ntable: sales\nentities:\n  - name: store\n    type: primary\n    key: restaurant_id\n    binding: {{ {binding} }}\n"
            );
            match parse_view_yaml(&yaml) {
                Err(SemanticError::Binding(msg)) => assert!(msg.contains(expect), "{msg}"),
                Err(SemanticError::Parse(_)) if expect == "exactly one" => {}
                other => panic!("expected a binding refusal for {binding:?}, got {other:?}"),
            }
        }
        // A foreign entity is a usage, not a definition.
        let yaml = "name: sales\ntable: sales\nentities:\n  - name: store\n    type: foreign\n    key: restaurant_id\n    binding: { registry: locations, system: toast }\n";
        assert!(
            matches!(parse_view_yaml(yaml), Err(SemanticError::Binding(m)) if m.contains("primary"))
        );
    }

    #[test]
    fn topic_minimal_parses() {
        let t = parse_topic_yaml("name: sales\nviews: [orders]\n").expect("valid topic");
        assert_eq!(t.name, "sales");
        assert_eq!(t.views, vec!["orders".to_string()]);
    }

    #[test]
    fn malformed_yaml_is_parse_error() {
        let err = parse_view_yaml("name: [unclosed").unwrap_err();
        assert!(matches!(err, SemanticError::Parse(_)));
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "alc_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(d.join("semantics/views")).unwrap();
        d
    }

    #[test]
    fn gate_ignores_non_semantic_files() {
        let d = tmpdir("gate_nonsem");
        assert!(gate_semantic_write(&d, &d.join("config.yml"), "not: validated").is_ok());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn gate_always_rejects_unparsable_proposed() {
        let d = tmpdir("gate_malformed");
        let target = d.join("semantics/views/x.view.yml");
        let err = gate_semantic_write(&d, &target, "name: [unclosed").unwrap_err();
        assert!(err.contains("semantic validation failed"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn gate_allows_valid_edit_on_valid_layer() {
        let d = tmpdir("gate_ok");
        std::fs::write(
            d.join("semantics/views/orders.view.yml"),
            "name: orders\ntable: orders\n",
        )
        .unwrap();
        let target = d.join("semantics/views/orders.view.yml");
        assert!(gate_semantic_write(&d, &target, "name: orders\ntable: orders_v2\n").is_ok());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn gate_blocks_regression_on_previously_valid_layer() {
        let d = tmpdir("gate_regress");
        std::fs::write(
            d.join("semantics/views/orders.view.yml"),
            "name: orders\ntable: orders\n",
        )
        .unwrap();
        // Add a topic whose base_view does not exist — parseable, but the
        // engine rejects it (this is the exact original-bug shape).
        let target = d.join("semantics/topics/bad.topic.yml");
        let res = gate_semantic_write(
            &d,
            &target,
            "name: bad\nbase_view: missing_view\nviews: [missing_view]\n",
        );
        assert!(
            res.is_err(),
            "a working→broken regression must be refused, got {res:?}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn gate_allows_edit_when_layer_already_broken() {
        let d = tmpdir("gate_repair");
        // Pre-existing broken topic on disk.
        std::fs::create_dir_all(d.join("semantics/topics")).unwrap();
        std::fs::write(
            d.join("semantics/topics/bad.topic.yml"),
            "name: bad\nbase_view: missing_view\nviews: [missing_view]\n",
        )
        .unwrap();
        // A perfectly valid, unrelated view edit must still be allowed so
        // the delegated builder can make progress repairing the layer.
        let target = d.join("semantics/views/orders.view.yml");
        assert!(
            gate_semantic_write(&d, &target, "name: orders\ntable: orders\n").is_ok(),
            "edits must be allowed while the layer is already broken (repair)"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn load_layer_from_dir_discovers_nested_and_honors_data_source_alias() {
        let dir = std::env::temp_dir().join(format!("alc_dir_{}", std::process::id()));
        let views = dir.join("semantics/views");
        std::fs::create_dir_all(&views).unwrap();
        // `data_source` alias — airlayer's native loader would reject this;
        // the shared loader (used by analytics, automation, builder) must not.
        std::fs::write(
            views.join("orders.view.yml"),
            "name: orders\ndata_source: warehouse\ntable: orders\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("semantics/sales.topic.yml"),
            "name: sales\nviews: [orders]\n",
        )
        .unwrap();

        let layer = load_layer_from_dir(&dir).expect("dir load");
        assert_eq!(layer.views.len(), 1);
        assert_eq!(layer.views[0].datasource.as_deref(), Some("warehouse"));
        assert_eq!(layer.topics.as_ref().map(|t| t.len()), Some(1));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_layer_from_dir_skips_worktree_and_state_copies() {
        // Reproduces the prod "Duplicate view name" failure. A branch worktree
        // under `.worktrees/` holds a full copy of the project's semantic
        // files; scanning the workspace root must not re-discover them, or
        // engine construction fails with duplicate view names. Locally this
        // dir is empty so the bug never surfaces — hence "works on local,
        // errors on prod".
        let dir = std::env::temp_dir().join(format!("alc_wt_{}", std::process::id()));
        let views = dir.join("semantics/views");
        std::fs::create_dir_all(&views).unwrap();
        std::fs::write(
            views.join("orders.view.yml"),
            "name: orders\ndata_source: warehouse\ntable: orders\n",
        )
        .unwrap();

        // Full copies of the same view in dirs other walkers already skip:
        // a git worktree checkout and the oxy state dir.
        for copy in [
            ".worktrees/feature-x/semantics/views",
            ".oxy_state/cache/semantics/views",
        ] {
            let p = dir.join(copy);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(
                p.join("orders.view.yml"),
                "name: orders\ndata_source: warehouse\ntable: orders\n",
            )
            .unwrap();
        }

        let layer = load_layer_from_dir(&dir).expect("dir load");
        assert_eq!(
            layer.views.len(),
            1,
            "copies under hidden dirs (.worktrees/, .oxy_state/) must not be re-discovered"
        );
        // The real failure was at engine construction — assert it builds clean.
        airlayer::SemanticEngine::from_semantic_layer(layer, airlayer::DatasourceDialectMap::new())
            .expect("engine builds without a duplicate-view-name error");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Regression: a scan root that isn't on this disk must ERROR, not return
    /// an empty layer. Returning empty made a stateless replica with no working
    /// copy report `Topic 'x' not found. Available: []` — a modelling error for
    /// what is actually a missing directory, which is unactionable for the user
    /// and undiagnosable from the response.
    #[test]
    fn missing_scan_root_errors_instead_of_yielding_empty_layer() {
        let missing = std::env::temp_dir().join(format!("alc_missing_{}", std::process::id()));
        std::fs::remove_dir_all(&missing).ok();

        let err = load_layer_from_dir(&missing).expect_err("a missing scan root must not succeed");
        assert!(
            matches!(err, SemanticError::Io { .. }),
            "expected an Io error naming the unreadable root, got: {err}"
        );
        assert!(
            err.to_string().contains(&missing.display().to_string()),
            "the error must name the path that could not be read: {err}"
        );
    }

    /// The counter-case: a root that DOES exist but models nothing is a legitimate
    /// empty layer, not an error. A workspace may ship zero `.view.yml` files.
    #[test]
    fn present_but_empty_scan_root_yields_empty_layer() {
        let dir = std::env::temp_dir().join(format!("alc_empty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let layer =
            load_layer_from_dir(&dir).expect("an existing root with no semantic files is Ok");
        assert!(layer.views.is_empty(), "no views were defined");
        assert!(layer.topics.is_none(), "no topics were defined");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `oxy-airlayer-compat` is the only crate that may depend on `airlayer`.
    ///
    /// Before this was enforced, `airlayer::` appeared in 414 places across
    /// nine crates — more of it in the HTTP transport layer than in the
    /// infrastructure layer that owns the adapter. Every consumer went
    /// straight to the engine, so the oxy-side lenience rules (the
    /// `data_source` alias, the skip-dir list, the injected row-count
    /// measure) were opt-in rather than unavoidable, and a pinned-rev bump
    /// had nine blast radii instead of one.
    #[test]
    fn this_is_the_sole_airlayer_dependent() {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .find(|p| p.join("crates").is_dir() && p.join("Cargo.toml").is_file())
            .expect("workspace root above crates/infrastructure/semantic")
            .to_path_buf();

        let this_crate = workspace_root.join("crates/infrastructure/semantic/Cargo.toml");
        let mut offenders = Vec::new();

        let mut stack = vec![workspace_root.join("crates")];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if path.file_name().is_some_and(|n| n == "target") {
                        continue;
                    }
                    stack.push(path);
                } else if path.file_name().is_some_and(|n| n == "Cargo.toml") && path != this_crate
                {
                    let Ok(manifest) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    // A dependency entry, not a mention in a comment: the key
                    // sits at the start of a line and is followed by `=`.
                    if manifest
                        .lines()
                        .any(|l| l.starts_with("airlayer") && l.contains('='))
                    {
                        offenders.push(
                            path.strip_prefix(&workspace_root)
                                .unwrap_or(&path)
                                .display()
                                .to_string(),
                        );
                    }
                }
            }
        }

        offenders.sort();
        assert!(
            offenders.is_empty(),
            "these manifests declare `airlayer` directly; depend on \
             oxy-airlayer-compat and use its re-exports instead:\n  {}",
            offenders.join("\n  ")
        );
    }
}
