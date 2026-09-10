//! Executes individual automation steps.
//!
//! Dispatches by task type: SQL queries run via `agentic-connector`,
//! semantic queries compile via airlayer then execute, and unsupported
//! types return clear error messages.

use agentic_core::result::CellValue;
use serde_json::{Value, json};

use crate::config::{SemanticQueryConfig, TaskType};
use crate::render::{
    normalize_workspace_relative_ref, render_jinja_string, validate_workspace_relative_path,
};
use crate::workspace::WorkspaceContext;

/// Default row limit for step execution results.
const DEFAULT_SAMPLE_LIMIT: u64 = 10_000;

/// Execute a single automation step and return its output as JSON.
pub async fn run_automation_step(
    workspace: &dyn WorkspaceContext,
    step_config: Value,
    render_context: Value,
    _automation_context: Value,
) -> Result<String, String> {
    let task: crate::config::TaskConfig = serde_json::from_value(step_config)
        .map_err(|e| format!("failed to deserialize step config: {e}"))?;

    let result = match &task.task_type {
        TaskType::ExecuteSql(cfg) => execute_sql(workspace, cfg, &render_context).await,
        TaskType::SemanticQuery(cfg) => execute_semantic_query(workspace, cfg).await,

        // These should never reach step_executor — handled by orchestrator.
        TaskType::Agent(_) => Err("agent tasks are delegated via TaskSpec::Agent".into()),
        TaskType::Formatter(_) => Err("formatter tasks execute inline in orchestrator".into()),
        TaskType::Conditional(_) => Err("conditional tasks execute inline in orchestrator".into()),
        TaskType::LoopSequential(_) => Err("loops are handled as fan-out by orchestrator".into()),
        TaskType::SubAutomation(_) => {
            Err("sub-automations are delegated via TaskSpec::Automation".into())
        }
        TaskType::Airway(_) => Err("airway tasks are delegated via TaskSpec::Airway".into()),

        TaskType::OmniQuery(cfg) => execute_omni_query(workspace, cfg).await,
        TaskType::LookerQuery(cfg) => execute_looker_query(workspace, cfg).await,
        TaskType::HttpRequest(cfg) => execute_http_request(workspace, cfg, &render_context).await,

        // Catches the retired `type: visualize` (now `#[serde(other)]`)
        // and any unknown task type the schema doesn't recognise.
        TaskType::Unknown => Err("unknown task type".into()),
    }?;

    // Exports are written from the decision-task hook in
    // `pipeline::executor::automation::run_decision_task` after
    // `commit_decision` succeeds. That hook handles every step type
    // uniformly (inline, delegated automation steps, agent steps,
    // sub-automations) by walking the result_delta; doing it here too
    // would double-write for inline tasks. See
    // `pipeline/src/executor/automation.rs::write_step_exports`.

    serde_json::to_string(&result).map_err(|e| format!("failed to serialize result: {e}"))
}

/// Execute a raw SQL query via the database connector.
///
/// Jinja templating is applied at three points so automation authors can
/// pass loop/iteration context through to the SQL layer:
///
/// 1. `sql_file` path itself (e.g. `data/example_{{schedules.value}}.sql`).
/// 2. The per-task `variables` map values (each value may reference the
///    parent `render_context` — e.g. `variable_b: "{{ metrics.value }}"`).
/// 3. The SQL content (inline or from-file), with the merged
///    `render_context + variables` as its environment.
///
/// Without this rendering, a multi-loop automation ends up calling
/// `tokio::fs::read_to_string` on a literal `example_{{schedules.value}}_.sql`
/// path which obviously fails. Matches the legacy oxy-workflow runner's
/// `Renderer::eval_expression` / `render` behavior.
async fn execute_sql(
    workspace: &dyn WorkspaceContext,
    cfg: &Value,
    render_context: &Value,
) -> Result<Value, String> {
    let database = cfg
        .get("database")
        .and_then(|v| v.as_str())
        .ok_or("execute_sql: missing 'database' field")?;

    // Render the per-task `variables` map (its own values may reference
    // `render_context`) and merge into the SQL render env.
    let sql_context = merge_sql_variables(render_context, cfg.get("variables"))?;

    let raw_sql = if let Some(q) = cfg.get("sql_query").and_then(|v| v.as_str()) {
        q.to_string()
    } else if let Some(path) = cfg.get("sql_file").and_then(|v| v.as_str()) {
        let rendered = render_jinja_string(path, render_context)
            .map_err(|e| format!("render sql_file path {path:?}: {e}"))?;
        load_sql_body(workspace, &rendered).await?
    } else {
        return Err("execute_sql: need 'sql_query' or 'sql_file'".into());
    };

    let sql =
        render_jinja_string(&raw_sql, &sql_context).map_err(|e| format!("render SQL body: {e}"))?;

    let connector = workspace.get_connector(database).await?;
    let exec_result = connector
        .execute_query(&sql, DEFAULT_SAMPLE_LIMIT)
        .await
        .map_err(|e| format!("SQL execution failed: {e}"))?;

    Ok(attach_sql(query_result_to_json(&exec_result.result), &sql))
}

