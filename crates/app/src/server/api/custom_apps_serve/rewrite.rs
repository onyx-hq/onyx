//! HTML byte transforms applied to a served custom-app entry document:
//! base-path rewriting for bundles built with a mismatched (or default)
//! base, and `window.__OXY_APP__` runtime-identity injection.

use std::path::Path as StdPath;

use uuid::Uuid;

use super::sources::AppRuntimeConfig;

/// Fix up the bundle's `index.html` when its baked base path doesn't
/// match the URL we're serving from. Common cause: an engineer
/// rebuilt locally without `OXY_APP_BASE_PATH` set, or renamed the
/// app slug after the bundle was built. Either way the bundle's
/// hardcoded `<script src="/customer-apps/<old>/<old>/...">` won't
/// resolve to anything under the current URL prefix and the browser
/// 404s every asset.
///
/// We scan for the first absolute `/customer-apps/<org>/<slug>/`
/// path in the HTML; if it differs from the prefix this request came
/// in on, we string-replace every occurrence. Vite/Astro/Rsbuild
/// SPAs load their entry script from this baked URL and resolve all
/// further chunks relative to that script's URL, so rewriting the
/// HTML alone is enough to fix the load chain.
///
/// Logs a warning on rewrite — the operator should still set
/// `OXY_APP_BASE_PATH` correctly at build time so this is a one-time
/// rescue and not a steady-state crutch.
pub(super) fn rewrite_bundle_base_path(
    bytes: &[u8],
    expected_prefix: &str,
    app_id: Uuid,
) -> Vec<u8> {
    let Ok(html) = std::str::from_utf8(bytes) else {
        // Binary content claiming to be HTML — leave it alone.
        return bytes.to_vec();
    };

    // Case 1: bundle was built with SOME `/customer-apps/<org>/<slug>/`
    // base path baked in (e.g. via `OXY_APP_BASE_PATH` or Vite's
    // `base:` option). If it doesn't match the prefix this request
    // came in on (operator rebuilt locally, renamed the slug, etc.),
    // string-replace to the correct one.
    if let Some(baked) = first_custom_apps_prefix(html) {
        if baked == expected_prefix {
            return bytes.to_vec();
        }
        tracing::warn!(
            app_id = %app_id,
            baked = %baked,
            expected = %expected_prefix,
            "Bundle base path mismatch — rewriting index.html. \
             Permanent fix: rebuild with OXY_APP_BASE_PATH={expected_prefix} \
             or rename the app slug to match the baked prefix."
        );
        return html.replace(&baked, expected_prefix).into_bytes();
    }

    // Case 2: bundle was built with default base `/` (no env var, no
    // Vite `base:` configured). The HTML has bare absolute paths like
    // `<script src="/assets/index-XXX.js">`. The browser resolves
    // those against the document origin (e.g. `:5173` in dev), not
    // under `/customer-apps/<org>/<slug>/`, and every chunk 404s.
    //
    // Walk the HTML once, prefixing the value of any `src=` / `href=`
    // attribute that starts with a single `/` (not `//`, not already
    // `/customer-apps/`). Catches Vite, Astro, Rsbuild, Next-export
    // default output.
    let rewritten = prefix_bare_absolute_paths(html, expected_prefix);
    if rewritten.len() != html.len() {
        tracing::warn!(
            app_id = %app_id,
            expected = %expected_prefix,
            "Bundle built with default base path — prefixing bare absolute \
             asset URLs at serve time. Permanent fix: install \
             `@oxy-hq/vite-plugin` (or rebuild with \
             OXY_APP_BASE_PATH={expected_prefix})."
        );
        return rewritten.into_bytes();
    }
    bytes.to_vec()
}

