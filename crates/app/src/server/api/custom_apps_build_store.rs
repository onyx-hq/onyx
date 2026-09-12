//! Storage for versioned custom-app bundles.
//!
//! Each publish writes its files under
//! `customer-apps/<app_id>/builds/<build_id>/`. Two backends, selected at
//! call time by whether `OXY_CUSTOMER_APPS_S3_BUCKET` is set:
//!
//! - **S3** (cloud / configured): the single source of truth — oxy holds no
//!   local copy. The serve path reads objects back through an in-memory
//!   cache ([`super::custom_apps_bundle_cache`]).
//! - **Filesystem** (local dev, no bucket): files land under
//!   `<OXY_STATE_DIR>/customer-apps/<app_id>/builds/<build_id>/` so an
//!   engineer can `oxy serve` + `oxyc publish` locally without MinIO/S3.
//!
//! Either way the logical key is [`build_prefix`]; it's recorded verbatim in
//! `app_builds.s3_prefix` and interpreted as an S3 key or an FS subpath
//! depending on the active backend. The serve path goes through this module,
//! so local serve "just works" off the filesystem.

use std::path::PathBuf;

use aws_config::BehaviorVersion;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;
use axum::body::Bytes;
use futures::stream::{StreamExt, TryStreamExt};
use uuid::Uuid;

/// In-flight `put_object` calls during a publish. High enough that a
/// few-hundred-file bundle stops being latency-bound, low enough not to
/// exhaust the SDK's connection pool or trip S3 request-rate throttling.
const PUT_CONCURRENCY: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum BuildStoreError {
    #[error("s3 error: {0}")]
    S3(String),
    #[error("filesystem build-store error: {0}")]
    Io(String),
}

/// Logical prefix (no leading slash, trailing slash) that holds a given
/// build's files. The single canonical layout for the publish pipeline,
/// shared by both backends.
pub fn build_prefix(app_id: Uuid, build_id: &str) -> String {
    format!("customer-apps/{app_id}/builds/{build_id}/")
}

/// Configured S3 bucket, or `None` to select the filesystem backend.
fn bucket() -> Option<String> {
    std::env::var("OXY_CUSTOMER_APPS_S3_BUCKET")
        .ok()
        .filter(|b| !b.trim().is_empty())
}