/// Resolve a `sql_file` ref to its SQL body: compile boundary first, working
/// copy only on a genuine miss.
///
/// Standalone rather than inline in [`execute_sql`] for the same reason
/// `agentic_pipeline::pipeline_ref::load_pipeline_yaml` is — the ordering below
/// is a property worth asserting, and asserting it should not require a
/// connector, a render context, or a database.
///
/// The order is the whole contract:
///
/// 1. **Containment + normalisation, before either backend.** A rendered path
///    can carry untrusted upstream data (a SQL row value substituted via
///    Jinja), so `../` is rejected before the ref is used as a lookup key, not
///    merely before it is used as a filesystem path. Normalising here is what
///    makes `./sql/x.sql` and `sql/x.sql` address the same compiled row.
/// 2. **`Ok(Some)` → done**, having touched no filesystem. This is what lets a
///    worker with no `/workspace` volume run an `execute_sql` step at all.
/// 3. **`Err` → propagate, never fall through.** The host could not be *asked*,
///    which is not "not compiled here". Falling through would let a working
///    copy that happens to exist answer a question the boundary owns.
/// 4. **`Ok(None)` → the working copy**, and only if this node actually has
///    one. `workspace_path()` is `Some` on a worker because the manager
///    DECLARES a working copy while the volume is absent, so the `is_dir()`
///    filter is what turns a raw ENOENT into a legible, deferrable message.
async fn load_sql_body(workspace: &dyn WorkspaceContext, sql_ref: &str) -> Result<String, String> {
    let sql_ref = normalize_workspace_relative_ref(sql_ref)
        .map_err(|e| format!("sql_file {sql_ref:?}: {e}"))?;

    match workspace.resolve_sql_file(&sql_ref).await {
        Ok(Some(sql)) => Ok(sql),
        Err(e) => Err(e),
        Ok(None) => {
            let root = workspace
                .workspace_path()
                .filter(|p| p.is_dir())
                .ok_or_else(|| {
                    // Name the carve-outs. `oxy_compile`'s walker routes
                    // `modeling/**/*.sql` to Airform and `schemas/**/*.sql` to
                    // `SchemaMigration` — neither lands in `verified_queries`,
                    // so a step pointing at one always misses the boundary.
                    // Without this the failure reads as a broken node rather
                    // than "this subtree is not a verified query", and a
                    // gold-layer rollup is exactly the sort of file someone
                    // files under `modeling/`.
                    let hint = if sql_ref.starts_with("modeling/") {
                        " (paths under `modeling/` are Airform models, not verified queries, so \
                         they are never served from the compile boundary)"
                    } else if sql_ref.starts_with("schemas/") {
                        " (paths under `schemas/` compile as OLTP schema migrations, not verified \
                         queries, so they are never served from the compile boundary)"
                    } else {
                        ""
                    };
                    format!(
                        "sql_file {sql_ref:?}: not served from the compile boundary and this node \
                         holds no workspace files{hint}"
                    )
                })?;
            let full_path = validate_workspace_relative_path(root, &sql_ref)
                .map_err(|e| format!("sql_file {sql_ref:?}: {e}"))?;
            tokio::fs::read_to_string(&full_path)
                .await
                .map_err(|e| format!("failed to read SQL file {}: {e}", full_path.display()))
        }
    }
}

/// Build the SQL renderer context by rendering each entry of the task's
/// `variables` map against `render_context` and then merging the result
/// into a flat object on top of `render_context`. Variables override
/// existing keys (matching the legacy precedence).
fn merge_sql_variables(render_context: &Value, variables: Option<&Value>) -> Result<Value, String> {
    let mut merged = render_context.clone();
    let Some(map) = variables.and_then(|v| v.as_object()) else {
        return Ok(merged);
    };
    let merged_map = merged
        .as_object_mut()
        .ok_or("execute_sql: render_context must be a JSON object")?;
    for (k, v) in map {
        let resolved = match v.as_str() {
            Some(s) => Value::String(
                render_jinja_string(s, render_context)
                    .map_err(|e| format!("render variable {k:?}: {e}"))?,
            ),
            None => v.clone(),
        };
        merged_map.insert(k.clone(), resolved);
    }
    Ok(merged)
}