/// Walk `html`, rewriting `src="/…"` and `href="/…"` attribute values
/// to `src="<prefix>…"` so a bundle built with the bundler's default
/// base path (`/`) still resolves all its chunks under the
/// customer-apps subpath.
///
/// Skips values that:
///   - don't start with `/` (relative or fully-qualified URLs),
///   - start with `//` (protocol-relative — those go to a different host),
///   - already start with `/customer-apps/` (defensive — this branch
///     only runs when no such prefix was found by [`first_custom_apps_prefix`],
///     but checking again is cheap insurance).
///
/// String-based rather than HTML-parser-based because the input is
/// always a bundler's generated `index.html` — there's no malformed
/// markup, no scripts to dodge, and we only touch attribute values of
/// `src` and `href`. A full parser would be ~3KB of dep weight for
/// zero functional gain here.
///
/// **Known limitations** (acceptable trade-offs, not bugs):
///   - Matches attribute-shaped substrings even when they appear
///     inside inline `<script>` JS string literals (e.g.
///     `const el = '<img src="/foo">'`). For SPA index.html files
///     this is benign because such literals are rare and the rewrite
///     is what the JS would want anyway, but a downstream consumer
///     embedding raw HTML strings in a script could see surprising
///     URL rewrites.
///   - Only matches `src=` and `href=` attribute names. Does NOT
///     touch `srcset=` (responsive images), `poster=` (videos),
///     `data=` (object tags), or CSS `url(/…)` references inside
///     inline `<style>` blocks. Modern bundlers don't emit those in
///     index.html, but a hand-written bundle that does will need to
///     bake the prefix at build time instead.
fn prefix_bare_absolute_paths(html: &str, prefix: &str) -> String {
    let prefix_trimmed = prefix.trim_end_matches('/');
    let mut out = String::with_capacity(html.len() + 256);
    let mut rest = html;

    loop {
        // Find the next ` src=` or ` href=`, whichever comes first.
        // Leading-space requirement avoids matching inside class names,
        // partial words, or JS string literals embedded in `<script>`.
        let next = [" src=", " href="]
            .iter()
            .filter_map(|pat| rest.find(pat).map(|p| (p, pat.len())))
            .min_by_key(|&(p, _)| p);
        let Some((attr_at, name_len)) = next else {
            out.push_str(rest);
            return out;
        };

        let after_eq = attr_at + name_len;
        out.push_str(&rest[..after_eq]);
        rest = &rest[after_eq..];

        // Attribute value must be quoted (single or double). Unquoted
        // attribute values are technically valid HTML but no bundler
        // emits them; skip the rewrite in that case.
        let Some(quote_char) = rest.chars().next() else {
            return out;
        };
        if quote_char != '"' && quote_char != '\'' {
            continue;
        }
        out.push(quote_char);
        rest = &rest[1..];

        let Some(close) = rest.find(quote_char) else {
            // Unterminated quoted value — broken HTML; pass through
            // unchanged from this point on rather than risk corruption.
            out.push_str(rest);
            return out;
        };
        let value = &rest[..close];
        if value.starts_with('/')
            && !value.starts_with("//")
            && !value.starts_with("/customer-apps/")
        {
            out.push_str(prefix_trimmed);
        }
        out.push_str(value);
        out.push(quote_char);
        rest = &rest[close + 1..];
    }
}

/// Path prefix every path-mounted custom app is served under.
///
/// Shared with the asset manifest, which must strip exactly the prefix this
/// module would rewrite — if the two disagreed about what a baked base looks
/// like, the manifest would advertise paths the serve path never produces.
pub(crate) const CUSTOM_APPS_SENTINEL: &str = "/customer-apps/";

/// Find the first `/customer-apps/<org>/<slug>/` substring in the
/// HTML. Returns the full matched prefix including the trailing
/// slash, or `None` if there isn't one or the surrounding characters
/// don't look like an attribute value.
pub(crate) fn first_custom_apps_prefix(html: &str) -> Option<String> {
    let idx = html.find(CUSTOM_APPS_SENTINEL)?;
    let after = &html[idx + CUSTOM_APPS_SENTINEL.len()..];

    // Read up to two more path segments. Both must be non-empty and
    // composed of URL-safe slug chars; the second is followed by a
    // closing slash that bounds the prefix.
    let mut segments: [Option<&str>; 2] = [None, None];
    let mut cursor = 0usize;
    for slot in segments.iter_mut() {
        let rest = &after[cursor..];
        let slash_at = rest.find('/')?;
        let segment = &rest[..slash_at];
        if segment.is_empty() || !segment.chars().all(is_slug_char) {
            return None;
        }
        *slot = Some(segment);
        cursor += slash_at + 1;
    }

    let (Some(org), Some(slug)) = (segments[0], segments[1]) else {
        return None;
    };
    Some(format!("{CUSTOM_APPS_SENTINEL}{org}/{slug}/"))
}

