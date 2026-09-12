//! Build-time brotli pre-compression for custom-app bundles.
//!
//! Compression moves from **once per request** to **once per publish**.
//! Before this, every asset response ran through the `CompressionLayer` on
//! `/customer-apps/{*path}` (`cli/commands/serve.rs`), so the same
//! immutable `main-a1b2c3.js` was re-compressed on every request, on every
//! replica, forever — pure CPU burn on the hot path, since a
//! content-hashed asset's bytes never change.
//!
//! At publish, [`precompressed_variants`] emits a sibling `<path>.br` object
//! for each worthwhile file. At serve, `custom_apps_serve::sources` probes
//! for that sibling when the client advertises `br` and streams it with
//! `Content-Encoding: br`, skipping the compressor entirely.
//!
//! ## Why this is safe to add to existing apps
//!
//! Builds published before this existed carry no `.br` objects. The probe
//! misses, the response falls through to the `CompressionLayer` exactly as
//! today, and the miss is remembered by `custom_apps_bundle_cache` so it
//! costs one store round-trip per object per process — not one per request.
//! No migration, no republish required.
//!
//! ## What is deliberately NOT pre-compressed
//!
//! - **HTML.** The serve path rewrites the base path and splices
//!   `window.__OXY_APP__` into every HTML response
//!   (`custom_apps_serve::rewrite`), so the bytes on the wire are not the
//!   bytes in the store. A pre-compressed HTML object would be stale by
//!   construction.
//! - **Already-compressed media** (png/jpg/webp/gif/woff/woff2). Brotli over
//!   a PNG spends CPU to grow the file.
//! - **Tiny files.** Below [`MIN_PRECOMPRESS_BYTES`] the extra store object
//!   and its probe cost more than the bytes saved.

use std::io::Cursor;

use axum::http::{HeaderMap, header};
use rayon::prelude::*;

/// Suffix for the pre-compressed sibling object. `assets/main-a1b2.js`
/// stores its brotli form at `assets/main-a1b2.js.br`.
pub const PRECOMPRESSED_SUFFIX: &str = ".br";

/// Below this, the round trip to fetch a separate object costs more than
/// the bytes saved. Roughly one TCP segment.
const MIN_PRECOMPRESS_BYTES: usize = 1024;

/// Brotli quality. Publish-time cost, serve-time benefit — but q11 is
/// 5-10x slower than q9 for ~2% more compression, and `oxyc publish` is an
/// interactive command an engineer waits on. q9 is the knob to raise if
/// bundle transfer size ever matters more than publish latency.
const BROTLI_QUALITY: i32 = 9;

/// Brotli window size (2^22 = 4 MiB), the standard default.
const BROTLI_WINDOW: i32 = 22;

/// Extensions worth pre-compressing: text-shaped bundle output that isn't
/// already an entropy-coded container. Deliberately excludes `html` — see
/// the module docs.
///
/// The serve path uses this as a **probe filter**: only a request whose path
/// could plausibly have a `.br` sibling is worth asking the store about. That
/// is what keeps an SPA route (`/orders/42`, no extension) and an image from
/// paying a doomed probe.
pub fn is_precompressible_extension(rel_path: &str) -> bool {
    let bare = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let Some((_, ext)) = bare.rsplit_once('.') else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "js" | "mjs" | "cjs" | "css" | "json" | "map" | "svg" | "txt" | "xml" | "wasm"
    )
}

/// True when a requested path names a pre-compressed sibling directly.
///
/// The siblings are an internal representation, not addressable assets: the
/// serve path picks one via `Accept-Encoding` and labels it
/// `Content-Encoding: br`. Asked for by name it would come back as ordinary
/// object bytes — raw brotli under `application/octet-stream` with no
/// encoding header, which no client can decode. Nothing links these URLs, so
/// this is tidiness rather than a live bug, but the serve path 404s them so
/// the representation stays an implementation detail.
///
/// This also covers siblings the *bundle* shipped (`vite-plugin-compression`
/// emits `assets/x.js.br`; [`precompressed_variants`] leaves those alone
/// rather than colliding with them). Such a file is still served — as the
/// brotli representation of `assets/x.js`, chosen by `Accept-Encoding`, which
/// is what those plugins exist for. Only fetching it by its literal name
/// stops working, and content negotiation is the supported way in.
///
/// That holds for a client advertising `br`. A bundle configured to ship
/// *only* the `.br` (`deleteOriginFile: true`) has no identity object, so a
/// gzip-only or `Accept-Encoding`-less client gets a 404 — but that shape was
/// never serviceable to those clients: before the literal-name 404 they fell
/// through to the SPA shell at 200, which is a worse answer than an honest
/// miss.
pub fn is_precompressed_path(rel_path: &str) -> bool {
    rel_path.ends_with(PRECOMPRESSED_SUFFIX)
}