/// Compile a semantic query via airlayer and execute the resulting SQL.
async fn execute_semantic_query(
    workspace: &dyn WorkspaceContext,
    cfg: &Value,
) -> Result<Value, String> {
    let query_config: SemanticQueryConfig = serde_json::from_value(cfg.clone())
        .map_err(|e| format!("failed to parse semantic query config: {e}"))?;

    // `context_root()`, not `workspace_path()` — the BACKLOG this replaces, and
    // the same defect `sql_file` had one function up.
    //
    // `workspace_path()` returns `Some` on a worker because the manager DECLARES
    // a working copy while the volume is absent, so the old `ok_or_else` guard
    // passed and the scan below ran against a directory that is not there. That
    // is worse than the `sql_file` case it mirrors: an absent directory yields
    // ZERO views rather than an error, so the step did not fail — it compiled a
    // query against an empty catalog and returned a wrong answer, or died later
    // with a shapeless "no databases configured".
    //
    // `context_root()` already does the right thing per role: on `Serve` /
    // `Worker` it materialises the promoted revision's semantic views, topics,
    // automations and verified `.sql` into a tempdir (`materialise_agent_context`)
    // and hands back that path; on `Ide` / `All` it hands back the working copy
    // unchanged. Binding it to a local is load-bearing — `ContextRoot` owns the
    // `TempDir` guard, so letting it drop before the scan would delete the very
    // directory being scanned.
    let context_root = workspace.context_root().await;
    let scan_path = context_root.path();
    if !scan_path.is_dir() {
        return Err(
            "semantic_query: no semantic context on this node — the workspace has no promoted \
             revision to materialise and no working copy to read"
                .to_string(),
        );
    }
    let databases = workspace.database_configs();
    let preagg = workspace.preagg_context();
    // Every automation step compiling a semantic query used to rebuild the
    // engine — a full re-read of the semantic directory plus a join-graph
    // build, per step, per run.
    let compiled = match workspace.semantic_engine_cache() {
        Some((cache, workspace_id)) => {
            // Key by the source that was actually SCANNED. That used to be
            // unconditionally the working copy; since the scan now runs against
            // `context_root()`, it is a materialised revision on a worker and
            // the working copy on an ide. `ContextRoot::revision()` reports
            // which, and `for_source` turns it into the matching key.
            //
            // Keeping `working_copy` here would have been a staleness bug, not
            // a crash: every revision would collide on one cache entry, so a
            // promote would appear not to take effect on the fleet.
            let key = oxy_airlayer_compat::EngineKey::for_source(
                workspace_id,
                context_root.revision(),
                &databases,
            );
            crate::semantic::resolve_and_compile_cached(
                &cache,
                key,
                scan_path,
                &databases,
                &query_config,
                preagg.as_ref(),
                None,
            )
        }
        None => crate::semantic::resolve_and_compile(
            scan_path,
            &databases,
            &query_config,
            preagg.as_ref(),
            None,
        ),
    }
    .map_err(|e| format!("semantic compilation failed: {e}"))?;

    match compiled {
        crate::semantic::CompiledQuery::Warehouse { sql, database_name } => {
            let connector = workspace.get_connector(&database_name).await?;
            let exec_result = connector
                .execute_query(&sql, DEFAULT_SAMPLE_LIMIT)
                .await
                .map_err(|e| format!("semantic query execution failed: {e}"))?;
            let mut result = attach_sql(query_result_to_json(&exec_result.result), &sql);
            result["is_preagg"] = serde_json::json!(false);
            Ok(result)
        }
        crate::semantic::CompiledQuery::Preaggregation {
            preagg_sql, source, ..
        } => {
            let sql_for_exec = preagg_sql.clone();
            let source_for_exec = source.clone();
            let mut result = tokio::task::spawn_blocking(move || {
                crate::preagg::execute_preagg_sql(&sql_for_exec, &source_for_exec)
            })
            .await
            .map_err(|e| format!("local Parquet execution task panicked: {e}"))?
            .map_err(|e| format!("local Parquet execution failed: {e}"))?;
            result = attach_sql(result, &preagg_sql);
            result["is_preagg"] = serde_json::json!(true);
            Ok(result)
        }
    }
}

