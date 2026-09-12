//! The per-build **asset manifest** — the small JSON document that turns a
//! published bundle from "a pile of objects in a prefix" into something the
//! serve plane and the browser can reason about ahead of time.
//!
//! It exists because everything fast about loading a bundle needs to know the
//! file list *before* the browser discovers it by parsing HTML:
//!
//! - **`Link: rel=preload` / `rel=modulepreload` on the HTML response.** The
//!   entry chunks start downloading while the HTML is still in flight, instead
//!   of one RTT later when the parser reaches the `<script>` tag. This is the
//!   single biggest first-load win available at the origin, and it is exactly
//!   what Firebase Hosting and Cloudflare Pages synthesise from their own build
//!   output.
//! - **Service-worker precache** (`custom_apps_service_worker`). The worker
//!   fetches this document at install and pins the build's entry assets, so the
//!   *second* load of an app is zero network for everything but the shell.
//! - **Publish-time cache warming.** `custom_apps_publish` seeds the bundle LRU
//!   with the entries named here, so the first visitor after a publish doesn't
//!   pay a cold object-store round-trip per critical chunk.
//!
//! ## Why it is a build artifact, not a database column
//!
//! The manifest is written *into the build prefix* at publish
//! (`__oxy/asset-manifest.json`) rather than onto the `app_builds` row. Three
//! reasons, in order of weight:
//!
//! 1. **It is build-scoped by construction.** The bundle cache keys on
//!    `(app_id, build_id, rel_path)`, so a promote or rollback serves the right
//!    manifest with no invalidation, exactly like every other object in the
//!    build. A column would need its own cache and its own drop sites.
//! 2. **It reads through machinery that already exists** — the same LRU, the
//!    same absence caching, the same pre-compression. A build published before
//!    this module existed simply has no such object, the absence is remembered
//!    once per process, and every caller degrades to "no manifest" rather than
//!    erroring.
//! 3. **No migration.** A JSONB column would have been a schema change for data
//!    that is immutable, per-build, and already has a natural home.
//!
//! ## Scope of `entries` vs `assets`
//!
//! `entries` is the **critical path**: the scripts and stylesheets the entry
//! HTML references directly, in document order. It is what gets preload hints
//! and what the worker precaches — a bounded, small list.
//!
//! `assets` is the **full precacheable set**: every content-hashed file in the
//! build. It is what the worker is *allowed* to serve cache-first without
//! revalidating, because a hashed URL can only change when its bytes do. It is
//! deliberately not precached eagerly — a big bundle would spend the visitor's
//! bandwidth on routes they may never open.

use super::custom_apps_serve::rewrite::{CUSTOM_APPS_SENTINEL, first_custom_apps_prefix};
use serde::{Deserialize, Serialize};

/// Bundle-relative path the manifest is published at. Under `__oxy/` — a
/// reserved namespace for platform-owned objects inside an app's own bundle,
/// so nothing an app ships can collide with it (see
/// [`is_reserved_platform_path`]).
pub const ASSET_MANIFEST_PATH: &str = "__oxy/asset-manifest.json";

/// The reserved prefix for platform-injected objects inside a build.
///
/// A bundle that happens to ship its own `__oxy/…` file would otherwise shadow
/// (or be shadowed by) ours depending on insertion order. Publish strips any
/// author-supplied path under this prefix, so the namespace is ours alone and
/// the serve path can answer for it without consulting the store.
pub const RESERVED_PLATFORM_PREFIX: &str = "__oxy/";

/// How many entry assets are worth naming. A real bundle has 2–6 (one entry
/// chunk, one vendor chunk, one stylesheet, maybe a font); anything past this
/// is a build that inlines its whole module graph into `<head>`, where preload
/// hints stop helping and start competing with each other for the connection.
///
/// The cap matters beyond taste: `entries` is rendered into a `Link` response
/// header, and header size is bounded by every proxy in the path (nginx's
/// default is 8 KiB for the whole block). An unbounded list would put response
/// success under the control of whatever the app's bundler emitted.
///
/// **This count alone is not that bound**, which is worth stating because the
/// arithmetic looks like it is: 16 entries × a [`MAX_PATH_LEN`] path plus the
/// base and the `rel=`/`as=` decoration is ~9–10 KiB, i.e. *over* the limit it
/// is justified by. `HeaderValue::from_str` accepts that happily and the
/// intermediary is the one that rejects it — a 502 on the shell, not a dropped
/// hint. [`preload_link_header`] therefore bounds the rendered bytes as well;
/// this cap just keeps the common case from needing that.
pub const MAX_ENTRIES: usize = 16;

/// Byte budget for the rendered `Link` header value.
///
/// Deliberately well under nginx's 8 KiB *whole-block* default: this is one
/// header among several on the response, and the block is what the limit
/// governs. 4 KiB leaves room for the rest and is far more than any real
/// bundle's entry list needs — a typical Vite build renders ~200 bytes here.
const MAX_LINK_HEADER_BYTES: usize = 4 * 1024;

/// Longest bundle-relative path admitted into the manifest. Same motivation as
/// the cap above — this document is rendered into a response header and into a
/// service worker's precache list, so the sizes have to be bounded somewhere
/// that isn't "whatever the tarball contained".
const MAX_PATH_LEN: usize = 512;