/// True when this bundle file should get a `.br` sibling at publish time.
pub fn is_precompressible(rel_path: &str, len: usize) -> bool {
    len >= MIN_PRECOMPRESS_BYTES
        && !is_function_artifact(rel_path)
        && is_precompressible_extension(rel_path)
}

/// Oxy Functions ship inside the bundle at `functions/<name>.js` and are read
/// by the runtime straight from that build-store key
/// (`custom_apps_publish::function_artifact_key`, recorded as
/// `app_functions.artifact_key`) — never fetched over the `/customer-apps/**`
/// asset route with an `Accept-Encoding`. A `.br` sibling for one could never
/// be requested, so it would be dead bytes in every build, retained for as
/// long as the build is.
///
/// Publish-side only: the serve-side probe filter doesn't need this, because a
/// request that does reach `functions/…` simply finds no sibling and has the
/// absence cached.
fn is_function_artifact(rel_path: &str) -> bool {
    rel_path.starts_with("functions/")
}

/// True when the caller advertised brotli in `Accept-Encoding`.
///
/// `q=0` is a **refusal**, not a weak preference (RFC 9110 §12.5.3), so
/// `br;q=0` must read as "do not send me brotli". Splitting on `;` and
/// keeping only the token would silently drop that parameter and ship
/// `Content-Encoding: br` to a client that explicitly declined it.
///
/// Everything else about the q-value is ignored on purpose: we are answering
/// "may I send brotli", not ranking codings against each other, so a full
/// qvalue parser (and float comparison) would buy nothing. Only the exact
/// zero forms the grammar permits are matched.
///
/// A bare `*` is **deliberately not** honored, though RFC 9110 §12.5.3 makes
/// it mean "any coding is acceptable". Such a client falls through to the
/// `CompressionLayer` and still gets a compressed body — just built on the
/// fly — so the only cost is a missed fast path, and matching `*` would mean
/// sending brotli to a client that never named it. Conservative on purpose,
/// not an oversight.
pub fn accepts_brotli(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(coding_accepts_brotli))
}

/// One `Accept-Encoding` element: `br`, `br;q=0.5`, `gzip`, …
fn coding_accepts_brotli(part: &str) -> bool {
    let mut fields = part.split(';');
    let token = fields.next().unwrap_or("").trim();
    if !token.eq_ignore_ascii_case("br") {
        return false;
    }
    !fields.any(is_q_zero)
}

/// True for a `q=0` parameter in any form the qvalue grammar allows (up to
/// three decimal places). String comparison rather than a float parse —
/// exact, and it sidesteps float-equality entirely.
fn is_q_zero(param: &str) -> bool {
    let param = param.trim();
    let Some((name, value)) = param.split_once('=') else {
        return false;
    };
    name.trim().eq_ignore_ascii_case("q")
        && matches!(value.trim(), "0" | "0." | "0.0" | "0.00" | "0.000")
}

/// Brotli-compress one buffer. Returns `None` when compression fails or
/// fails to pay for itself (output >= input), so the caller simply doesn't
/// emit a sibling and the serve path falls back to on-the-fly compression.
fn compress(bytes: &[u8]) -> Option<Vec<u8>> {
    // Struct-update rather than assign-after-default: `clippy::
    // field_reassign_with_default` rejects the latter.
    let params = brotli::enc::BrotliEncoderParams {
        quality: BROTLI_QUALITY,
        lgwin: BROTLI_WINDOW,
        ..Default::default()
    };
    let mut out = Vec::with_capacity(bytes.len() / 3);
    brotli::BrotliCompress(&mut Cursor::new(bytes), &mut out, &params).ok()?;
    (out.len() < bytes.len()).then_some(out)
}