/// Add the executed SQL string to a query-result JSON object so the export
/// wrapper can write it out for `format: sql`. Non-object results are
/// returned unchanged (defensive — every caller currently produces an
/// object, but a future variant might not).
fn attach_sql(result: Value, sql: &str) -> Value {
    match result {
        Value::Object(mut map) => {
            map.insert("sql".to_string(), Value::String(sql.to_string()));
            Value::Object(map)
        }
        other => other,
    }
}

/// Convert a `QueryResult` to the JSON format expected by downstream steps.
fn query_result_to_json(result: &agentic_core::result::QueryResult) -> Value {
    let rows: Vec<Value> = result
        .rows
        .iter()
        .map(|row| {
            let cells: Vec<Value> = row
                .0
                .iter()
                .map(|cell| match cell {
                    CellValue::Text(s) => Value::String(s.clone()),
                    CellValue::Number(n) => serde_json::Number::from_f64(*n)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                    CellValue::Null => Value::Null,
                })
                .collect();
            Value::Array(cells)
        })
        .collect();

    json!({
        "columns": result.columns,
        "rows": rows,
        "row_count": result.total_row_count,
        "truncated": result.truncated,
    })
}

/// Execute an Omni query via the standalone API client.
async fn execute_omni_query(
    workspace: &dyn WorkspaceContext,
    cfg: &Value,
) -> Result<Value, String> {
    let integration = cfg
        .get("integration")
        .and_then(|v| v.as_str())
        .ok_or("omni_query: missing 'integration' field")?;
    let topic = cfg
        .get("topic")
        .and_then(|v| v.as_str())
        .ok_or("omni_query: missing 'topic' field")?;

    let fields: Vec<String> = cfg
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if fields.is_empty() {
        return Err("omni_query: at least one field is required".into());
    }

    let limit = cfg.get("limit").and_then(|v| v.as_u64()).map(|l| l as u32);

    // Get integration credentials.
    let config = workspace.get_integration(integration).await?;
    let crate::workspace::IntegrationConfig::Omni { base_url, api_key } = config else {
        return Err(format!(
            "integration '{integration}' is not an Omni integration"
        ));
    };

    let client = omni::OmniApiClient::new(base_url, api_key)
        .map_err(|e| format!("omni client error: {e}"))?;

    let mut query_builder = omni::QueryStructure::builder()
        .topic(topic)
        .fields(fields.clone());
    if let Some(l) = limit {
        query_builder = query_builder.limit(l);
    }
    let query_structure = query_builder
        .build()
        .map_err(|e| format!("omni query build error: {e}"))?;

    let request = omni::QueryRequest::builder()
        .query(query_structure)
        .build()
        .map_err(|e| format!("omni request build error: {e}"))?;

    let response = client
        .execute_query(request)
        .await
        .map_err(|e| format!("omni query failed: {e}"))?;

    // Omni returns base64-encoded Arrow IPC in `result`. Extract available
    // metadata from the summary and return the generated SQL if available,
    // letting downstream consumers know what was queried.
    let sql = response
        .summary
        .as_ref()
        .and_then(|s| s.display_sql.clone())
        .unwrap_or_default();

    let field_names: Vec<String> = response
        .summary
        .as_ref()
        .and_then(|s| s.fields.as_ref())
        .map(|f| f.keys().cloned().collect())
        .unwrap_or_else(|| fields.clone());

    // Return metadata — full Arrow decoding would require the `arrow` crate.
    // The SQL can be executed via a database connector for full data access.
    Ok(json!({
        "columns": field_names,
        "rows": [],
        "row_count": 0,
        "sql": sql,
        "text": format!("Omni query executed against topic '{}'. SQL: {}", topic, sql),
    }))
}