/// What kind of preload hint an entry deserves. Chosen from the extension and
/// the tag that referenced it, because the browser needs `as=` to set the right
/// priority and the right CORS mode — a mismatched `as=` is worse than no hint
/// at all (it downloads the file twice).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    /// An ES module — gets `rel=modulepreload`, which also warms the module's
    /// own static imports in most engines.
    Module,
    /// A classic script — `rel=preload; as=script`.
    Script,
    /// A stylesheet — `rel=preload; as=style`. Render-blocking, so this is the
    /// highest-value hint of the three.
    Style,
    /// A font. `crossorigin` is mandatory on font preloads even same-origin, or
    /// the browser fetches it a second time in anonymous mode.
    Font,
}

impl EntryKind {
    /// The `rel`/`as` pair for a `Link` header, plus whether `crossorigin` is
    /// required.
    /// `(rel, as, crossorigin, fetchpriority)`.
    ///
    /// `fetchpriority=high` is a nudge, not a guarantee: it is honored in the
    /// `Link` *response header* by Chromium (and in 103 Early Hints), ignored
    /// harmlessly elsewhere, and it is a valid extension parameter so no proxy
    /// rejects it. Given only to the two resources first paint actually blocks
    /// on — the entry module and the render-blocking stylesheet — never to fonts
    /// (which are not first-paint-critical and would only steal bandwidth from
    /// the two that are).
    fn link_attrs(
        self,
    ) -> (
        &'static str,
        Option<&'static str>,
        bool,
        Option<&'static str>,
    ) {
        match self {
            EntryKind::Module => ("modulepreload", None, false, Some("high")),
            EntryKind::Script => ("preload", Some("script"), false, Some("high")),
            EntryKind::Style => ("preload", Some("style"), false, Some("high")),
            EntryKind::Font => ("preload", Some("font"), true, None),
        }
    }
}

/// One critical-path asset the entry HTML references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// Bundle-relative path, no leading slash (`assets/index-abc123.js`).
    pub path: String,
    pub kind: EntryKind,
}

/// Per-app opt-outs for the platform's browser-side runtime, read from the
/// bundle's `oxy-app.json` at publish and carried in the asset manifest.
///
/// They live **here** rather than on the `apps` row because they are properties
/// of a *build*: an author who turns the service worker off does so in the
/// manifest they ship, and a rollback should restore the setting the rolled-back
/// build was published with. Carrying them in the document the serve path
/// already fetches also means reading them costs nothing extra.
///
/// Both default **on**. An app that says nothing gets the worker and the
/// instrumentation, which is the whole point — "add tracking to all custom
/// apps" is not a thing you can achieve with an opt-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientPrefs {
    /// `performance.serviceWorker: false` in `oxy-app.json`.
    pub service_worker: bool,
    /// `analytics: false` in `oxy-app.json`. Note this only silences the
    /// *client* runtime — the server still records one view row per HTML
    /// navigation, which no app can opt out of and which is what the Activity
    /// tab's floor has always been.
    pub analytics: bool,
}

impl Default for ClientPrefs {
    fn default() -> Self {
        Self {
            service_worker: true,
            analytics: true,
        }
    }
}

impl ClientPrefs {
    /// Read the opt-outs out of a raw `oxy-app.json`.
    ///
    /// Takes the untyped value rather than `OxyAppManifest` because publish
    /// already holds the raw JSON at the point the asset manifest is built, and
    /// because an unreadable or absent block must mean "defaults", never an
    /// error: a typo in an optional performance hint is not a reason to fail a
    /// publish.
    pub fn from_manifest_json(manifest: Option<&serde_json::Value>) -> Self {
        let Some(manifest) = manifest else {
            return Self::default();
        };
        Self {
            service_worker: manifest
                .get("performance")
                .and_then(|p| p.get("serviceWorker"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
            analytics: manifest
                .get("analytics")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true),
        }
    }
}

/// The published document. Serialised to `__oxy/asset-manifest.json`.
///
/// `schema_version` is present so a worker cached from an older build can
/// recognise a shape it doesn't understand and fall through to network rather
/// than mis-parsing it — a service worker outlives the page that installed it,
/// so it is the one consumer that genuinely can be older than the data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetManifest {
    pub schema_version: u32,
    /// The build this manifest describes. The worker uses it as its cache
    /// generation: a changed `build_id` means "drop everything and re-precache".
    pub build_id: String,
    /// Critical-path assets in document order.
    pub entries: Vec<Entry>,
    /// Every content-hashed asset in the build — the set the worker may serve
    /// cache-first. Not precached eagerly.
    pub assets: Vec<String>,
    /// Browser-runtime opt-outs this build was published with. `#[serde(default)]`
    /// so a manifest written before the field existed reads as "both on".
    #[serde(default)]
    pub client: ClientPrefs,
}

/// Current schema. Bump only on a breaking shape change; a worker checks this
/// before trusting the rest.
pub const SCHEMA_VERSION: u32 = 1;

