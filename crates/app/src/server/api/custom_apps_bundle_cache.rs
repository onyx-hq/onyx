//! In-memory LRU over custom-app bundle objects.
//!
//! The serve path resolves `(app_id, build_id, rel_path)` to bytes; this
//! cache absorbs hot reads so S3 is hit only on a miss. Keyed by
//! `"<app_id>/<build_id>/<rel_path>"` — because `build_id` is part of the
//! key, a promote/rollback (which repoints to a different build) serves
//! fresh bytes with no explicit invalidation.
//!
//! ## Absences are cached too
//!
//! A *known-absent* object is remembered rather than re-fetched, in a
//! second LRU held apart from the bytes (see [`byte_cache`] for why the two
//! must not share a map). A build's file set is fixed once
//! `put_build` returns — the prefix is written once and never appended to —
//! so "not in this build" is a permanent fact for that `build_id`, and
//! caching it is safe.
//!
//! That invariant is **enforced, not assumed**. `put_build` never wipes a
//! prefix before writing, so a second publish under a reused `build_id` would
//! merge into the first — and a file the rebuild added would read as
//! permanently absent here, which for a `.js` request means the SPA
//! `index.html` fallback at 200 rather than the asset. Two things keep that
//! from happening: `custom_apps_publish` rejects a publish whose
//! `(app_id, build_id)` already has an `app_builds` row (409), and the CLI's
//! default id is unique per CI *run*, not per commit, so a workflow re-run
//! doesn't collide in the first place (`sdk/cli/src/publish/provenance.ts::ciBuildId`).
//! If either is ever relaxed, this section is the thing that breaks.
//!
//! Two hot paths depend on this, and both re-fetched on *every* request
//! before it existed:
//!   - **SPA fallback.** A client-side route (`/orders/42`) misses the
//!     object store, then falls back to `index.html`. Without negative
//!     caching that's one doomed store round-trip per navigation, forever.
//!   - **Pre-compressed variants.** The serve path probes `<path>.br`
//!     (see `custom_apps_precompress`). Builds published before
//!     pre-compression existed have no `.br` objects at all, so every
//!     asset request on an old build would pay a doomed probe.
//!
//! Errors are **not** cached — only a definitive present/absent answer is,
//! so a transient S3 failure retries on the next request.

use std::num::NonZeroUsize;
use std::sync::OnceLock;

use axum::body::Bytes;

use lru::LruCache;
use parking_lot::Mutex;
use uuid::Uuid;

use super::custom_apps_build_store::{self, BuildStoreError};

/// Entry cap for cached **bytes**, count-bounded rather than byte-bounded
/// for simplicity — revisit if a single app ships huge assets.
///
/// Worth knowing that this leaves the two maps bounded backwards relative to
/// their value: `MAX_ABSENT_REL_LEN` gives a *length* bound to the map whose
/// entries are ≤ ~550 bytes, while this one count-bounds the map holding whole
/// assets, capped only by the bundle's own unpack ceiling — 8192 slots × a
/// multi-MiB chunk is GiBs resident on a replica hosting many apps, and every
/// publisher sizes their own assets. The revisit is cheaper now that entries
/// are `Bytes`: `len()` is O(1), so a running total decremented on eviction is
/// a few lines.
///
/// A build contributes roughly its file count, plus — for an asset requested
/// by both a brotli and a non-brotli client — one entry per representation.
/// The `.br`-first probe in `custom_apps_serve::sources` keeps the common
/// case to a single entry per asset, since a brotli hit never fetches the
/// identity object.
const MAX_BYTE_ENTRIES: usize = 8192;

/// Entry cap for **known-absent** keys. Larger than the byte cap because an
/// entry is a bare string rather than a payload, and because the key space
/// here is genuinely unbounded: `rel` comes from the request URL, so every
/// distinct client-side route a visitor hits records one.
const MAX_ABSENT_ENTRIES: usize = 16384;

/// Longest `rel` worth remembering as absent.
///
/// The cap above counts entries, but the key embeds `rel` verbatim and
/// `is_safe_rel` constrains only its *shape*, never its length — so a count
/// cap alone leaves the resident bytes of this map under request control.
/// A real bundle path is far under this, so declining to record longer ones
/// costs nothing on any genuine request. The pathological URL re-pays a store
/// round-trip on **every** request rather than once per process — not a
/// one-off — which is the deliberate trade: the alternative is resident bytes
/// under request control. Hashing the tail into the key would keep both
/// properties if that ever stops being the right call.
///
/// The bound is on `rel_path`, while the stored key is
/// `"<uuid>/<build_id>/<rel>"` and legacy `build_id`s have no length cap
/// (`MAX_BUILD_ID_LEN` gates new publishes only), so the true per-entry
/// ceiling is `512 + |build_id| + 37`.
const MAX_ABSENT_REL_LEN: usize = 512;