/// Execute a Looker query via the standalone API client.
async fn execute_looker_query(
    workspace: &dyn WorkspaceContext,
    cfg: &Value,
) -> Result<Value, String> {
    let integration = cfg
        .get("integration")
        .and_then(|v| v.as_str())
        .ok_or("looker_query: missing 'integration' field")?;
    let model = cfg
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or("looker_query: missing 'model' field")?;
    let explore = cfg
        .get("explore")
        .and_then(|v| v.as_str())
        .ok_or("looker_query: missing 'explore' field")?;

    let fields: Vec<String> = cfg
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    if fields.is_empty() {
        return Err("looker_query: at least one field is required".into());
    }

    let limit = cfg.get("limit").and_then(|v| v.as_i64());

    // Get integration credentials.
    let config = workspace.get_integration(integration).await?;
    let crate::workspace::IntegrationConfig::Looker {
        base_url,
        client_id,
        client_secret,
    } = config
    else {
        return Err(format!(
            "integration '{integration}' is not a Looker integration"
        ));
    };

    let auth_config = oxy_looker::LookerAuthConfig {
        base_url,
        client_id,
        client_secret,
    };
    let mut client = oxy_looker::LookerApiClient::new(auth_config)
        .map_err(|e| format!("looker client error: {e}"))?;

    let request = oxy_looker::InlineQueryRequest {
        model: model.to_string(),
        view: explore.to_string(),
        fields: fields.clone(),
        limit: limit.or(Some(10_000)),
        filters: None,
        filter_expression: None,
        sorts: None,
        query_timezone: None,
        pivots: None,
        fill_fields: None,
    };

    let response = client
        .run_inline_query(request)
        .await
        .map_err(|e| format!("looker query failed: {e}"))?;

    // Convert response data (Vec<HashMap<String, Value>>) to columns + rows.
    let columns: Vec<String> = fields;
    let rows: Vec<Value> = response
        .data
        .iter()
        .map(|row| {
            let cells: Vec<Value> = columns
                .iter()
                .map(|col| row.get(col).cloned().unwrap_or(Value::Null))
                .collect();
            Value::Array(cells)
        })
        .collect();

    let row_count = rows.len() as u64;
    Ok(json!({
        "columns": columns,
        "rows": rows,
        "row_count": row_count,
        "truncated": false,
    }))
}

/// Extract step results from a JSON output into a normalized format.
pub fn extract_automation_steps(output: &Value) -> Vec<Value> {
    match output {
        Value::Array(arr) => arr.clone(),
        Value::Object(map) => map
            .iter()
            .filter(|(_, v)| !v.is_null())
            .take(20)
            .map(|(name, value)| {
                json!({
                    "step_name": name,
                    "text": value.to_string(),
                })
            })
            .collect(),
        other => vec![json!({
            "step_name": "result",
            "text": other.to_string(),
        })],
    }
}