/// Root of the local-mode build store: `OXY_STATE_DIR`, else the platform
/// data dir under `oxy`. Both `put`/`get`/`delete` resolve through here so
/// writes and reads always agree.
fn state_root() -> PathBuf {
    if let Ok(dir) = std::env::var("OXY_STATE_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .map(|d| d.join("oxy"))
        .unwrap_or_else(|| PathBuf::from(".oxy-state"))
}

/// Absolute on-disk directory for a build under the filesystem backend.
fn fs_build_dir(app_id: Uuid, build_id: &str) -> PathBuf {
    state_root().join(build_prefix(app_id, build_id))
}

/// Longest accepted `build_id`. Comfortably fits a sha plus a run id and
/// attempt (`ci_build_id`), while keeping the S3 key and the on-disk path
/// well inside any path-length limit.
const MAX_BUILD_ID_LEN: usize = 200;

/// Reject a `build_id` that cannot safely become a path segment.
///
/// The id is caller-supplied (`--build-id`, or the publish multipart field)
/// and flows into [`build_prefix`] → [`fs_build_dir`] → `state_root().join(..)`.
/// [`is_safe_rel`] guards the per-file `rel` but never the id segment, so
/// without this a `build_id` of `../../../../tmp/x` writes bundle files
/// outside the state dir on the filesystem backend, and `delete_build`'s
/// `remove_dir_all` targets the traversed directory. S3 keys are literal, so
/// that backend is contained either way — but the FS backend is what local
/// dev and single-node self-host run on.
///
/// The charset is deliberately narrower than "not traversal": an id is a
/// human-readable handle in the admin UI and a greppable prefix in the store,
/// so restricting it to `[A-Za-z0-9._-]` costs nothing real. A leading dot is
/// refused so an id can never produce a hidden directory.
pub fn is_valid_build_id(build_id: &str) -> bool {
    is_containable_build_id(build_id)
        && build_id.len() <= MAX_BUILD_ID_LEN
        && build_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// The weaker property [`is_valid_build_id`] is built on: this id cannot
/// escape its own prefix.
///
/// **Read and delete sides use this, not the strict check.** `--build-id` was
/// free text until [`is_valid_build_id`] existed, so `app_builds` rows in an
/// existing deployment can legitimately hold `release/v1`, a colon-bearing
/// `2026-08-07T10:00:00Z`, or `v1.0+build`. Refusing to *serve* those would
/// break already-published apps, and refusing to *delete* them is worse than
/// it sounds: `gc_builds` drops the row whether or not the store delete
/// succeeded, so a rejected reap orphans the prefix with nothing left pointing
/// at it — reclaimable only by `delete_app`'s whole-app sweep.
///
/// Containment is all those two paths need. The strict charset is a
/// forward-looking policy about what we'll *accept*, which is a different
/// question and belongs only on the write side.
///
/// Containment is **component-wise**, the same shape as [`is_safe_rel`] — an
/// interior separator is not a hazard. `release/v1` joins to
/// `<state>/customer-apps/<uuid>/builds/release/v1/`, which has no `ParentDir`
/// component and is not absolute, so it lands inside the state dir; on S3 the
/// key is literal and `build_prefix` already contains separators. Refusing it
/// would take the app dark (`get_object` → `Ok(None)` → SPA fallback also
/// missing → 404 on every request, with the absence cached) *and* strand the
/// prefix, which is the exact failure this split exists to prevent.
///
/// Known legacy edge, not introduced here and not closed here: two legacy ids
/// where one is a path prefix of the other (`x` and `x/y`) nest. Reaping `x`
/// takes `x/y`'s bytes with it, and the overlap runs through the read path
/// too — `rel` comes from the request URL, so on build `x` a crafted
/// `…/y/index.html` addresses build `x/y`'s file. Same app, so the auth gate
/// is identical and no tenant data crosses; it does let a visitor of the live
/// build reach an unpromoted draft's assets. Only reachable on rows that
/// predate [`is_valid_build_id`], which now makes the shape uncreatable.
pub fn is_containable_build_id(build_id: &str) -> bool {
    use std::path::Component;
    let path = std::path::Path::new(build_id);
    let mut components = path.components().peekable();
    if components.peek().is_none() {
        // Empty, or nothing but separators.
        return false;
    }
    components.all(|c| match c {
        // A leading dot on any component would make a hidden directory.
        Component::Normal(s) => !s.to_string_lossy().starts_with('.'),
        // `..`, `.`, `/`, and Windows prefixes all escape or anchor.
        _ => false,
    })
}

/// Reject relative paths that could escape the build dir on the filesystem
/// backend (`..` components or absolute). The tar extractor already guards
/// uploaded paths; this is defense-in-depth for both put and the serve-time
/// get, whose `rel_path` is derived from the request URL.
fn is_safe_rel(rel: &str) -> bool {
    let p = std::path::Path::new(rel);
    !p.is_absolute()
        && !p
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Build an S3 client, honoring `AWS_ENDPOINT_URL` (localstack/MinIO) by
/// forcing path-style addressing.
async fn s3_client() -> S3Client {
    let shared = aws_config::load_defaults(BehaviorVersion::latest()).await;
    let mut builder = aws_sdk_s3::config::Builder::from(&shared);
    if std::env::var("AWS_ENDPOINT_URL")
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        builder = builder.force_path_style(true);
    }
    S3Client::from_conf(builder.build())
}

/// Upload every `(relative_path, bytes)` pair under the build prefix.
/// Returns the prefix that was written.
pub async fn put_build(
    app_id: Uuid,
    build_id: &str,
    files: Vec<(String, Vec<u8>)>,
) -> Result<String, BuildStoreError> {
    // Guard the id segment itself, not just the per-file `rel` below — see
    // `is_valid_build_id`. Publish rejects a bad id with a 422 before reaching
    // here; this is the backstop for every other caller of the store.
    if !is_valid_build_id(build_id) {
        return Err(BuildStoreError::Io(format!(
            "unsafe build id: {build_id:?}"
        )));
    }
    let prefix = build_prefix(app_id, build_id);
    if let Some(bad) = files.iter().find(|(rel, _)| !is_safe_rel(rel)) {
        return Err(BuildStoreError::Io(format!("unsafe build path: {}", bad.0)));
    }
    // Multi-instance guard: the filesystem backend writes the bundle to THIS
    // node's local disk, but on a multi-replica deployment (OXY_ROLE !=
    // `all`) the serve path round-robins — a later asset request lands on a
    // replica whose local disk has no such build → blank page / 404. Refuse
    // the publish with a clear error instead of silently degrading to
    // per-node-local storage. Only affects deployments that actually use
    // custom apps; `all`-mode (local dev / single process) is unaffected.
    if bucket().is_none()
        && crate::server::role_manifest::current_process_role()
            != crate::server::role_manifest::Role::All
    {
        return Err(BuildStoreError::Io(
            "custom-app publish requires a shared object store on a multi-instance \
             deployment: set OXY_CUSTOMER_APPS_S3_BUCKET (the filesystem backend is \
             single-node only)"
                .to_string(),
        ));
    }
    match bucket() {
        Some(bucket) => {
            let client = s3_client().await;
            // Concurrent, not serial. A Vite bundle is a few hundred small
            // chunks and publish-time brotli adds a `.br` sibling for most of
            // them (`custom_apps_precompress`), so a serial loop pays a few
            // hundred round-trip latencies back to back — which dominates the
            // compression cost the siblings were meant to save. Each PUT is
            // independent (distinct keys under a prefix nothing reads until
            // the `app_builds` row lands), so ordering carries no meaning.
            //
            // `try_collect` short-circuits on the first error exactly as `?`
            // did, and the caller's rollback still deletes the whole prefix —
            // a partial upload leaves no more mess than before.
            futures::stream::iter(files.into_iter().map(|(rel, bytes)| {
                let client = &client;
                let bucket = &bucket;
                let prefix = &prefix;
                async move {
                    let key = format!("{prefix}{}", rel.trim_start_matches('/'));
                    client
                        .put_object()
                        .bucket(bucket)
                        .key(&key)
                        .body(ByteStream::from(bytes))
                        .send()
                        .await
                        .map_err(|e| BuildStoreError::S3(format!("put_object {key}: {e}")))?;
                    Ok::<(), BuildStoreError>(())
                }
            }))
            .buffer_unordered(PUT_CONCURRENCY)
            .try_collect::<Vec<()>>()
            .await?;
        }
        None => {
            let root = fs_build_dir(app_id, build_id);
            for (rel, bytes) in files {
                let dest = root.join(rel.trim_start_matches('/'));
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| {
                        BuildStoreError::Io(format!("mkdir {}: {e}", parent.display()))
                    })?;
                }
                tokio::fs::write(&dest, bytes)
                    .await
                    .map_err(|e| BuildStoreError::Io(format!("write {}: {e}", dest.display())))?;
            }
        }
    }
    Ok(prefix)
}