/// Cached object bytes.
///
/// `Bytes`, not `Arc<Vec<u8>>`: the serve path hands these straight to
/// `Body::from`, which takes `Bytes` by value and refcounts it. Holding
/// `Arc<Vec<u8>>` forced a `to_vec()` at that boundary — a full alloc and
/// memcpy of the asset on every warm-cache hit, which is exactly the
/// per-request cost this module exists to remove.
type ByteCache = Mutex<LruCache<String, Bytes>>;
/// Keys known not to exist in their build. Value-less: presence *is* the fact.
type AbsentCache = Mutex<LruCache<String, ()>>;

/// Bytes and absences live in **separate** LRUs rather than one map of
/// `Option<_>`.
///
/// Sharing one map lets negatives evict positives, and the two are not
/// remotely equal in value: an absence costs one store round-trip to
/// rediscover, while a positive costs a round-trip *and* the bytes. Because
/// absent keys are attacker-shaped — unbounded, URL-derived, and cheap to
/// generate — sustained traffic over many client-side routes on one app
/// (a crawler, a deep-linked list view) would otherwise evict *another*
/// app's hot asset bytes from this process-global cache. Splitting them
/// makes that structurally impossible instead of a sizing accident, and
/// lets each cap suit what it holds.
fn byte_cache() -> &'static ByteCache {
    static CACHE: OnceLock<ByteCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(MAX_BYTE_ENTRIES).expect("MAX_BYTE_ENTRIES > 0"),
        ))
    })
}

fn absent_cache() -> &'static AbsentCache {
    static CACHE: OnceLock<AbsentCache> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(MAX_ABSENT_ENTRIES).expect("MAX_ABSENT_ENTRIES > 0"),
        ))
    })
}

fn key(app_id: Uuid, build_id: &str, rel_path: &str) -> String {
    format!("{app_id}/{build_id}/{}", rel_path.trim_start_matches('/'))
}

/// Return the object bytes, fetching from the build store on a cache miss
/// and caching the outcome — including a definitive absence. `Ok(None)`
/// when the object does not exist in the build.
pub async fn get_or_fetch(
    app_id: Uuid,
    build_id: &str,
    rel_path: &str,
) -> Result<Option<Bytes>, BuildStoreError> {
    let k = key(app_id, build_id, rel_path);
    // Bytes first: the common case, and the more valuable answer.
    if let Some(hit) = byte_cache().lock().get(&k).cloned() {
        return Ok(Some(hit));
    }
    // Then "we have asked before and it wasn't there".
    if absent_cache().lock().get(&k).is_some() {
        return Ok(None);
    }
    // A store error propagates WITHOUT being cached, so a transient failure
    // doesn't pin a false absence for the life of the build.
    let fetched = custom_apps_build_store::get_object(app_id, build_id, rel_path).await?;
    match &fetched {
        Some(bytes) => {
            byte_cache().lock().put(k, bytes.clone());
        }
        None if rel_path.len() <= MAX_ABSENT_REL_LEN => {
            absent_cache().lock().put(k, ());
        }
        // Too long to be worth remembering — see `MAX_ABSENT_REL_LEN`.
        // Logged because the consequence is a store round-trip on *every*
        // request for this path: without a line to grep, that surfaces only
        // as unexplained store traffic.
        //
        // `trace!`, not `debug!`: these are exactly the paths with no
        // memoization, so the line repeats per request rather than once —
        // a scanner walking long URLs would hold it open indefinitely, and
        // `debug` is enabled in plenty of dev and staging configs.
        //
        // A 64-char prefix rather than the whole path: an operator needs a
        // handle to correlate against the access log, but `rel_path` is
        // request-controlled and 512+ bytes of it per line is its own
        // problem. Taken by `chars()` so a multi-byte boundary can't panic.
        None => {
            let prefix: String = rel_path.chars().take(64).collect();
            tracing::trace!(
                "app {app_id} build {build_id}: not caching absence of a {} byte path \
                 (> {MAX_ABSENT_REL_LEN}); every request for it hits the store. starts: {prefix:?}",
                rel_path.len()
            );
        }
    }
    Ok(fetched)
}

