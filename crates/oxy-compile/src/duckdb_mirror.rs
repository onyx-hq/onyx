//! Compile-time S3 mirror for **local-file** DuckDB warehouses.
//!
//! `Local` / `File` DuckDB keeps its data in the workspace working tree, which
//! the stateless serve fleet doesn't have — so a compiled workspace whose
//! warehouse is local DuckDB can't be queried on the fleet. At compile time we
//! DO have the working copy, so we upload that data to S3 (content-addressed,
//! so unchanged files aren't re-sent) and record an `s3_mirror` block in the
//! compiled config. The runtime DuckDB connector reads from S3 via `httpfs`
//! when it sees that block. Local/IDE reads come from `config.yml` (no
//! `s3_mirror`), so they're unaffected.
//!
//! Gated on `OXY_COMPILE_BLOB_S3_BUCKET`; a no-op without it. Credentials are
//! the pod's instance role (S3 `credential_chain`), so nothing sensitive is
//! stored. **Interim bridge** — isolated in this one module so it can be
//! replaced by an Airhouse-backed approach later without touching the compiler.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use slugify::slugify;
use uuid::Uuid;

use crate::blob_store;

/// Per-file size ceiling. Above this, mirroring is skipped with a warning —
/// the interim bridge reads the whole file into memory to hash + upload, and a
/// multi-GB local DuckDB warehouse isn't a good fit for it anyway.
const MAX_MIRROR_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Mirror every local-file DuckDB database in `databases` (the compiled
/// config's JSON array) to S3, injecting an `s3_mirror` block into each entry.
/// Best-effort per database: a failure logs and leaves that one un-mirrored
/// (it simply won't be fleet-servable). No-op when no bucket is configured.
pub async fn mirror_duckdb_databases(
    workspace_path: &Path,
    workspace_id: Uuid,
    databases: &mut Value,
) {
    let Some(bucket) = blob_store::bucket() else {
        return;
    };
    let Some(entries) = databases.as_array_mut() else {
        return;
    };

    for db in entries.iter_mut() {
        if db.get("type").and_then(Value::as_str) != Some("duckdb") {
            continue;
        }
        let db_name = db
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("duckdb")
            .to_string();
        // Clone the path strings so the immutable borrow ends before the awaits
        // + the mutable insert below.
        let dataset = db.get("dataset").and_then(Value::as_str).map(String::from);
        let file_path = db.get("path").and_then(Value::as_str).map(String::from);

        let mirror = match (dataset, file_path) {
            (Some(dataset), _) if !looks_remote(&dataset) => {
                mirror_local_dir(workspace_path, &dataset, workspace_id, &db_name, &bucket).await
            }
            (_, Some(path)) if !looks_remote(&path) => {
                mirror_file(workspace_path, &path, workspace_id, &db_name, &bucket).await
            }
            // DuckLake / MotherDuck / already-remote paths need no mirror.
            _ => None,
        };

        if let Some(mirror) = mirror
            && let Some(obj) = db.as_object_mut()
        {
            obj.insert("s3_mirror".to_string(), mirror);
        }
    }
}

/// A path we shouldn't try to read off the local disk (already a URL or an
/// absolute system path the connector handles directly).
fn looks_remote(path: &str) -> bool {
    path.contains("://")
}

/// `Local` mode: mirror each CSV/Parquet in the dataset directory as a table.
/// Matches the local connector's behaviour — table name = slugified file stem,
/// `.parquet` wins on a stem collision.
async fn mirror_local_dir(
    workspace_path: &Path,
    dataset: &str,
    workspace_id: Uuid,
    db_name: &str,
    bucket: &str,
) -> Option<Value> {
    let dir = workspace_path.join(dataset);
    let read_dir = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        // A workspace with no local DuckDB dataset simply has no directory, and
        // that was a `WARN` on every compile of every such workspace. Only a
        // directory that exists and still cannot be read is worth a warning.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(dir = %dir.display(), "duckdb mirror: no dataset dir; skipping");
            return None;
        }
        Err(e) => {
            tracing::warn!(?e, dir = %dir.display(), "duckdb mirror: cannot read dataset dir; skipping");
            return None;
        }
    };

    // stem(slug) → (path, format), preferring parquet over csv on collision.
    let mut chosen: BTreeMap<String, (PathBuf, &'static str)> = BTreeMap::new();
    for entry in read_dir.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let format = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("parquet") => "parquet",
            Some("csv") => "csv",
            _ => continue,
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("table")
            .to_string();
        let table = slugify!(&stem, separator = "_");
        match chosen.get(&table) {
            Some((_, "parquet")) => {} // keep the parquet we already have
            _ => {
                chosen.insert(table, (path, format));
            }
        }
    }

    let mut tables = Vec::new();
    for (table, (path, format)) in chosen {
        if let Some(key) = upload_data_file(&path, workspace_id, db_name).await {
            tables.push(json!({ "table": table, "key": key, "format": format }));
        }
    }
    if tables.is_empty() {
        return None;
    }
    Some(build_manifest(bucket, tables, None))
}