/// Execute an `http_request` task: make an outbound HTTP call from a workflow.
///
/// Fields (read from the opaque task `cfg`):
/// - `url` (templated), `method` (default POST), `headers` (templated values),
/// - `body` (templated raw) OR `form` (templated map → urlencoded),
/// - `secrets: [NAME]` resolved into the `{{ secrets.NAME }}` template scope,
/// - `timeout_secs` (1–120, default 30), `expected_status` (default any 2xx),
/// - `allow_hosts` (extra egress allowlist), `persist_to_secret { from, name }`.
///
/// Returns `{ status, json, text }`. Secrets are injected into the request but
/// never echoed in the output. The rotating-OAuth-token path is `persist_to_secret`,
/// which writes a JSON-pointer field of the response back to a project secret.
async fn execute_http_request(
    workspace: &dyn WorkspaceContext,
    cfg: &Value,
    render_context: &Value,
) -> Result<Value, String> {
    let url_tmpl = cfg
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or("http_request: missing 'url'")?;
    let method = cfg
        .get("method")
        .and_then(|v| v.as_str())
        .unwrap_or("POST")
        .to_ascii_uppercase();
    let timeout_secs = cfg
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(30)
        .clamp(1, 120);

    // Resolve declared secrets into a `{{ secrets.NAME }}` scope layered onto the
    // render context. Missing secrets fail loudly rather than rendering empty.
    let mut secrets_map = serde_json::Map::new();
    if let Some(names) = cfg.get("secrets").and_then(|v| v.as_array()) {
        for name in names.iter().filter_map(|v| v.as_str()) {
            match workspace.fetch_secret(name).await {
                Some(val) => {
                    secrets_map.insert(name.to_string(), Value::String(val));
                }
                None => return Err(format!("http_request: secret {name:?} not found")),
            }
        }
    }
    let mut ctx = render_context.clone();
    if let Some(obj) = ctx.as_object_mut() {
        obj.insert("secrets".to_string(), Value::Object(secrets_map));
    } else {
        ctx = json!({ "secrets": Value::Object(secrets_map) });
    }

    let url = render_jinja_string(url_tmpl, &ctx).map_err(|e| format!("render url: {e}"))?;

    let allow_hosts: Vec<String> = cfg
        .get("allow_hosts")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    validate_egress(&url, &allow_hosts)?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        // Do NOT follow redirects: `validate_egress` only vets the initial URL, so a
        // 3xx to a private IP or a non-`allow_hosts` host would slip past the egress
        // guard — and this task ships declared secrets to the endpoint.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| format!("http_request: client build failed: {e}"))?;
    let method_parsed = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| format!("http_request: invalid method {method:?}"))?;
    let mut req = client.request(method_parsed, &url);

    if let Some(headers) = cfg.get("headers").and_then(|v| v.as_object()) {
        for (k, v) in headers {
            if let Some(val_tmpl) = v.as_str() {
                let val = render_jinja_string(val_tmpl, &ctx)
                    .map_err(|e| format!("render header {k}: {e}"))?;
                req = req.header(k, val);
            }
        }
    }

    // `form` (urlencoded) takes precedence over a raw `body`.
    if let Some(form) = cfg.get("form").and_then(|v| v.as_object()) {
        let mut pairs: Vec<(String, String)> = Vec::with_capacity(form.len());
        for (k, v) in form {
            if let Some(val_tmpl) = v.as_str() {
                let val = render_jinja_string(val_tmpl, &ctx)
                    .map_err(|e| format!("render form {k}: {e}"))?;
                pairs.push((k.clone(), val));
            }
        }
        req = req.form(&pairs);
    } else if let Some(body_tmpl) = cfg.get("body").and_then(|v| v.as_str()) {
        let body = render_jinja_string(body_tmpl, &ctx).map_err(|e| format!("render body: {e}"))?;
        req = req.body(body);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("http_request: send failed: {e}"))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| format!("http_request: failed to read response body: {e}"))?;

    let expected: Vec<u16> = cfg
        .get("expected_status")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_u64().and_then(|n| u16::try_from(n).ok()))
                .collect()
        })
        .unwrap_or_default();
    let ok = if expected.is_empty() {
        (200..300).contains(&status)
    } else {
        expected.contains(&status)
    };
    if !ok {
        let snippet: String = text.chars().take(500).collect();
        return Err(format!(
            "http_request: unexpected status {status}: {snippet}"
        ));
    }

    // Parse JSON if the body is JSON; otherwise `json` stays null and `text` carries it.
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);

    // Rotating-secret write-back (e.g. the OAuth refresh_token).
    if let Some(pts) = cfg.get("persist_to_secret").and_then(|v| v.as_object()) {
        let from = pts
            .get("from")
            .and_then(|v| v.as_str())
            .ok_or("http_request: persist_to_secret missing 'from'")?;
        let name = pts
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or("http_request: persist_to_secret missing 'name'")?;
        let extracted = json.pointer(from).and_then(|v| v.as_str()).ok_or_else(|| {
            format!("http_request: persist_to_secret found no string at {from:?}")
        })?;
        workspace
            .store_secret(name, extracted)
            .await
            .map_err(|e| format!("http_request: persist_to_secret failed: {e}"))?;
    }

    Ok(json!({ "status": status, "json": json, "text": text }))
}

/// SSRF guard for `http_request`. Requires HTTPS, denies localhost / cloud
/// metadata hostnames and private/loopback/link-local IP literals, and — when
/// `allow_hosts` is non-empty — requires the URL host to be in it.
fn validate_egress(url_str: &str, allow_hosts: &[String]) -> Result<(), String> {
    let parsed = url::Url::parse(url_str)
        .map_err(|e| format!("http_request: invalid url {url_str:?}: {e}"))?;
    if parsed.scheme() != "https" {
        return Err(format!(
            "http_request: only https is allowed (got scheme {:?})",
            parsed.scheme()
        ));
    }
    let host = parsed
        .host_str()
        .ok_or("http_request: url has no host")?
        .to_ascii_lowercase();

    if host == "localhost" || host.ends_with(".localhost") || host == "metadata.google.internal" {
        return Err(format!("http_request: host {host:?} is not allowed"));
    }
    // IP-LITERAL guard only. A hostname that RESOLVES to a private IP (DNS
    // rebinding) is NOT caught here — the parse below fails for a non-literal
    // host, so `allow_hosts` is the real security boundary for hostname targets
    // and callers sending secrets should always set it. Fully closing the
    // rebinding gap needs a resolving connector that validates every resolved IP
    // (follow-up); disabling redirects above removes the redirect-to-private vector.
    //
    // `host_str()` serializes an IPv6 literal with brackets (`[::1]`); strip them
    // so it parses as an IpAddr.
    let ip_str = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
        let blocked = match ip {
            std::net::IpAddr::V4(v4) => ipv4_blocked(&v4),
            // Fold an IPv4-mapped literal (`::ffff:a.b.c.d`) down to its V4 form so
            // e.g. `[::ffff:169.254.169.254]` can't tunnel past the V4 ranges; for a
            // genuine V6 address also reject ULA (fc00::/7) and link-local (fe80::/10).
            std::net::IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => ipv4_blocked(&v4),
                None => {
                    v6.is_loopback()
                        || v6.is_unspecified()
                        || v6.is_unique_local()
                        || v6.is_unicast_link_local()
                }
            },
        };
        if blocked {
            return Err(format!(
                "http_request: target IP {ip} is in a blocked range"
            ));
        }
    }
    if !allow_hosts.is_empty() && !allow_hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Err(format!(
            "http_request: host {host:?} is not in allow_hosts {allow_hosts:?}"
        ));
    }
    Ok(())
}