/// Fetch a single object from a build. `Ok(None)` when the key/file is
/// absent (so the serve path can fall back to the SPA index or 404).
pub async fn get_object(
    app_id: Uuid,
    build_id: &str,
    rel_path: &str,
) -> Result<Option<Bytes>, BuildStoreError> {
    let rel = rel_path.trim_start_matches('/');
    // Two segments, two guards: `rel` comes from the request URL, `build_id`
    // comes from an `app_builds` row — which for a row written before
    // `is_valid_build_id` existed was never checked at all, so the write-side
    // gate cannot cover it retroactively. On the FS backend that id lands in
    // `fs_build_dir(..).join(rel)` below, so a stored traversal id would read
    // outside the state dir. Containment, not the strict charset: a legacy id
    // must still serve its app.
    if !is_safe_rel(rel) {
        return Ok(None);
    }
    if !is_containable_build_id(build_id) {
        // Loudly: `Ok(None)` is indistinguishable from a genuine miss, the SPA
        // fallback misses too, and `custom_apps_bundle_cache` caches the
        // absence — so the app 404s uniformly for the process lifetime. An
        // operator needs something to grep for.
        tracing::warn!(
            "app {app_id}: refusing unsafe build id {build_id:?} — every asset in this build \
             will 404 until the row is corrected"
        );
        return Ok(None);
    }
    match bucket() {
        Some(bucket) => {
            let client = s3_client().await;
            let key = format!("{}{}", build_prefix(app_id, build_id), rel);
            match client.get_object().bucket(&bucket).key(&key).send().await {
                Ok(resp) => {
                    let data = resp
                        .body
                        .collect()
                        .await
                        .map_err(|e| BuildStoreError::S3(format!("collect {key}: {e}")))?;
                    // `into_bytes()` already hands back the same `bytes::Bytes` the
                    // serve path wants (one `bytes` in the lockfile, which is
                    // what `axum::body::Bytes` re-exports). Round-tripping it
                    // through `Vec` here just to rebuild a `Bytes` in the cache
                    // was a full alloc + memcpy on every cold fetch.
                    Ok(Some(data.into_bytes()))
                }
                Err(err) => {
                    let is_missing = err
                        .as_service_error()
                        .map(|e| e.is_no_such_key())
                        .unwrap_or(false);
                    if is_missing {
                        Ok(None)
                    } else {
                        Err(BuildStoreError::S3(format!("get_object {key}: {err}")))
                    }
                }
            }
        }
        None => {
            let dest = fs_build_dir(app_id, build_id).join(rel);
            match tokio::fs::read(&dest).await {
                Ok(bytes) => Ok(Some(Bytes::from(bytes))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(BuildStoreError::Io(format!("read {}: {e}", dest.display()))),
            }
        }
    }
}

/// What a `HEAD` learned about an object's size.
///
/// `Unknown` is a real answer, not a zero: a response without `Content-Length`
/// says the object is there and declines to say how big. Collapsing it into `0`
/// is what would make a present file report as empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectSize {
    Bytes(u64),
    Unknown,
}