/// `File` mode: mirror the `.duckdb` file; the connector attaches it read-only.
async fn mirror_file(
    workspace_path: &Path,
    rel_path: &str,
    workspace_id: Uuid,
    db_name: &str,
    bucket: &str,
) -> Option<Value> {
    let path = workspace_path.join(rel_path);
    let key = upload_data_file(&path, workspace_id, db_name).await?;
    Some(build_manifest(bucket, Vec::new(), Some(key)))
}

/// Read, hash, and upload a single data file under a content-addressed key
/// (`workspaces/{id}/duckdb/{db}/{hash}-{name}`). Skips the upload when the key
/// already exists (unchanged content). Returns the key.
async fn upload_data_file(path: &Path, workspace_id: Uuid, db_name: &str) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_MIRROR_FILE_BYTES {
        tracing::warn!(
            file = %path.display(),
            bytes = meta.len(),
            "duckdb mirror: file exceeds size cap; skipping (not fleet-servable)"
        );
        return None;
    }
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(?e, file = %path.display(), "duckdb mirror: read failed; skipping");
            return None;
        }
    };
    let hash = {
        let mut h = Sha256::new();
        h.update(&body);
        h.finalize()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    let fname = path.file_name().and_then(|f| f.to_str()).unwrap_or("data");
    let key = format!("workspaces/{workspace_id}/duckdb/{db_name}/{hash}-{fname}");

    match blob_store::object_exists(&key).await {
        Ok(true) => return Some(key), // already mirrored — unchanged content
        Ok(false) => {}
        Err(e) => tracing::warn!(
            ?e,
            "duckdb mirror: head check failed; attempting upload anyway"
        ),
    }

    let content_type = if fname.to_ascii_lowercase().ends_with(".csv") {
        "text/csv"
    } else {
        "application/octet-stream"
    };
    match blob_store::put_object_at_key(&key, body, content_type).await {
        Ok(Some(k)) => Some(k),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(?e, file = %path.display(), "duckdb mirror: upload failed; skipping");
            None
        }
    }
}

/// Build the `s3_mirror` JSON. Region/endpoint come from the same env the S3
/// client uses, so the connector's `CREATE SECRET` matches the bucket.
fn build_manifest(bucket: &str, tables: Vec<Value>, attach_key: Option<String>) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("bucket".to_string(), json!(bucket));
    if let Some(region) = std::env::var("AWS_REGION")
        .ok()
        .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
        .filter(|s| !s.is_empty())
    {
        m.insert("region".to_string(), json!(region));
    }
    if let Some(endpoint) = std::env::var("AWS_ENDPOINT_URL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        m.insert("endpoint_url".to_string(), json!(endpoint));
    }
    if !tables.is_empty() {
        m.insert("tables".to_string(), Value::Array(tables));
    }
    if let Some(key) = attach_key {
        m.insert("attach_key".to_string(), json!(key));
    }
    Value::Object(m)
}