/// IPv4 ranges we never allow outbound: private, loopback, link-local (incl.
/// 169.254.169.254 cloud-metadata), unspecified, and broadcast. Shared by the
/// direct-V4 path and the IPv4-mapped-V6 path so neither can bypass the other.
fn ipv4_blocked(v4: &std::net::Ipv4Addr) -> bool {
    v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
}

#[cfg(test)]
mod sql_file_tests {
    use super::load_sql_body;
    use crate::workspace::WorkspaceContext;
    use async_trait::async_trait;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    /// A host that records what it was asked for, so a test can assert the
    /// filesystem was never consulted rather than only that the result looked
    /// right.
    struct FakeHost {
        answer: Result<Option<String>, String>,
        /// Reported as this node's working copy. `None` models a worker.
        root: Option<PathBuf>,
        asked: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl WorkspaceContext for FakeHost {
        fn workspace_path(&self) -> Option<&Path> {
            self.root.as_deref()
        }
        fn database_configs(&self) -> Vec<oxy_airlayer_compat::DatabaseConfig> {
            vec![]
        }
        async fn list_automation_files(&self) -> Result<Vec<PathBuf>, String> {
            Ok(vec![])
        }
        async fn resolve_automation_yaml(
            &self,
            _r: &str,
        ) -> Result<String, crate::WorkspaceReadError> {
            unreachable!("not exercised by these tests")
        }
        async fn get_connector(
            &self,
            _name: &str,
        ) -> Result<Arc<dyn agentic_connector::DatabaseConnector>, String> {
            // These tests stop at the SQL body; nothing here executes a query.
            unreachable!("not exercised by these tests")
        }
        async fn get_integration(
            &self,
            _name: &str,
        ) -> Result<crate::workspace::IntegrationConfig, String> {
            unreachable!("not exercised by these tests")
        }
        async fn resolve_sql_file(&self, sql_ref: &str) -> Result<Option<String>, String> {
            self.asked.lock().unwrap().push(sql_ref.to_string());
            self.answer.clone()
        }
    }