/// Build the `.br` siblings for a bundle's files.
///
/// Returns only the new `(path, bytes)` pairs — the caller appends them to
/// the file list handed to the build store. CPU-bound: callers run this
/// inside `spawn_blocking`, never on a Tokio worker.
///
/// Fanned out with rayon because this is the cost an engineer waits on at
/// `oxyc publish`. Files compress independently, so the work is embarrassingly
/// parallel; parallelising it is most of what buys back the headroom that
/// [`BROTLI_QUALITY`] spends. Output order is not meaningful — the caller
/// appends these to a file list that the store writes key by key.
pub fn precompressed_variants(files: &[(String, Vec<u8>)]) -> Vec<(String, Vec<u8>)> {
    // A bundle may already ship its own siblings: `vite-plugin-compression`
    // and friends emit `assets/x.js.br` next to `assets/x.js`, and those
    // arrive in `files`. Generating ours too would put two entries under one
    // key — which `put_build` resolves by last-write-wins, and since it went
    // concurrent that is a race rather than a deterministic outcome. Both
    // bodies are brotli of the same source so either would serve correctly,
    // but a silent nondeterminism is worth one lookup to avoid. Theirs wins:
    // it is already in the file list and may have been built at a quality we
    // have no reason to second-guess.
    let existing: std::collections::HashSet<&str> =
        files.iter().map(|(path, _)| path.as_str()).collect();
    files
        .par_iter()
        .filter(|(path, bytes)| is_precompressible(path, bytes.len()))
        .filter_map(|(path, bytes)| {
            let sibling = format!("{path}{PRECOMPRESSED_SUFFIX}");
            if existing.contains(sibling.as_str()) {
                return None;
            }
            compress(bytes).map(|out| (sibling, out))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(accept_encoding: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_ENCODING, accept_encoding.parse().unwrap());
        h
    }

    #[test]
    fn html_is_never_precompressed() {
        // The serve path rewrites base paths and injects window.__OXY_APP__
        // into HTML, so a stored compressed copy would never match the bytes
        // actually sent. This is the load-bearing exclusion.
        assert!(!is_precompressible("index.html", 100_000));
        assert!(!is_precompressible("nested/page.html", 100_000));
    }

    #[test]
    fn already_compressed_media_is_skipped() {
        for path in [
            "assets/logo.png",
            "assets/photo.jpg",
            "assets/hero.webp",
            "assets/anim.gif",
            "assets/font.woff2",
            "assets/font.woff",
        ] {
            assert!(!is_precompressible(path, 100_000), "{path} should skip");
        }
    }

    #[test]
    fn text_bundle_output_is_precompressed() {
        for path in [
            "assets/main-a1b2c3.js",
            "assets/style.css",
            "_next/static/chunks/x.mjs",
            "data.json",
            "main.js.map",
            "icon.svg",
            "module.wasm",
        ] {
            assert!(is_precompressible(path, 100_000), "{path} should compress");
        }
    }

    #[test]
    fn tiny_files_are_skipped() {
        // Below one segment the extra object + probe costs more than it saves.
        assert!(!is_precompressible("assets/tiny.js", 10));
        assert!(is_precompressible("assets/big.js", MIN_PRECOMPRESS_BYTES));
    }

    #[test]
    fn a_sibling_the_bundle_already_ships_is_not_regenerated() {
        // `vite-plugin-compression` emits `assets/x.js.br` next to `x.js`.
        // Generating ours too puts two entries under one key, and `put_build`
        // now uploads concurrently — so last-write-wins becomes a race.
        let files = vec![
            ("assets/app.js".to_string(), vec![b'a'; 4096]),
            ("assets/app.js.br".to_string(), vec![b'x'; 128]),
            ("assets/other.js".to_string(), vec![b'a'; 4096]),
        ];
        let variants = precompressed_variants(&files);
        assert_eq!(
            variants.len(),
            1,
            "only the file without a shipped sibling earns one"
        );
        assert_eq!(variants[0].0, "assets/other.js.br");
    }

    #[test]
    fn function_artifacts_get_no_sibling() {
        // functions/<name>.js is read by the runtime at its build-store key,
        // never over the asset route — a .br for it is dead bytes in every
        // retained build.
        assert!(!is_precompressible("functions/post-je.js", 100_000));
        assert!(is_precompressible("assets/post-je.js", 100_000));
        let files = vec![
            ("functions/handler.js".to_string(), vec![b'a'; 4096]),
            ("assets/app.js".to_string(), vec![b'a'; 4096]),
        ];
        let variants = precompressed_variants(&files);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].0, "assets/app.js.br");
    }

    #[test]
    fn probe_filter_rejects_paths_that_can_never_have_a_sibling() {
        // The serve path calls this BEFORE asking the store for `<path>.br`.
        // Without the filter, every SPA navigation and every image would pay
        // a doomed probe on first request.
        assert!(!is_precompressible_extension("orders/42"));
        assert!(!is_precompressible_extension("index.html"));
        assert!(!is_precompressible_extension("assets/logo.png"));
        assert!(is_precompressible_extension("assets/main-a1b2.js"));
        // Size is deliberately NOT part of the probe filter — it can't be
        // known without fetching. A small file simply has no sibling and the
        // absence is cached.
        assert!(is_precompressible_extension("assets/tiny.js"));
    }

    #[test]
    fn extensionless_paths_are_skipped() {
        assert!(!is_precompressible("LICENSE", 100_000));
        // A dot in a directory but not the filename must not be read as an
        // extension — `rsplit('/')` first is what makes this hold.
        assert!(!is_precompressible("some.dir/README", 100_000));
    }

    #[test]
    fn accepts_brotli_reads_the_token_list() {
        assert!(accepts_brotli(&headers_with("gzip, deflate, br")));
        assert!(accepts_brotli(&headers_with("br")));
        assert!(accepts_brotli(&headers_with("gzip, br;q=1.0")));
        assert!(accepts_brotli(&headers_with("br;q=0.1")));
        assert!(!accepts_brotli(&headers_with("gzip, deflate")));
        assert!(!accepts_brotli(&HeaderMap::new()));
        // `brotli` is not `br` — a substring test on the raw header would
        // wrongly match here.
        assert!(!accepts_brotli(&headers_with("gzip, brotli")));
    }

    #[test]
    fn q_zero_is_a_refusal_not_a_weak_preference() {
        // RFC 9110 §12.5.3: q=0 means "do not send me this coding". Stripping
        // the parameter list and keeping the bare token would ship
        // `Content-Encoding: br` to a client that explicitly declined it.
        for header in [
            "br;q=0",
            "br;q=0.0",
            "br;q=0.000",
            "gzip, br;q=0",
            "br; q=0",
            "br;Q=0",
        ] {
            assert!(
                !accepts_brotli(&headers_with(header)),
                "{header:?} refuses brotli"
            );
        }
        // A different coding at q=0 says nothing about brotli.
        assert!(accepts_brotli(&headers_with("gzip;q=0, br")));
    }

    #[test]
    fn variants_round_trip_and_shrink() {
        // Highly compressible input so the size assertion is not flaky.
        let js = "export const x = 1;\n".repeat(500).into_bytes();
        let original_len = js.len();
        let files = vec![
            ("assets/main.js".to_string(), js),
            ("assets/logo.png".to_string(), vec![0u8; 4096]),
            ("index.html".to_string(), vec![b'x'; 4096]),
        ];
        let variants = precompressed_variants(&files);

        assert_eq!(
            variants.len(),
            1,
            "only the .js earns a sibling (png is media, html is rewritten at serve)"
        );
        let (path, bytes) = &variants[0];
        assert_eq!(path, "assets/main.js.br");
        assert!(
            bytes.len() < original_len,
            "brotli output ({}) must be smaller than input ({original_len})",
            bytes.len()
        );

        // And it must actually be valid brotli that decodes back to the input.
        let mut decoded = Vec::new();
        brotli::BrotliDecompress(&mut Cursor::new(bytes), &mut decoded)
            .expect("emitted object must be valid brotli");
        assert_eq!(decoded, files[0].1, "decompressed bytes match the original");
    }

    #[test]
    fn precompressed_siblings_are_not_addressable() {
        // The serve path 404s these rather than handing back raw brotli under
        // `application/octet-stream` with no `Content-Encoding`.
        assert!(is_precompressed_path("assets/main-a1b2.js.br"));
        assert!(is_precompressed_path("index.html.br"));
        assert!(!is_precompressed_path("assets/main-a1b2.js"));
        // Not a fuzzy match — `.brotli` is a different name, and an SPA route
        // must still fall through to the shell.
        assert!(!is_precompressed_path("assets/main.brotli"));
        assert!(!is_precompressed_path("orders/42"));
    }

    #[test]
    fn incompressible_input_emits_no_sibling() {
        // Incompressible bytes with a compressible extension: brotli can't
        // win, so `compress`'s `out.len() < bytes.len()` guard must decline
        // and we must not emit a sibling larger than the original.
        //
        // The fixture has to be genuinely incompressible, which is a higher
        // bar than "looks random". An earlier version of this test used the
        // top byte of a Knuth multiplicative hash over a counter; that is a
        // low-discrepancy sequence, and brotli models it down to 1036 bytes
        // — an 8x win — so a sibling WAS emitted and this assert fired in CI.
        // xorshift32 is not something an LZ+context encoder can invert, so
        // its output lands at input + brotli's stored-block overhead.
        let mut state: u32 = 0x9E37_79B9;
        let mut noise = Vec::with_capacity(8192);
        while noise.len() < 8192 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            noise.extend_from_slice(&state.to_le_bytes());
        }
        let files = vec![("assets/noise.json".to_string(), noise.clone())];
        assert!(
            precompressed_variants(&files).is_empty(),
            "brotli found structure in the fixture, so this no longer tests \
             the size guard — replace the fixture, not the guard"
        );
        // Guard the premise directly: a silent pass because the path stopped
        // being precompressible would test nothing at all.
        assert!(is_precompressible("assets/noise.json", noise.len()));
    }
}