fn is_slug_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'
}

/// Splice `<script>window.__OXY_APP__ = JSON.parse(…)</script>` into the
/// HTML's `<head>` so the bundle's SDK reads identity from a runtime
/// global instead of build-time env vars.
///
/// We `JSON.parse` a JSON-string literal rather than emit an object
/// literal — JS engines parse JSON ~2× faster, and we only have to escape
/// for one syntactic layer (string contents) instead of two (object +
/// string).
///
/// Insertion strategy: prefer `</head>` (puts the global early, before
/// any deferred scripts run). If the bundle has no `</head>`, log a
/// warning and serve untouched — the bundle was hand-rolled or pre-Next
/// and the admin needs to add one.
/// Splice the platform's browser-side bootstrap into `<head>`: the app's
/// runtime identity (`window.__OXY_APP__`) followed by the client runtime that
/// registers the service worker and auto-instruments the page
/// (`custom_apps_client`).
///
/// One injection point rather than two because they are one contract — the
/// runtime is inert without the identity object, and a bundle with no `<head>`
/// must lose both together rather than half of each.
pub(crate) fn inject_app_config(
    bytes: &[u8],
    runtime: &AppRuntimeConfig,
    path: &StdPath,
) -> Vec<u8> {
    let json = match serde_json::to_string(runtime) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!("Failed to serialise runtime config for {path:?}: {e}");
            return bytes.to_vec();
        }
    };
    let escaped_for_js_string = json
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        // </script> in the JSON would otherwise end the script block.
        // The HTML spec only cares about a literal `</` after `script`,
        // but it's cheaper to escape both directions of the slash.
        .replace("</", "<\\/");
    // The identity object first, then the platform runtime that reads it. The
    // runtime bails out if `window.__OXY_APP__` is absent, so the order is
    // load-bearing rather than cosmetic — and both are inline so a navigation
    // costs no extra round trip to bootstrap either one.
    // The manifest link, ahead of the scripts. Without it the synthesised
    // manifest is unreachable — a browser only looks for one it was pointed at,
    // so serving it and never linking it is the same as not having it.
    //
    // ABSOLUTE, built from `base_path`. A document-relative href resolves
    // against the document's own URL, and this runs on every served document —
    // the SPA fallback hands the shell to deep links too, and there is no
    // trailing-slash redirect. So `/customer-apps/acme/ops/orders/42` would ask
    // for `…/orders/__oxy/manifest.webmanifest`, and `/customer-apps/acme/ops`
    // with no slash would ask for `/customer-apps/acme/__oxy/…` — where the
    // router reads `__oxy` as the app slug. Both 404, and a manifest that 404s
    // does not report anything: the browser just never offers to install.
    //
    // One absolute string is enough because `base_path` is the same value on
    // both surfaces (`sources.rs`) — the same reason `runtime.js` builds the
    // worker URL as `base + "__oxy/sw.js"`.
    //
    // `crossorigin="use-credentials"` because this manifest is behind the app's
    // auth gate. `rel=manifest` is the one link type whose CORS default is
    // `anonymous`, so without it the fetch carries no session cookie, follows
    // the login redirect, and parses as HTML — silently, with no install
    // prompt. Same-origin credentialed requests need nothing extra in return,
    // so it is a no-op where the gate does not bite.
    let manifest_link = format!(
        r#"<link rel="manifest" href="{}{}" crossorigin="use-credentials">"#,
        runtime.base_path,
        crate::server::api::custom_apps_client::WEB_MANIFEST_PATH
    );
    let snippet = format!(
        "{manifest_link}<script>window.__OXY_APP__=JSON.parse('{escaped_for_js_string}');</script>{}",
        crate::server::api::custom_apps_client::runtime_script_tag()
    );

    let needle = b"</head>";
    let Some(pos) = find_subsequence(bytes, needle) else {
        tracing::warn!(
            "No </head> in {path:?}; skipping window.__OXY_APP__ injection. \
             Bundles must include a <head>…</head> for runtime identity injection."
        );
        return bytes.to_vec();
    };
    let mut out = Vec::with_capacity(bytes.len() + snippet.len());
    out.extend_from_slice(&bytes[..pos]);
    out.extend_from_slice(snippet.as_bytes());
    out.extend_from_slice(&bytes[pos..]);
    out
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::super::headers::{VersionPin, etag_for};

    /// Every cache-policy test in this module pins the UNVERSIONED rule — the
    /// path-and-resolved-file decision a request with no `?v=` gets. The
    /// version pin has its own tests beside `headers::cache_control_for`.
    fn cache_control_for(request_path: &str, file_path: &std::path::Path) -> &'static str {
        super::super::headers::cache_control_for(request_path, file_path, VersionPin::default())
    }
    use super::*;

    fn fake_runtime() -> AppRuntimeConfig {
        AppRuntimeConfig {
            app_id: Uuid::nil(),
            slug: "acme-analytics".to_string(),
            org_id: Uuid::nil(),
            org_slug: "acme".to_string(),
            project_id: Uuid::nil(),
            branch: "main".to_string(),
            api_base_url: String::new(),
            base_path: String::from("/customer-apps/acme/acme-analytics/"),
            service_worker: true,
            analytics: true,
            build_id: String::new(),
        }
    }

    /// The manifest link must resolve to the SAME URL from any served
    /// document, not just the app root.
    ///
    /// `inject_app_config` runs on every HTML response, and the SPA fallback
    /// hands the shell to deep links, so a document-relative href would resolve
    /// against `/customer-apps/acme/acme-analytics/orders/42` and ask for
    /// `…/orders/__oxy/manifest.webmanifest`. A manifest that 404s reports
    /// nothing — the browser just never offers to install.
    #[test]
    fn manifest_link_is_absolute_so_deep_links_resolve_it() {
        let rt = fake_runtime();
        let html = b"<html><head></head><body/></html>";
        let expected = format!(r#"href="{}__oxy/manifest.webmanifest""#, rt.base_path);
        for served_at in [
            "index.html",
            "orders/42/index.html",
            "deeply/nested/route/index.html",
        ] {
            let out = inject_app_config(html, &rt, std::path::Path::new(served_at));
            let s = std::str::from_utf8(&out).unwrap();
            assert!(
                s.contains(&expected),
                "document served at {served_at} got a link that does not resolve: {s}"
            );
        }
    }

    /// The manifest sits behind the app's auth gate, and `rel=manifest` is the
    /// one link type whose CORS default is `anonymous` — without this the fetch
    /// carries no cookie, follows the login redirect, and parses as HTML.
    #[test]
    fn manifest_link_requests_credentials() {
        let out = inject_app_config(
            b"<html><head></head></html>",
            &fake_runtime(),
            std::path::Path::new("index.html"),
        );
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains(r#"crossorigin="use-credentials""#));
    }

    #[test]
    fn inject_app_config_splices_before_closing_head() {
        let html = b"<!doctype html><html><head><title>X</title></head><body/></html>";
        let out = inject_app_config(html, &fake_runtime(), std::path::Path::new("test.html"));
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("window.__OXY_APP__=JSON.parse"));
        // Verify ordering: script lands inside <head>, before </head>.
        let script_pos = s.find("window.__OXY_APP__").unwrap();
        let close_head_pos = s.find("</head>").unwrap();
        assert!(script_pos < close_head_pos);
        // Original markup before <head>'s close is preserved.
        assert!(s.contains("<title>X</title>"));
    }

    #[test]
    fn inject_app_config_escapes_closing_script_tag() {
        // Pretend an app name happens to contain `</script>` — without
        // escaping, the inline JSON would let an HTML parser break out of
        // the script block.
        let mut rt = fake_runtime();
        rt.slug = "evil</script><img src=x>".to_string();
        let html = b"<html><head></head></html>";
        let out = inject_app_config(html, &rt, std::path::Path::new("x.html"));
        let s = std::str::from_utf8(&out).unwrap();
        // The literal `</script>` must NOT appear in the injected payload.
        // Two legitimate closes are expected — the identity tag's own, and the
        // client-runtime tag spliced next to it. A third means the escaped JSON
        // broke out of its element, which is the whole point of the escaping.
        let script_count = s.matches("</script>").count();
        assert_eq!(
            script_count, 2,
            "expected the identity tag + the client runtime tag, and nothing else"
        );
    }

    #[test]
    fn inject_app_config_no_head_passes_through() {
        let html = b"<html><body>bare</body></html>";
        let out = inject_app_config(html, &fake_runtime(), std::path::Path::new("x.html"));
        assert_eq!(out, html);
    }

    #[test]
    fn find_subsequence_finds_match() {
        assert_eq!(find_subsequence(b"abcdefg", b"cde"), Some(2));
        assert_eq!(find_subsequence(b"abcdefg", b"xyz"), None);
        assert_eq!(find_subsequence(b"", b"x"), None);
    }

    #[test]
    fn prefix_bare_absolute_paths_rewrites_vite_default_output() {
        // Vite's `pnpm build` with no `base:` option produces bare
        // absolute asset paths. Without prefixing them at serve time
        // the browser requests them from the wrong origin and 404s.
        let html = r#"<!doctype html><html><head>
<script type="module" crossorigin src="/assets/index-DA-MTpVz.js"></script>
<link rel="stylesheet" crossorigin href="/assets/style-XXX.css">
</head><body></body></html>"#;
        let out = prefix_bare_absolute_paths(html, "/customer-apps/acme/store-pulse/");
        assert!(
            out.contains("src=\"/customer-apps/acme/store-pulse/assets/index-DA-MTpVz.js\""),
            "expected script src to be prefixed, got: {out}"
        );
        assert!(
            out.contains("href=\"/customer-apps/acme/store-pulse/assets/style-XXX.css\""),
            "expected link href to be prefixed, got: {out}"
        );
    }

    #[test]
    fn prefix_bare_absolute_paths_leaves_protocol_relative_alone() {
        // `//cdn.example.com/x.js` points at a different origin
        // entirely — prefixing it would corrupt the URL.
        let html = r#"<script src="//cdn.example.com/lib.js"></script>"#;
        let out = prefix_bare_absolute_paths(html, "/customer-apps/acme/x/");
        assert_eq!(out, html);
    }

    #[test]
    fn prefix_bare_absolute_paths_leaves_external_urls_alone() {
        let html = r#"<link rel="icon" href="https://acme.com/favicon.ico">"#;
        let out = prefix_bare_absolute_paths(html, "/customer-apps/acme/x/");
        assert_eq!(out, html);
    }

    #[test]
    fn prefix_bare_absolute_paths_leaves_relative_urls_alone() {
        let html = r#"<script src="assets/foo.js"></script>"#;
        let out = prefix_bare_absolute_paths(html, "/customer-apps/acme/x/");
        assert_eq!(out, html);
    }

    #[test]
    fn prefix_bare_absolute_paths_handles_single_quotes() {
        let html = r#"<script src='/main.js'></script>"#;
        let out = prefix_bare_absolute_paths(html, "/customer-apps/acme/x/");
        assert!(out.contains("src='/customer-apps/acme/x/main.js'"));
    }

    #[test]
    fn rewrite_bundle_base_path_prefixes_default_base_bundle() {
        // End-to-end: a Vite-default bundle (no /customer-apps/ in HTML)
        // gets rescued by the prefixing pass.
        let html = br#"<html><head><script src="/assets/main-XYZ.js"></script></head></html>"#;
        let out = rewrite_bundle_base_path(html, "/customer-apps/acme/x/", Uuid::nil());
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("src=\"/customer-apps/acme/x/assets/main-XYZ.js\""));
    }

    #[test]
    fn rewrite_bundle_base_path_no_op_when_already_correct() {
        // Bundle has the right prefix; nothing should change.
        let html = br#"<html><head><script src="/customer-apps/acme/x/assets/main-XYZ.js"></script></head></html>"#;
        let out = rewrite_bundle_base_path(html, "/customer-apps/acme/x/", Uuid::nil());
        assert_eq!(out, html);
    }

    #[test]
    fn cache_control_marks_hashed_assets_immutable() {
        assert_eq!(
            cache_control_for(
                "/assets/index-DA-MTpVz.js",
                std::path::Path::new("index-DA-MTpVz.js")
            ),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control_for(
                "/_next/static/chunks/abc.js",
                std::path::Path::new("abc.js")
            ),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn cache_control_html_and_root_files_revalidate() {
        assert_eq!(
            cache_control_for("/index.html", std::path::Path::new("index.html")),
            "private, no-cache"
        );
        assert_eq!(
            cache_control_for("/favicon.ico", std::path::Path::new("favicon.ico")),
            "public, max-age=300"
        );
    }

    #[test]
    fn html_is_private_so_a_shared_cache_cannot_store_the_session_cookie() {
        // HTML responses carry a per-visitor tracking Set-Cookie. `no-cache`
        // alone still permits a SHARED cache to store them, which would hand
        // every later visitor one session id. `private` is the term that
        // forbids it — and the precondition for fronting this route with a CDN.
        for path in ["/index.html", "/", "/some/client/route"] {
            let policy = cache_control_for(path, std::path::Path::new("index.html"));
            assert!(
                policy.contains("private"),
                "HTML for {path} must be private, got {policy:?}"
            );
        }
        // Content-hashed assets carry no cookie and are identical for every
        // viewer, so they stay shared-cacheable — that is the whole CDN win.
        assert!(
            cache_control_for("/assets/main-a1b2.js", std::path::Path::new("main-a1b2.js"))
                .starts_with("public"),
            "hashed assets must remain publicly cacheable"
        );
    }

    #[test]
    fn spa_fallback_html_is_never_immutable_under_a_hashed_asset_url() {
        // The prefix rule reads the REQUESTED path, the HTML test reads the
        // RESOLVED one, and the SPA fallback is where they diverge: a miss on
        // a hashed chunk resolves to `index.html`. If the prefix won, a
        // browser or CDN would pin an HTML 200 at a module-script URL for a
        // year — and because Vite hashes by content, the next build shipping
        // that same chunk serves the cached HTML as a script and white-screens
        // the app, with no server-side remedy.
        //
        // The sibling test above loops only over paths that miss the `assets/`
        // prefix, which is exactly how this survived.
        for requested in [
            "/assets/index-abc123.js",
            "assets/index-abc123.js", // no leading slash
            "/_next/static/chunks/abc.js",
        ] {
            let policy = cache_control_for(requested, std::path::Path::new("index.html"));
            assert_eq!(
                policy, "private, no-cache",
                "SPA-fallback HTML served for {requested} must not be cacheable, got {policy:?}"
            );
        }
        // The genuine hit at the same URL keeps the immutable policy — the
        // resolved file is what changed, not the rule.
        assert_eq!(
            cache_control_for(
                "/assets/index-abc123.js",
                std::path::Path::new("index-abc123.js")
            ),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn etag_is_stable_and_weak() {
        let a = etag_for(b"<html>hello</html>");
        let b = etag_for(b"<html>hello</html>");
        let c = etag_for(b"<html>world</html>");
        assert!(a.starts_with("W/\""), "weak etag, got {a}");
        assert_eq!(a, b, "deterministic");
        assert_ne!(a, c, "content-sensitive");
    }
}