/// Is this bundle-relative path inside the platform-reserved namespace?
pub fn is_reserved_platform_path(rel: &str) -> bool {
    rel.trim_start_matches('/')
        .to_ascii_lowercase()
        .starts_with(RESERVED_PLATFORM_PREFIX)
}

/// True for a path under a content-hashed output directory — the ones whose
/// URL changes when their bytes do, and which are therefore safe to serve
/// cache-first forever.
///
/// Deliberately the **same prefix list** as `cache_control_for`'s `immutable`
/// branch in `custom_apps_serve::headers`. If the two ever disagree, the worker
/// would pin something the origin says is revalidatable (or vice versa), and
/// the failure mode is a stale chunk with no server-side remedy. A new bundler
/// convention belongs in both, and the test at the bottom of this module
/// asserts they agree.
pub fn is_immutable_asset_path(rel: &str) -> bool {
    let t = rel.trim_start_matches('/');
    t.starts_with("assets/") || t.starts_with("_next/static/")
}

/// Build the manifest for a publish, from the bundle's `(path, bytes)` list.
///
/// The entry list comes from parsing the bundle's own `index.html` rather than
/// from a bundler-specific manifest file (`.vite/manifest.json`,
/// `build-manifest.json`, …). One parser covers every bundler this platform
/// accepts, and it reads the artifact that is actually served — so it cannot
/// disagree with what the browser will do.
pub fn build_from_files(
    build_id: &str,
    files: &[(String, Vec<u8>)],
    client: ClientPrefs,
) -> AssetManifest {
    let index_html = files
        .iter()
        .find(|(p, _)| p.trim_start_matches('/') == "index.html")
        .map(|(_, b)| b.as_slice())
        .unwrap_or(b"");

    let entries = parse_entries(index_html);

    let mut assets: Vec<String> = files
        .iter()
        .map(|(p, _)| p.trim_start_matches('/').to_string())
        .filter(|p| {
            is_immutable_asset_path(p)
                && !is_reserved_platform_path(p)
                && p.len() <= MAX_PATH_LEN
                // A `.br` sibling is an internal representation the serve path
                // refuses to address (`custom_apps_serve::sources`), so naming
                // it here would hand the worker a list of URLs that 404.
                && !p.ends_with(super::custom_apps_precompress::PRECOMPRESSED_SUFFIX)
        })
        .collect();
    // Sorted so the document is byte-stable across publishes of identical
    // input: the tarball's iteration order is not, and an unstable manifest
    // would make every build's ETag differ for no reason.
    assets.sort();
    assets.dedup();

    AssetManifest {
        schema_version: SCHEMA_VERSION,
        build_id: build_id.to_string(),
        entries,
        assets,
        client,
    }
}

/// Reserve the `__oxy/` namespace in a bundle's file list and install the
/// platform's asset manifest into it. Returns the manifest that was written.
///
/// Both steps, in this order:
///
/// 1. **Strip anything the author put under the prefix.** The serve path answers
///    for `__oxy/*` itself, so an author-supplied file there could only shadow
///    ours or be shadowed by it depending on which landed last. Dropping it is
///    what makes the namespace genuinely reserved rather than reserved by
///    convention.
/// 2. **Write the manifest**, generated from the file list as it stands *after*
///    the strip — so it never advertises a path that was just removed.
///
/// Shared by `oxyc publish` and `oxy seed` deliberately: the seeded example app
/// is the bundle every new workspace opens first, and a seed that skipped this
/// would ship the one app we control on the slow path.
///
/// A serialisation failure is logged and swallowed rather than propagated. The
/// consequence of no manifest is "no preload hints, no precache" — every
/// consumer already handles it, because every build published before this
/// existed is in exactly that state. Failing a good publish over a hint would be
/// the wrong trade.
pub fn install_into(
    files: &mut Vec<(String, Vec<u8>)>,
    build_id: &str,
    manifest_json: Option<&serde_json::Value>,
) -> AssetManifest {
    let before = files.len();
    files.retain(|(path, _)| !is_reserved_platform_path(path));
    if files.len() != before {
        tracing::warn!(
            "build {build_id}: dropped {} file(s) under the reserved `{RESERVED_PLATFORM_PREFIX}` \
             prefix from the uploaded bundle",
            before - files.len(),
        );
    }
    let manifest = build_from_files(
        build_id,
        files,
        ClientPrefs::from_manifest_json(manifest_json),
    );
    match serde_json::to_vec(&manifest) {
        Ok(bytes) => files.push((ASSET_MANIFEST_PATH.to_string(), bytes)),
        Err(e) => tracing::error!(
            "build {build_id}: could not serialise the asset manifest, shipping without one: {e}"
        ),
    }
    manifest
}