/// Warm the cache directly (used at publish time so the first viewer of a
/// freshly-published `index.html` doesn't pay the cold S3 round trip).
pub fn seed(app_id: Uuid, build_id: &str, rel_path: &str, bytes: Bytes) {
    let k = key(app_id, build_id, rel_path);
    // Drop any recorded absence for this key. Lookups check bytes first, so
    // this isn't load-bearing for correctness — but leaving a contradicted
    // negative behind wastes a slot and would read as a bug to the next
    // person holding both maps in their head.
    absent_cache().lock().pop(&k);
    byte_cache().lock().put(k, bytes);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The absence itself must be remembered. Proven observably: ask for a
    /// file that isn't there (caching the absence), then create it on disk
    /// and ask again — a still-`None` answer can only mean the second call
    /// never reached the store.
    ///
    /// This is what spares the SPA-fallback and `.br`-probe paths a doomed
    /// store round-trip on every single request.
    #[tokio::test]
    async fn absent_object_is_remembered_and_not_refetched() {
        let tmp = std::env::temp_dir().join(format!("oxy-bc-test-{}", Uuid::new_v4()));
        // SAFETY: nextest runs each test in its own process, so no other test
        // observes these vars. Forces the filesystem build-store backend.
        unsafe {
            std::env::remove_var("OXY_CUSTOMER_APPS_S3_BUCKET");
            std::env::set_var("OXY_STATE_DIR", &tmp);
        }
        let app = Uuid::new_v4();

        let first = get_or_fetch(app, "b1", "assets/main.js.br")
            .await
            .expect("miss is not an error");
        assert!(first.is_none(), "object genuinely absent on the first ask");

        // Materialise the file the cache has already recorded as absent.
        let dir = tmp.join(format!("customer-apps/{app}/builds/b1/assets"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("main.js.br"), b"\x1b\x0e\x00").expect("write");

        let second = get_or_fetch(app, "b1", "assets/main.js.br")
            .await
            .expect("cached absence is not an error");
        assert!(
            second.is_none(),
            "absence must be served from cache — a Some here means the store was hit again"
        );

        unsafe {
            std::env::remove_var("OXY_STATE_DIR");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Absences must not be able to push bytes out. The two live in separate
    /// LRUs precisely so a crawler walking one app's client-side routes
    /// can't cost another app its hot assets on this process-global cache.
    #[tokio::test]
    async fn absences_cannot_evict_cached_bytes() {
        // SAFETY: nextest gives each test its own process.
        unsafe {
            std::env::remove_var("OXY_CUSTOMER_APPS_S3_BUCKET");
            std::env::set_var("OXY_STATE_DIR", std::env::temp_dir().join("oxy-bc-evict"));
        }
        let victim = Uuid::new_v4();
        let bytes = Bytes::from_static(b"<html>hot asset");
        seed(victim, "b1", "assets/hot.js", bytes.clone());

        // Record more absences than the byte cache could ever hold. Under one
        // shared map this alone would evict the seeded entry.
        //
        // `expect` rather than `let _`: errors are deliberately not cached
        // (see `get_or_fetch`), so a loop that errored every iteration would
        // record zero absences, never pressure the byte cache, and leave the
        // assertion below passing vacuously.
        let noisy = Uuid::new_v4();
        for i in 0..(MAX_BYTE_ENTRIES + 256) {
            let miss = get_or_fetch(noisy, "b1", &format!("route/{i}"))
                .await
                .expect("a miss is not an error — the premise of this test");
            assert!(miss.is_none(), "route/{i} must not exist");
        }

        let got = get_or_fetch(victim, "b1", "assets/hot.js")
            .await
            .expect("cached bytes must not require the store");
        assert_eq!(
            got.as_deref(),
            Some(&b"<html>hot asset"[..]),
            "negative entries evicted cached bytes — the two caches are sharing capacity"
        );
        unsafe {
            std::env::remove_var("OXY_STATE_DIR");
        }
    }

    #[tokio::test]
    async fn seeded_entry_served_without_s3() {
        // Seeding first proves the cache short-circuits the store entirely:
        // the lookup returns the seeded bytes without ever calling
        // get_object (no S3 round trip, no filesystem read).
        let app = Uuid::new_v4();
        let bytes = Bytes::from_static(b"<html>seeded");
        seed(app, "bx", "index.html", bytes.clone());
        let got = get_or_fetch(app, "bx", "index.html")
            .await
            .expect("cache hit must not touch S3");
        assert_eq!(got.as_deref(), Some(&b"<html>seeded"[..]));
    }
}