    fn host(answer: Result<Option<String>, String>, root: Option<PathBuf>) -> FakeHost {
        FakeHost {
            answer,
            root,
            asked: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The happy path that makes a volume-less worker viable at all: served
    /// from Postgres, with no working copy anywhere in sight.
    #[tokio::test]
    async fn boundary_hit_is_used_without_a_working_copy() {
        let h = host(Ok(Some("SELECT 1".into())), None);
        assert_eq!(
            load_sql_body(&h, "sql/rollup.sql").await.unwrap(),
            "SELECT 1"
        );
    }

    /// `Err` must NOT fall through to the filesystem. Modelled with a root that
    /// really exists and holds a DIFFERENT body — so if the ordering ever
    /// regressed to "try the FS on error", this test would see the disk copy
    /// instead of the host's error.
    #[tokio::test]
    async fn a_boundary_error_never_falls_through_to_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sql")).unwrap();
        std::fs::write(dir.path().join("sql/rollup.sql"), "SELECT 'from disk'").unwrap();

        let h = host(Err("pool timed out".into()), Some(dir.path().to_path_buf()));
        let err = load_sql_body(&h, "sql/rollup.sql").await.unwrap_err();
        assert!(err.contains("pool timed out"), "got {err}");
        assert!(
            !err.contains("from disk"),
            "the on-disk copy must not have answered: {err}"
        );
    }

    /// The #3056 failure in miniature: a miss on a node with no working copy
    /// must say so legibly rather than surface as a raw ENOENT.
    #[tokio::test]
    async fn a_miss_with_no_working_copy_is_legible() {
        let h = host(Ok(None), None);
        let err = load_sql_body(&h, "sql/rollup.sql").await.unwrap_err();
        assert!(
            err.contains("holds no workspace files"),
            "expected the node-shaped message, got {err}"
        );
        assert!(
            !err.contains("No such file"),
            "must not degrade to a raw ENOENT: {err}"
        );
    }

    /// `workspace_path()` is `Some` on a worker because the manager DECLARES a
    /// working copy while the volume is absent. The `is_dir()` filter is what
    /// separates "declared" from "present" — without it, this case is the
    /// ENOENT that took the pipeline down.
    #[tokio::test]
    async fn a_declared_but_absent_working_copy_is_treated_as_absent() {
        let h = host(Ok(None), Some(PathBuf::from("/definitely/not/here")));
        let err = load_sql_body(&h, "sql/rollup.sql").await.unwrap_err();
        assert!(
            err.contains("holds no workspace files"),
            "expected the node-shaped message, got {err}"
        );
    }

    /// A `./`-prefixed ref must reach the boundary under the walker's spelling.
    /// Before normalisation this missed a row that existed and fell back to the
    /// filesystem — the instance affinity this path removes, reachable by a
    /// spelling.
    #[tokio::test]
    async fn a_dot_slash_ref_is_normalised_before_the_lookup() {
        let h = host(Ok(Some("SELECT 1".into())), None);
        load_sql_body(&h, "./sql/./rollup.sql").await.unwrap();
        assert_eq!(h.asked.lock().unwrap().as_slice(), &["sql/rollup.sql"]);
    }

    /// Containment runs before either backend, so a traversal never becomes a
    /// lookup key.
    #[tokio::test]
    async fn traversal_is_rejected_before_the_boundary_is_asked() {
        let h = host(Ok(Some("SELECT 1".into())), None);
        let err = load_sql_body(&h, "../../etc/passwd").await.unwrap_err();
        assert!(err.contains(".."), "got {err}");
        assert!(
            h.asked.lock().unwrap().is_empty(),
            "the boundary must not be asked for an escaping ref"
        );
    }

    /// `modeling/` and `schemas/` `.sql` never compile as verified queries, so
    /// the miss is about the subtree, not about the node.
    #[tokio::test]
    async fn carve_out_paths_say_why_they_can_never_be_served() {
        for (path, needle) in [
            ("modeling/proj/models/x.sql", "Airform"),
            ("schemas/001_init.sql", "schema migrations"),
        ] {
            let h = host(Ok(None), None);
            let err = load_sql_body(&h, path).await.unwrap_err();
            assert!(err.contains(needle), "for {path}: {err}");
        }
    }
}

#[cfg(test)]
mod http_request_tests {
    use super::validate_egress;

    #[test]
    fn egress_requires_https() {
        assert!(validate_egress("http://quickbooks.api.intuit.com/x", &[]).is_err());
        assert!(validate_egress("https://quickbooks.api.intuit.com/x", &[]).is_ok());
    }

    #[test]
    fn egress_denies_localhost_and_metadata() {
        assert!(validate_egress("https://localhost/x", &[]).is_err());
        assert!(validate_egress("https://foo.localhost/x", &[]).is_err());
        assert!(validate_egress("https://metadata.google.internal/x", &[]).is_err());
    }

    #[test]
    fn egress_denies_private_and_loopback_ips() {
        for u in [
            "https://127.0.0.1/x",
            "https://10.0.0.5/x",
            "https://192.168.1.1/x",
            "https://172.16.0.1/x",
            "https://169.254.169.254/x", // cloud metadata
            "https://[::1]/x",
        ] {
            assert!(validate_egress(u, &[]).is_err(), "should deny {u}");
        }
    }

    #[test]
    fn egress_denies_ipv6_mapped_and_scoped() {
        for u in [
            "https://[::ffff:169.254.169.254]/x", // IPv4-mapped cloud metadata
            "https://[::ffff:10.0.0.5]/x",        // IPv4-mapped private
            "https://[fd00::1]/x",                // unique-local (fc00::/7)
            "https://[fe80::1]/x",                // link-local (fe80::/10)
        ] {
            assert!(validate_egress(u, &[]).is_err(), "should deny {u}");
        }
    }

    #[test]
    fn egress_enforces_allow_hosts() {
        let allow = vec!["quickbooks.api.intuit.com".to_string()];
        assert!(validate_egress("https://quickbooks.api.intuit.com/v3/x", &allow).is_ok());
        assert!(validate_egress("https://evil.example.com/x", &allow).is_err());
    }
}