/// Extract the critical-path assets from an entry HTML document.
///
/// A deliberately small, forgiving scanner rather than a real HTML parser:
/// the input is bundler output, the failure mode of a miss is "one fewer
/// preload hint", and pulling an HTML parser into the publish path to recover
/// a hint is a bad trade. It reads `src=` / `href=` out of `<script>` and
/// `<link>` tags and keeps the ones that point at same-bundle relative paths.
fn parse_entries(html: &[u8]) -> Vec<Entry> {
    let Ok(text) = std::str::from_utf8(html) else {
        return Vec::new();
    };
    let mut out: Vec<Entry> = Vec::new();
    let lower = text.to_ascii_lowercase();
    // The base path this bundle was BUILT with, if it baked one in. Read once
    // per document with the same function the serve-time rewriter uses, so the
    // two can never disagree about what the prefix is.
    let baked = first_custom_apps_prefix(text);

    let mut cursor = 0usize;
    while let Some(rel_start) = lower[cursor..].find('<') {
        let start = cursor + rel_start;
        let Some(rel_end) = lower[start..].find('>') else {
            break;
        };
        let end = start + rel_end + 1;
        // Slice from the ORIGINAL text: attribute values are case-sensitive
        // (a hashed filename is mixed case), while the tag/attr names we match
        // on come from the lowercased copy.
        let tag_lower = &lower[start..end];
        let tag_raw = &text[start..end];
        cursor = end;

        if tag_lower.starts_with("<script") {
            let Some(src) = attr_value(tag_lower, tag_raw, "src") else {
                continue;
            };
            let kind = if tag_lower.contains("type=\"module\"")
                || tag_lower.contains("type='module'")
                || tag_lower.contains("type=module")
            {
                EntryKind::Module
            } else {
                EntryKind::Script
            };
            push_entry(&mut out, &src, kind, baked.as_deref());
        } else if tag_lower.starts_with("<link") {
            let Some(href) = attr_value(tag_lower, tag_raw, "href") else {
                continue;
            };
            // `rel=stylesheet` is the render-blocking one worth hinting.
            // `rel=modulepreload` is a hint the bundle already emitted — echo
            // it into the header so it starts one RTT earlier.
            let kind = if tag_lower.contains("stylesheet") {
                EntryKind::Style
            } else if tag_lower.contains("modulepreload") {
                EntryKind::Module
            } else if tag_lower.contains("as=\"font\"") || tag_lower.contains("as='font'") {
                EntryKind::Font
            } else {
                continue;
            };
            push_entry(&mut out, &href, kind, baked.as_deref());
        }
    }
    out
}

/// Admit one parsed reference into the entry list, normalising and filtering.
///
/// Rejects anything that isn't a same-bundle relative path: absolute URLs
/// (`https://…`, `//cdn…`), data URIs, and root-absolute paths that point
/// outside the bundle. A hint for a cross-origin asset is not wrong, but it is
/// not ours to make — and a root-absolute path is exactly the build-time-base
/// mismatch that `rewrite_bundle_base_path` exists to repair, so hinting the
/// un-rewritten form would preload a 404.
fn push_entry(out: &mut Vec<Entry>, raw: &str, kind: EntryKind, baked: Option<&str>) {
    if out.len() >= MAX_ENTRIES {
        return;
    }
    let value = raw.trim();
    if value.is_empty() || value.len() > MAX_PATH_LEN {
        return;
    }
    if value.contains("://") || value.starts_with("//") || value.starts_with("data:") {
        return;
    }
    // A root-absolute reference in bundler output is written against the base
    // path the bundle was BUILT with. `rewrite_bundle_base_path` fixes those in
    // the body at serve time; the manifest is generated at publish time, before
    // that rewrite exists, so anything root-absolute whose base doesn't already
    // match the bundle root would produce a hint for a URL that never resolves.
    // Keeping only the tail is correct precisely because every consumer re-bases
    // against the app root: the worker's precache prepends `ASSET_BASE`, and the
    // `Link:` preload header prepends the request's own base.
    //
    // Two root-absolute shapes reach here, and only one is a plain trim:
    //
    //  - `/assets/x.js` — built with the default base `/`. The tail IS the value
    //    minus its slash, and serve-time Case 2 prefixes it the same way.
    //  - `/customer-apps/<org>/<slug>/assets/x.js` — built with a base baked in.
    //    Trimming only the slash keeps the WHOLE mount prefix, and every consumer
    //    then re-bases a path that is already based: `ASSET_BASE + path` doubles
    //    the prefix and 404s. Strip the baked prefix so the tail is really a tail.
    let value = match baked {
        // A `/customer-apps/...` reference that does NOT sit under this bundle's
        // own baked prefix points at a different app. A hint for someone else's
        // asset is not ours to make — drop it rather than guess at a tail.
        Some(prefix) if value.starts_with(CUSTOM_APPS_SENTINEL) => {
            match value.strip_prefix(prefix) {
                Some(tail) => tail,
                None => return,
            }
        }
        // No baked prefix to anchor against, and the value is already based.
        // `first_custom_apps_prefix` reads the document's FIRST `/customer-apps/`
        // occurrence, so one that isn't a bundle base — a canonical `<link>` with
        // no trailing slash, say — yields `None` while the script tags below it
        // are still fully prefixed. Storing one would hand every consumer a path
        // to double, which is the bug this strip exists to prevent; drop it for
        // the same reason a cross-app reference is dropped.
        None if value.starts_with(CUSTOM_APPS_SENTINEL) => return,
        _ => value,
    };
    // `./assets/x.js` and `assets/x.js` are the same object.
    let normalized = value
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string();
    if normalized.is_empty() || normalized.contains("..") {
        return;
    }
    if out.iter().any(|e| e.path == normalized) {
        return;
    }
    out.push(Entry {
        path: normalized,
        kind,
    });
}