impl ObjectSize {
    /// True only when the size is *known* to be zero.
    pub fn is_known_empty(self) -> bool {
        matches!(self, ObjectSize::Bytes(0))
    }
}

/// Whether an object exists in a build, and how many bytes it is — without
/// transferring it.
///
/// `Ok(None)` for an absent key, mirroring [`get_object`]. Exists for the
/// per-app health endpoint, which asks "is `index.html` there and non-empty?" on
/// every poll: `get_object` answers that identically but downloads the file to do
/// it, and this endpoint is designed to be polled per app. Same two guards as
/// `get_object` — an unsafe `rel` or an uncontainable `build_id` reads as a miss.
pub async fn head_object(
    app_id: Uuid,
    build_id: &str,
    rel_path: &str,
) -> Result<Option<ObjectSize>, BuildStoreError> {
    let rel = rel_path.trim_start_matches('/');
    if !is_safe_rel(rel) {
        return Ok(None);
    }
    if !is_containable_build_id(build_id) {
        // Same reasoning as `get_object`: `Ok(None)` is indistinguishable from a
        // genuine miss, so an operator needs something to grep for. It matters
        // more here — the health endpoint is the surface that would otherwise
        // report "re-publish the app" for a state re-publishing cannot fix.
        tracing::warn!(
            "app {app_id}: refusing unsafe build id {build_id:?} — every asset in this build \
             will 404 until the row is corrected"
        );
        return Ok(None);
    }
    match bucket() {
        Some(bucket) => {
            let client = s3_client().await;
            let key = format!("{}{}", build_prefix(app_id, build_id), rel);
            match client.head_object().bucket(&bucket).key(&key).send().await {
                // A response without `Content-Length` means "present, size
                // unknown" — NOT zero. Defaulting to 0 would make the health
                // check report "present but empty in the build store", a `fail`
                // carrying a confidently wrong detail.
                Ok(resp) => Ok(Some(match resp.content_length() {
                    Some(n) if n >= 0 => ObjectSize::Bytes(n as u64),
                    _ => ObjectSize::Unknown,
                })),
                Err(err) => {
                    // HEAD reports a missing key as 404 without a typed
                    // `NoSuchKey` the way GET does, so the status is what has to
                    // be read here.
                    let missing = err
                        .raw_response()
                        .map(|r| r.status().as_u16() == 404)
                        .unwrap_or(false)
                        || err
                            .as_service_error()
                            .map(|e| e.is_not_found())
                            .unwrap_or(false);
                    if missing {
                        Ok(None)
                    } else {
                        Err(BuildStoreError::S3(format!("head_object {key}: {err}")))
                    }
                }
            }
        }
        None => {
            let dest = fs_build_dir(app_id, build_id).join(rel);
            match tokio::fs::metadata(&dest).await {
                Ok(meta) => Ok(Some(ObjectSize::Bytes(meta.len()))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(BuildStoreError::Io(format!("stat {}: {e}", dest.display()))),
            }
        }
    }
}

/// Delete every object/file under a build prefix (keep-last-N GC).
pub async fn delete_build(app_id: Uuid, build_id: &str) -> Result<(), BuildStoreError> {
    // `remove_dir_all` on the FS backend — never let a traversal id name the
    // directory being reaped. Containment only, deliberately: a legacy row
    // whose id predates `is_valid_build_id` must stay reapable, or `gc_builds`
    // strands its prefix forever. See `is_containable_build_id`.
    if !is_containable_build_id(build_id) {
        return Err(BuildStoreError::Io(format!(
            "unsafe build id: {build_id:?}"
        )));
    }
    match bucket() {
        Some(bucket) => {
            let client = s3_client().await;
            let prefix = build_prefix(app_id, build_id);
            let mut continuation: Option<String> = None;
            loop {
                let mut req = client.list_objects_v2().bucket(&bucket).prefix(&prefix);
                if let Some(token) = &continuation {
                    req = req.continuation_token(token.clone());
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| BuildStoreError::S3(format!("list_objects_v2 {prefix}: {e}")))?;
                for obj in resp.contents() {
                    if let Some(key) = obj.key() {
                        client
                            .delete_object()
                            .bucket(&bucket)
                            .key(key)
                            .send()
                            .await
                            .map_err(|e| {
                                BuildStoreError::S3(format!("delete_object {key}: {e}"))
                            })?;
                    }
                }
                if resp.is_truncated().unwrap_or(false) {
                    continuation = resp.next_continuation_token().map(str::to_string);
                    if continuation.is_none() {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        None => {
            let dir = fs_build_dir(app_id, build_id);
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(BuildStoreError::Io(format!("rmdir {}: {e}", dir.display()))),
            }
        }
    }
    Ok(())
}

/// Delete every object/file under `customer-apps/<app_id>/` — used by
/// the admin delete-app endpoint to reclaim S3 storage so a deleted
/// row doesn't leak its bundle bytes forever. Best-effort: returns the
/// underlying error so the caller can decide whether to fail the
/// HTTP request or just log and proceed (the admin handler logs and
/// proceeds — see `delete_app` in `admin/apps/handlers.rs`).
///
/// On the local filesystem backend this is a `remove_dir_all` of
/// `<state_root>/customer-apps/<app_id>/`. On S3 it walks the prefix
/// in pages and batches each page through `DeleteObjects` (≤1000 keys
/// per call — the S3 limit, which matches `ListObjectsV2`'s default
/// page size, so each page becomes exactly one delete request).
pub async fn delete_app(app_id: Uuid) -> Result<(), BuildStoreError> {
    match bucket() {
        Some(bucket) => {
            let client = s3_client().await;
            let prefix = format!("customer-apps/{app_id}/");
            let mut continuation: Option<String> = None;
            loop {
                let mut req = client.list_objects_v2().bucket(&bucket).prefix(&prefix);
                if let Some(token) = &continuation {
                    req = req.continuation_token(token.clone());
                }
                let resp = req
                    .send()
                    .await
                    .map_err(|e| BuildStoreError::S3(format!("list_objects_v2 {prefix}: {e}")))?;

                // Batch this page into a single DeleteObjects call (≤1000
                // keys per S3 limit; ListObjectsV2 returns ≤1000 by default
                // so one delete-objects call covers each page exactly).
                let keys: Vec<aws_sdk_s3::types::ObjectIdentifier> = resp
                    .contents()
                    .iter()
                    .filter_map(|o| o.key())
                    .filter_map(|k| {
                        aws_sdk_s3::types::ObjectIdentifier::builder()
                            .key(k)
                            .build()
                            .ok()
                    })
                    .collect();
                if !keys.is_empty() {
                    let del = aws_sdk_s3::types::Delete::builder()
                        .set_objects(Some(keys))
                        .quiet(true)
                        .build()
                        .map_err(|e| BuildStoreError::S3(format!("build Delete payload: {e}")))?;
                    client
                        .delete_objects()
                        .bucket(&bucket)
                        .delete(del)
                        .send()
                        .await
                        .map_err(|e| {
                            BuildStoreError::S3(format!("delete_objects {prefix}: {e}"))
                        })?;
                }

                if resp.is_truncated().unwrap_or(false) {
                    continuation = resp.next_continuation_token().map(str::to_string);
                    if continuation.is_none() {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        None => {
            // FS backend mirrors the S3 prefix layout: builds live at
            // `<state_root>/customer-apps/<app_id>/builds/<build_id>/…`,
            // so the per-app directory we want to nuke is
            // `<state_root>/customer-apps/<app_id>/`.
            let app_dir = state_root().join(format!("customer-apps/{app_id}"));
            match tokio::fs::remove_dir_all(&app_dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    return Err(BuildStoreError::Io(format!(
                        "rmdir {}: {e}",
                        app_dir.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The id becomes a path segment under the state dir on the FS backend,
    /// and `delete_build` hands that path to `remove_dir_all`.
    #[test]
    fn build_id_traversal_is_refused() {
        assert!(!is_valid_build_id("../../../../tmp/x"));
        assert!(!is_valid_build_id(".."));
        assert!(!is_valid_build_id("a/b"));
        assert!(!is_valid_build_id("/abs"));
        // A leading dot would make a hidden directory.
        assert!(!is_valid_build_id(".hidden"));
        assert!(!is_valid_build_id(""));
        assert!(!is_valid_build_id(&"a".repeat(MAX_BUILD_ID_LEN + 1)));
        // Anything outside the charset, including the separators and the
        // wildcards an S3 prefix scan would be unhappy about.
        assert!(!is_valid_build_id("a b"));
        assert!(!is_valid_build_id("a\\b"));
        assert!(!is_valid_build_id("a*b"));
    }

    /// The read and delete sides must keep working for ids that predate the
    /// strict validator — `--build-id` was free text, so these are real rows
    /// in existing deployments. Refusing them would break serving, and would
    /// strand storage: `gc_builds` drops the row whether or not the store
    /// delete succeeded.
    #[test]
    fn legacy_ids_stay_readable_and_reapable_unless_they_can_escape() {
        for legacy in [
            "2026-08-07T10:00:00Z", // colons
            "v1.0+build",           // plus
            "release~rc1",          // tilde
            "feature branch",       // space
            // An interior separator is NOT an escape: this joins to
            // `<state>/…/builds/release/v1/`, still inside the state dir, and
            // on S3 the key is literal. Refusing it would 404 the whole app.
            "release/v1",
            // On unix a backslash is an ordinary filename byte — one
            // component, one real directory.
            "a\\b",
        ] {
            assert!(
                is_containable_build_id(legacy),
                "{legacy:?} cannot escape its prefix, so it must stay serveable and reapable"
            );
            assert!(
                !is_valid_build_id(legacy),
                "{legacy:?} must not be accepted for a NEW publish"
            );
        }
        // These genuinely escape or anchor outside the prefix.
        for escaping in [
            "..",
            ".",
            "../x",
            "a/../../x",
            "/abs",
            "",
            "/",
            ".hidden",
            "a/.hidden",
        ] {
            assert!(
                !is_containable_build_id(escaping),
                "{escaping:?} escapes or anchors outside its prefix"
            );
        }
    }

    /// Everything the CLI can actually produce must pass.
    #[test]
    fn build_id_accepts_what_the_cli_generates() {
        // `ci_build_id`: sha, sha-run, sha-run.attempt.
        assert!(is_valid_build_id("0a1b2c3d"));
        assert!(is_valid_build_id("0a1b2c3d-31166280801"));
        assert!(is_valid_build_id("0a1b2c3d-31166280801.2"));
        // The random-uuid fallback.
        assert!(is_valid_build_id(&Uuid::new_v4().simple().to_string()));
        // A hand-picked id an engineer might pass.
        assert!(is_valid_build_id("v1.2.3_rc1"));
        assert!(is_valid_build_id(&"a".repeat(MAX_BUILD_ID_LEN)));
    }

    #[test]
    fn build_prefix_has_no_leading_slash_and_trailing_slash() {
        let p = build_prefix(Uuid::nil(), "abc123");
        assert!(!p.starts_with('/'), "no leading slash: {p}");
        assert!(p.ends_with('/'), "trailing slash: {p}");
        assert_eq!(
            p,
            "customer-apps/00000000-0000-0000-0000-000000000000/builds/abc123/"
        );
    }

    #[tokio::test]
    async fn delete_app_removes_every_build_on_fs_and_is_idempotent() {
        // Force the filesystem backend into a temp state dir.
        let tmp = std::env::temp_dir().join(format!("oxy-bs-test-{}", Uuid::new_v4()));
        // SAFETY: single-threaded test; we set then clear the vars.
        unsafe {
            std::env::remove_var("OXY_CUSTOMER_APPS_S3_BUCKET");
            std::env::set_var("OXY_STATE_DIR", &tmp);
        }
        let app = Uuid::new_v4();

        // Two builds + a nested asset under each — must all disappear.
        put_build(
            app,
            "b1",
            vec![
                ("index.html".into(), b"<html>1".to_vec()),
                ("assets/main.js".into(), b"console.log(1)".to_vec()),
            ],
        )
        .await
        .expect("put b1");
        put_build(app, "b2", vec![("index.html".into(), b"<html>2".to_vec())])
            .await
            .expect("put b2");
        let app_dir = state_root().join(format!("customer-apps/{app}"));
        assert!(app_dir.exists(), "app dir should exist before delete");

        delete_app(app).await.expect("delete_app");
        assert!(!app_dir.exists(), "app dir gone after delete_app");
        assert_eq!(
            get_object(app, "b1", "index.html").await.expect("get b1"),
            None,
            "b1 index.html gone"
        );
        assert_eq!(
            get_object(app, "b2", "index.html").await.expect("get b2"),
            None,
            "b2 index.html gone"
        );

        // Idempotent: calling again on a non-existent app dir is a no-op
        // (NotFound is swallowed), so an admin double-click can't 500.
        delete_app(app).await.expect("delete_app idempotent");

        unsafe {
            std::env::remove_var("OXY_STATE_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn fs_backend_put_get_delete_roundtrips() {
        // Force the filesystem backend into a temp state dir.
        let tmp = std::env::temp_dir().join(format!("oxy-bs-test-{}", Uuid::new_v4()));
        // SAFETY: single-threaded test; we set then clear the vars.
        unsafe {
            std::env::remove_var("OXY_CUSTOMER_APPS_S3_BUCKET");
            std::env::set_var("OXY_STATE_DIR", &tmp);
        }
        let app = Uuid::new_v4();
        put_build(app, "b1", vec![("index.html".into(), b"<html>hi".to_vec())])
            .await
            .expect("put");
        let got = get_object(app, "b1", "index.html").await.expect("get");
        assert_eq!(got.as_deref(), Some(&b"<html>hi"[..]));
        let missing = get_object(app, "b1", "nope.js").await.expect("get missing");
        assert_eq!(missing, None);
        delete_build(app, "b1").await.expect("delete");
        let after = get_object(app, "b1", "index.html")
            .await
            .expect("get after delete");
        assert_eq!(after, None);
        unsafe {
            std::env::remove_var("OXY_STATE_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[cfg(test)]
mod object_size_tests {
    use super::ObjectSize;

    /// Only a size *known* to be zero is an empty object.
    ///
    /// `Unknown` exists because a `HEAD` without `Content-Length` says "it's
    /// there" and declines to say how big. Folding that into `0` — which the
    /// first draft did via `unwrap_or(0)` — made the health endpoint report a
    /// perfectly good `index.html` as "present but empty in the build store": a
    /// failure carrying a confidently wrong detail, on an endpoint whose whole
    /// value is that its verdict can be trusted.
    #[test]
    fn only_a_known_zero_is_empty() {
        assert!(ObjectSize::Bytes(0).is_known_empty());
        assert!(!ObjectSize::Bytes(1).is_known_empty());
        assert!(
            !ObjectSize::Unknown.is_known_empty(),
            "an unmeasured object is present, not empty"
        );
    }
}