/// Pull one attribute's value out of a single tag.
///
/// `tag_lower` locates the attribute (names are case-insensitive); `tag_raw`
/// supplies the bytes (values are not). Handles double, single, and unquoted
/// forms because bundler output is not consistent about it.
fn attr_value(tag_lower: &str, tag_raw: &str, name: &str) -> Option<String> {
    // Find `name=` preceded by whitespace, so `data-src=` doesn't match `src=`.
    let needle = format!("{name}=");
    let mut from = 0usize;
    let at = loop {
        let idx = tag_lower[from..].find(&needle)? + from;
        let preceded_by_space = idx > 0
            && tag_lower[..idx]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_whitespace());
        if preceded_by_space {
            break idx;
        }
        from = idx + needle.len();
    };
    let rest_raw = &tag_raw[at + needle.len()..];
    let mut chars = rest_raw.chars();
    match chars.next()? {
        q @ ('"' | '\'') => {
            let body = &rest_raw[1..];
            let end = body.find(q)?;
            Some(body[..end].to_string())
        }
        _ => {
            let end = rest_raw
                .find(|c: char| c.is_ascii_whitespace() || c == '>')
                .unwrap_or(rest_raw.len());
            Some(rest_raw[..end].to_string())
        }
    }
}

/// Render the manifest's entries as a `Link` response header value.
///
/// `base` is the app's serve-time URL prefix (`/customer-apps/<org>/<app>/`, or
/// `/` on a custom-app subdomain) — the manifest stores bundle-relative paths
/// precisely so the same document works on both surfaces.
///
/// Returns `None` when there is nothing worth hinting, so the caller can skip
/// the header entirely rather than emit an empty one.
pub fn preload_link_header(manifest: &AssetManifest, base: &str) -> Option<String> {
    if manifest.entries.is_empty() {
        return None;
    }
    let base = if base.ends_with('/') {
        base.to_string()
    } else {
        format!("{base}/")
    };
    let mut parts: Vec<String> = Vec::new();
    let mut rendered = 0usize;
    for e in manifest.entries.iter().take(MAX_ENTRIES) {
        let (rel, as_, crossorigin, priority) = e.kind.link_attrs();
        let mut part = format!("<{base}{}>; rel={rel}", e.path);
        if let Some(a) = as_ {
            part.push_str(&format!("; as={a}"));
        }
        if crossorigin {
            part.push_str("; crossorigin");
        }
        if let Some(p) = priority {
            part.push_str(&format!("; fetchpriority={p}"));
        }
        // Stop at the byte budget rather than emitting a header an intermediary
        // will reject. Dropping the tail costs a hint; exceeding nginx's
        // header-block limit costs the whole response — the entries are in
        // document order, so what survives is the part that mattered most.
        let separator = if parts.is_empty() { 0 } else { ", ".len() };
        if rendered + separator + part.len() > MAX_LINK_HEADER_BYTES {
            break;
        }
        rendered += separator + part.len();
        parts.push(part);
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, body: &str) -> (String, Vec<u8>) {
        (path.to_string(), body.as_bytes().to_vec())
    }

    #[test]
    fn parses_vite_style_entry_html() {
        let html = r#"<!doctype html><html><head>
            <link rel="stylesheet" crossorigin href="/assets/index-9f3a.css">
            <script type="module" crossorigin src="/assets/index-1b2c.js"></script>
            <link rel="modulepreload" crossorigin href="/assets/vendor-77aa.js">
        </head><body><div id="root"></div></body></html>"#;
        let m = build_from_files("b1", &[f("index.html", html)], ClientPrefs::default());
        assert_eq!(
            m.entries,
            vec![
                Entry {
                    path: "assets/index-9f3a.css".into(),
                    kind: EntryKind::Style
                },
                Entry {
                    path: "assets/index-1b2c.js".into(),
                    kind: EntryKind::Module
                },
                Entry {
                    path: "assets/vendor-77aa.js".into(),
                    kind: EntryKind::Module
                },
            ]
        );
    }

    /// The shape that ships from `oxyc publish` for a path-mounted app: Vite
    /// built with `base: /customer-apps/<org>/<slug>/`, so every reference is
    /// root-absolute AND already carries the mount prefix.
    ///
    /// Regression: the entries were stored with the prefix still attached, and
    /// every consumer re-bases against the app root — so the worker fetched
    /// `<base>/customer-apps/<org>/<slug>/assets/x.js`, got a 404 for each one,
    /// and left an EMPTY precache behind while the `Link:` header preloaded the
    /// same doubled URLs on every navigation. Both read as working: the page
    /// itself renders from the correct paths in the HTML.
    #[test]
    fn strips_the_base_a_path_mounted_bundle_was_built_with() {
        let html = r#"<!doctype html><html><head>
            <link rel="stylesheet" href="/customer-apps/acme/sales/assets/index-9f3a.css">
            <script type="module" src="/customer-apps/acme/sales/assets/index-1b2c.js"></script>
        </head><body></body></html>"#;
        let m = build_from_files("b1", &[f("index.html", html)], ClientPrefs::default());
        assert_eq!(
            m.entries,
            vec![
                Entry {
                    path: "assets/index-9f3a.css".into(),
                    kind: EntryKind::Style
                },
                Entry {
                    path: "assets/index-1b2c.js".into(),
                    kind: EntryKind::Module
                },
            ]
        );

        // The property that actually matters, stated the way the two consumers
        // use it — neither may produce a doubled prefix.
        let base = "/customer-apps/acme/sales/";
        for e in &m.entries {
            let resolved = format!("{base}{}", e.path);
            assert_eq!(
                resolved.matches("/customer-apps/").count(),
                1,
                "re-basing {} doubled the mount prefix",
                e.path
            );
        }
    }

    /// `first_custom_apps_prefix` reads the document's FIRST `/customer-apps/`
    /// occurrence. A canonical link without a trailing slash is not a bundle
    /// base, so it yields `None` — while the script tags below it are still
    /// fully prefixed. Storing those unanchored would reintroduce the doubling
    /// this PR fixes, just narrower.
    #[test]
    fn drops_a_prefixed_reference_it_cannot_anchor() {
        let html = r#"<!doctype html><html><head>
            <link rel="canonical" href="https://app.oxygen-hq.com/customer-apps/acme/sales">
            <script type="module" src="/customer-apps/acme/sales/assets/index-1b2c.js"></script>
        </head></html>"#;
        // Precondition: this document really does defeat the prefix reader —
        // without it the test would pass for the wrong reason.
        assert_eq!(
            crate::server::api::custom_apps_serve::rewrite::first_custom_apps_prefix(html),
            None,
            "fixture no longer exercises the unanchored path"
        );
        let m = build_from_files("b1", &[f("index.html", html)], ClientPrefs::default());
        assert_eq!(m.entries, vec![]);
    }

    /// The invariant both consumers actually depend on, stated once instead of
    /// enumerated per base x surface: an entry names a file in the bundle.
    ///
    /// `assets` is derived from the tarball's own paths, so entries being a
    /// subset of it is exactly what makes `ASSET_BASE + path` resolve in the
    /// worker AND what makes the publish-time cache warm in
    /// `custom_apps_publish` match `path.trim_start_matches('/') == entry.path`.
    /// The doubled prefix broke all three at once.
    #[test]
    fn every_entry_names_a_file_the_bundle_actually_ships() {
        let html = r#"<!doctype html><html><head>
            <link rel="stylesheet" href="/customer-apps/acme/sales/assets/index-9f3a.css">
            <script type="module" src="/customer-apps/acme/sales/assets/index-1b2c.js"></script>
        </head></html>"#;
        let m = build_from_files(
            "b1",
            &[
                f("index.html", html),
                f("assets/index-9f3a.css", "body{}"),
                f("assets/index-1b2c.js", "export{}"),
            ],
            ClientPrefs::default(),
        );
        assert!(!m.entries.is_empty(), "fixture produced nothing to check");
        for e in &m.entries {
            assert!(
                m.assets.contains(&e.path),
                "entry {} is not among the bundle's files {:?}",
                e.path,
                m.assets
            );
        }
    }

    /// A bundle built with the default base `/` is the OTHER root-absolute
    /// shape, and it must keep behaving as it did: the tail is the value minus
    /// its slash, because serve-time prefixing does exactly that too.
    #[test]
    fn default_base_bundle_still_keeps_the_whole_path() {
        let html = r#"<html><head><script type="module" src="/assets/main-XYZ.js"></script></head></html>"#;
        let m = build_from_files("b1", &[f("index.html", html)], ClientPrefs::default());
        assert_eq!(
            m.entries,
            vec![Entry {
                path: "assets/main-XYZ.js".into(),
                kind: EntryKind::Module
            }]
        );
    }

    /// Stripping is anchored to THIS bundle's baked prefix, not to any
    /// `/customer-apps/` path. A reference to a different app is someone
    /// else's asset — hinting it would preload across an app boundary.
    #[test]
    fn does_not_hint_an_asset_belonging_to_a_different_app() {
        let html = r#"<html><head>
            <script type="module" src="/customer-apps/acme/sales/assets/ours.js"></script>
            <link rel="stylesheet" href="/customer-apps/other/app/assets/theirs.css">
        </head></html>"#;
        let m = build_from_files("b1", &[f("index.html", html)], ClientPrefs::default());
        assert_eq!(
            m.entries,
            vec![Entry {
                path: "assets/ours.js".into(),
                kind: EntryKind::Module
            }]
        );
    }

    /// Next.js emits classic `<script src>` under `_next/static/`, single
    /// quotes and unquoted attributes both show up in hand-written shells, and
    /// `data-src` must not be mistaken for `src`.
    #[test]
    fn parses_the_other_attribute_shapes() {
        let html = "<html><head>\
            <script src='/_next/static/chunks/main-abc.js'></script>\
            <script data-src=/decoy.js src=/_next/static/chunks/pages-def.js></script>\
            </head></html>";
        let m = build_from_files("b1", &[f("index.html", html)], ClientPrefs::default());
        assert_eq!(
            m.entries
                .iter()
                .map(|e| e.path.as_str())
                .collect::<Vec<_>>(),
            vec![
                "_next/static/chunks/main-abc.js",
                "_next/static/chunks/pages-def.js"
            ]
        );
        assert!(m.entries.iter().all(|e| e.kind == EntryKind::Script));
    }

    /// A hint we cannot honour is worse than no hint: it costs a connection and
    /// a console warning, and for a root-absolute path built against a
    /// different base it preloads a URL that 404s.
    #[test]
    fn skips_references_that_are_not_ours_to_hint() {
        let html = r#"<html><head>
            <script type="module" src="https://cdn.example.com/x.js"></script>
            <script type="module" src="//cdn.example.com/y.js"></script>
            <link rel="stylesheet" href="data:text/css,body{}">
            <link rel="icon" href="/favicon.ico">
            <script type="module" src="assets/ok.js"></script>
        </head></html>"#;
        let m = build_from_files("b1", &[f("index.html", html)], ClientPrefs::default());
        assert_eq!(
            m.entries,
            vec![Entry {
                path: "assets/ok.js".into(),
                kind: EntryKind::Module
            }]
        );
    }

    #[test]
    fn entry_list_is_capped_and_deduped() {
        let mut html = String::from("<html><head>");
        for i in 0..40 {
            html.push_str(&format!(
                "<script type=\"module\" src=\"/assets/c{i}.js\"></script>"
            ));
        }
        // A repeat of one already listed must not consume a slot.
        html.push_str("<script type=\"module\" src=\"/assets/c0.js\"></script></head></html>");
        let m = build_from_files("b1", &[f("index.html", &html)], ClientPrefs::default());
        assert_eq!(m.entries.len(), MAX_ENTRIES);
        assert_eq!(m.entries[0].path, "assets/c0.js");
    }

    #[test]
    fn assets_list_holds_only_hashed_paths_and_is_stable() {
        let files = vec![
            f("index.html", "<html><head></head></html>"),
            f("assets/b.js", "b"),
            f("assets/a.css", "a"),
            f("assets/a.css.br", "brotli"),
            f("_next/static/chunk.js", "c"),
            f("favicon.ico", "i"),
            f("__oxy/asset-manifest.json", "{}"),
        ];
        let m = build_from_files("b1", &files, ClientPrefs::default());
        assert_eq!(
            m.assets,
            vec!["_next/static/chunk.js", "assets/a.css", "assets/b.js"],
            "only content-hashed dirs, sorted, no .br siblings, no reserved paths"
        );
    }

    #[test]
    fn preload_header_renders_rel_and_as_per_kind() {
        let m = AssetManifest {
            schema_version: SCHEMA_VERSION,
            build_id: "b1".into(),
            entries: vec![
                Entry {
                    path: "assets/x.css".into(),
                    kind: EntryKind::Style,
                },
                Entry {
                    path: "assets/x.js".into(),
                    kind: EntryKind::Module,
                },
                Entry {
                    path: "assets/x.woff2".into(),
                    kind: EntryKind::Font,
                },
            ],
            assets: vec![],
            client: ClientPrefs::default(),
        };
        let header = preload_link_header(&m, "/customer-apps/acme/sales").expect("header");
        assert_eq!(
            header,
            "</customer-apps/acme/sales/assets/x.css>; rel=preload; as=style; fetchpriority=high, \
             </customer-apps/acme/sales/assets/x.js>; rel=modulepreload; fetchpriority=high, \
             </customer-apps/acme/sales/assets/x.woff2>; rel=preload; as=font; crossorigin"
        );
        // A subdomain-served app has base `/`; the same manifest must work.
        let header = preload_link_header(&m, "/").expect("header");
        assert!(header.starts_with("</assets/x.css>; rel=preload; as=style; fetchpriority=high"));
    }

    /// The entry count was justified by nginx's header limit but does not by
    /// itself respect it — 16 max-length paths render to ~9–10 KiB. Exceeding a
    /// proxy's header block 502s the shell rather than dropping the hint, so the
    /// rendered bytes get their own budget.
    #[test]
    fn preload_header_is_bounded_in_bytes_not_only_in_entries() {
        let long = "a".repeat(MAX_PATH_LEN);
        let m = AssetManifest {
            schema_version: SCHEMA_VERSION,
            build_id: "b1".into(),
            entries: (0..MAX_ENTRIES)
                .map(|i| Entry {
                    path: format!("assets/{i}-{long}.js"),
                    kind: EntryKind::Module,
                })
                .collect(),
            assets: vec![],
            client: ClientPrefs::default(),
        };
        let header = preload_link_header(&m, "/customer-apps/acme/sales").expect("header");
        assert!(
            header.len() <= MAX_LINK_HEADER_BYTES,
            "rendered {} bytes, over the {MAX_LINK_HEADER_BYTES} budget",
            header.len()
        );
        // Truncated, not emptied — the earliest entries are the critical ones.
        assert!(header.contains("assets/0-"), "kept the first entry");
    }

    #[test]
    fn preload_header_is_absent_when_there_is_nothing_to_hint() {
        let m = build_from_files(
            "b1",
            &[f("index.html", "<html><head></head></html>")],
            ClientPrefs::default(),
        );
        assert!(preload_link_header(&m, "/").is_none());
    }

    /// The worker serves anything in `assets` cache-first and never
    /// revalidates it. That is only sound while the origin agrees the same
    /// paths are `immutable` — see `cache_control_for`. Two lists, one rule.
    #[test]
    fn immutable_prefixes_agree_with_the_origin_cache_policy() {
        use crate::server::api::custom_apps_serve::cache_control_for_test_only as cache_control_for;
        for p in ["assets/x.js", "_next/static/x.js"] {
            assert!(is_immutable_asset_path(p));
            assert!(
                cache_control_for(p, std::path::Path::new(p)).contains("immutable"),
                "{p} is precacheable here but not immutable at the origin"
            );
        }
        for p in ["favicon.ico", "manifest.webmanifest", "sw-custom.js"] {
            assert!(!is_immutable_asset_path(p));
            assert!(
                !cache_control_for(p, std::path::Path::new(p)).contains("immutable"),
                "{p} is immutable at the origin but not precacheable here"
            );
        }
    }

    #[test]
    fn client_prefs_default_on_and_opt_out_explicitly() {
        assert_eq!(
            ClientPrefs::from_manifest_json(None),
            ClientPrefs::default()
        );
        assert!(ClientPrefs::default().service_worker && ClientPrefs::default().analytics);

        let opted_out = serde_json::json!({
            "schemaVersion": 2,
            "slug": "x",
            "performance": { "serviceWorker": false },
            "analytics": false
        });
        let prefs = ClientPrefs::from_manifest_json(Some(&opted_out));
        assert!(!prefs.service_worker);
        assert!(!prefs.analytics);

        // A manifest that says nothing, or says something unreadable, gets the
        // defaults rather than an error — an optional hint must not be able to
        // change whether a publish succeeds.
        let noise = serde_json::json!({ "performance": "yes please", "analytics": 7 });
        assert_eq!(
            ClientPrefs::from_manifest_json(Some(&noise)),
            ClientPrefs::default()
        );
    }

    /// A manifest published before `client` existed must read as "both on",
    /// not fail to parse — every build already in an object store is one.
    #[test]
    fn manifest_without_client_block_deserializes_to_defaults() {
        let older = r#"{"schemaVersion":1,"buildId":"b1","entries":[],"assets":[]}"#;
        let m: AssetManifest = serde_json::from_str(older).expect("older manifest still parses");
        assert_eq!(m.client, ClientPrefs::default());
    }

    /// The worker reads `schemaVersion` and `buildId`; a snake_case document
    /// would leave it silently unable to precache anything.
    #[test]
    fn manifest_serializes_in_the_casing_the_worker_reads() {
        let m = build_from_files(
            "b1",
            &[f("index.html", "<html><head></head></html>")],
            ClientPrefs::default(),
        );
        let json = serde_json::to_string(&m).expect("serialize");
        assert!(json.contains("\"schemaVersion\":1"), "{json}");
        assert!(json.contains("\"buildId\":\"b1\""), "{json}");
        assert!(json.contains("\"serviceWorker\":true"), "{json}");
    }

    /// The one entry point both `oxyc publish` and `oxy seed` use, so the seeded
    /// example app is on the same fast path as a real publish.
    #[test]
    fn install_into_reserves_the_namespace_and_writes_the_manifest() {
        let mut files = vec![
            f(
                "index.html",
                "<html><head><script type=\"module\" src=\"/assets/a.js\"></script></head></html>",
            ),
            f("assets/a.js", "x"),
            // An author file under the reserved prefix — including one at the
            // manifest's own path, which is the collision that matters.
            f("__oxy/asset-manifest.json", "{\"evil\":true}"),
            f("__oxy/sw.js", "self.addEventListener('fetch', () => {})"),
        ];
        let manifest = install_into(&mut files, "b9", None);

        assert_eq!(manifest.build_id, "b9");
        assert_eq!(manifest.entries.len(), 1);

        let reserved: Vec<&str> = files
            .iter()
            .map(|(p, _)| p.as_str())
            .filter(|p| is_reserved_platform_path(p))
            .collect();
        assert_eq!(
            reserved,
            vec![ASSET_MANIFEST_PATH],
            "ours is the only file left under the prefix"
        );
        let written = files
            .iter()
            .find(|(p, _)| p == ASSET_MANIFEST_PATH)
            .expect("manifest written");
        let parsed: AssetManifest = serde_json::from_slice(&written.1).expect("round-trips");
        assert_eq!(
            parsed, manifest,
            "the bytes written are the manifest returned"
        );
    }

    #[test]
    fn reserved_prefix_matches_case_insensitively() {
        assert!(is_reserved_platform_path("__oxy/sw.js"));
        assert!(is_reserved_platform_path("/__oxy/asset-manifest.json"));
        assert!(is_reserved_platform_path("__OXY/sw.js"));
        assert!(!is_reserved_platform_path("assets/__oxy/x.js"));
        assert!(!is_reserved_platform_path("__oxygen/x.js"));
    }
}
