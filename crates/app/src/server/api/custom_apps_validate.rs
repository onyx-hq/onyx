//! Custom-app bundle validation — fast, synchronous, byte-level checks run at
//! publish time. This is **gate 1** of the validator (see the partner-platform
//! design doc's "validator can't be bypassed" invariant).
//!
//! These catch the common "deployed wrong / won't render" failures BEFORE the
//! build is stored, turning them into an actionable `422` (which check, why, how
//! to fix) instead of a bundle that serves a blank "Loading…" forever. A build
//! that passes is recorded `app_builds.validation_status = passed`, and promotion
//! to the live channel is gated on that status
//! (`custom_apps_publish::gate_promotion` + `admin::apps::handlers::publish_one`)
//! — so a build that hasn't passed can never go live.
//!
//! The heavier **gate 2** — a deploy-time render probe that would re-validate the
//! STORED bundle and downgrade `validation_status` to `failed` — is a tracked
//! follow-up, not yet built (its natural home is a worker-fleet TaskSpec, which
//! can't reach this crate's validators without a layering change).
//!
//! Uses "custom app" naming per the rename decision; it validates the same
//! bundles the (frozen-wire) `/customer-apps/` serve path serves.

use serde::Serialize;

/// A structured, actionable validation failure. Serializable so a UI can render
/// it; its `Display` (used for the publish `422` body) reads check + message +
/// remediation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BundleValidation {
    /// Machine-readable check id, e.g. `base_path_mismatch`.
    pub check: String,
    /// What's wrong, in plain language.
    pub message: String,
    /// One concrete fix.
    pub remediation: String,
}

impl BundleValidation {
    pub(crate) fn new(
        check: &str,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            check: check.to_string(),
            message: message.into(),
            remediation: remediation.into(),
        }
    }
}

impl std::fmt::Display for BundleValidation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "bundle failed validation [{}]: {} (fix: {})",
            self.check, self.message, self.remediation
        )
    }
}

/// Validate a freshly-unpacked bundle (the `(relative_path, bytes)` pairs from
/// `unpack_tar_gz`). Returns the FIRST failure so the message is unambiguous.
/// `Ok(())` means the fast checks passed — not a guarantee it renders (that's
/// the probe's job), but that the known blank-screen causes are ruled out.
pub fn validate_bundle(
    files: &[(String, Vec<u8>)],
    org_slug: &str,
    app_slug: &str,
) -> Result<(), BundleValidation> {
    // 1. index.html present at the bundle root.
    let index = files
        .iter()
        .find(|(p, _)| p == "index.html")
        .ok_or_else(|| {
            BundleValidation::new(
                "index_html_missing",
                "The bundle has no index.html at its root.",
                "Publish the build OUTPUT directory (e.g. Vite's dist/), which contains index.html at the top level.",
            )
        })?;

    let html = std::str::from_utf8(&index.1).map_err(|_| {
        BundleValidation::new(
            "index_html_not_utf8",
            "index.html is not valid UTF-8.",
            "The index.html appears corrupted — rebuild the app and republish.",
        )
    })?;

    // 2. </head> present — the serve path splices window.__OXY_APP__ before it;
    //    without a head the app boots with no runtime config and can't reach the
    //    API.
    if !html.contains("</head>") {
        return Err(BundleValidation::new(
            "head_missing",
            "index.html has no </head> tag, so the Oxy runtime config can't be injected and the app can't reach the API.",
            "Ensure index.html has a <head>…</head> section (standard for Vite/CRA output).",
        ));
    }

    // 3. Baked base path matches the registered slug. THE #1 "stuck on Loading…"
    //    cause: serve-time rewriting patches index.html but can't reach into the
    //    JS chunks, so a mismatch 404s every data fetch. Note the expected prefix
    //    uses the frozen wire path `/customer-apps/`.
    if let Some(baked) = crate::server::api::custom_apps_serve::first_custom_apps_prefix(html) {
        let expected = format!("/customer-apps/{org_slug}/{app_slug}/");
        if baked != expected {
            return Err(BundleValidation::new(
                "base_path_mismatch",
                format!(
                    "Bundle was built with base path {baked:?}, but this app is registered as \
                     {expected:?}. The JS chunks fetch from the baked path and will 404 every data \
                     product — the dashboard will sit at 'Loading…' forever."
                ),
                format!(
                    "Rebuild with OXY_APP_BASE_PATH={expected}, or register the app under a slug \
                     that matches the baked path."
                ),
            ));
        }
    }

    Ok(())
}

/// Largest launcher-card image (`art`) that publishes without a warning.
///
/// The HQ home downloads every card's image on every visit, and a card is at
/// most ~600 CSS px wide. A 1280×640 WebP or JPEG is 30–110 KB; the PNG
/// screenshots that prompted this were 120–944 KB, up to 3588 px wide, and took
/// ~1.8 s each to arrive.
const ART_WARN_BYTES: usize = 200 * 1024;

/// A publish warning for an oversized `art` image, or `None`.
///
/// **Advisory, not gate 1.** An oversized image still renders — it only makes
/// the home page slow — so it must never fail a publish the way
/// [`validate_bundle`] does. It rides `PublishResult::warnings`, which
/// `oxyc publish` prints.
///
/// Silent when there is no manifest, no `art`, or `art` names a file the bundle
/// does not carry: the launcher already falls back to a letter tile there, and
/// this check is about bytes, not about whether the image exists.
pub fn oversized_art_warning(
    files: &[(String, Vec<u8>)],
    manifest: Option<&serde_json::Value>,
) -> Option<String> {
    let art = manifest?.get("art")?.as_str()?;
    let (_, bytes) = files.iter().find(|(p, _)| p == art)?;
    if bytes.len() <= ART_WARN_BYTES {
        return None;
    }
    Some(format!(
        "launcher card image `{art}` is {} KB. The HQ home downloads it for every card on every \
         visit, so keep it under {} KB: export 1280×640 as WebP or JPEG, e.g. \
         `cwebp -q 80 -resize 1280 0 {art} -o card.webp`, then set \"art\": \"card.webp\".",
        bytes.len() / 1024,
        ART_WARN_BYTES / 1024,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(entries: &[(&str, &[u8])]) -> Vec<(String, Vec<u8>)> {
        entries
            .iter()
            .map(|(p, b)| (p.to_string(), b.to_vec()))
            .collect()
    }

    #[test]
    fn passes_a_well_formed_bundle() {
        let html = b"<html><head></head><body></body></html>";
        let f = files(&[("index.html", html), ("assets/app.js", b"//js")]);
        assert!(validate_bundle(&f, "acme", "sales").is_ok());
    }

    #[test]
    fn rejects_missing_index() {
        let f = files(&[("assets/app.js", b"//js")]);
        let e = validate_bundle(&f, "acme", "sales").unwrap_err();
        assert_eq!(e.check, "index_html_missing");
    }

    #[test]
    fn rejects_missing_head() {
        let f = files(&[("index.html", b"<html><body></body></html>")]);
        let e = validate_bundle(&f, "acme", "sales").unwrap_err();
        assert_eq!(e.check, "head_missing");
    }

    #[test]
    fn rejects_base_path_mismatch() {
        // Baked for a different slug than registered.
        let html = br#"<html><head><script src="/customer-apps/acme/OLD/app.js"></script></head><body></body></html>"#;
        let f = files(&[("index.html", html)]);
        let e = validate_bundle(&f, "acme", "sales").unwrap_err();
        assert_eq!(e.check, "base_path_mismatch");
        assert!(
            e.remediation
                .contains("OXY_APP_BASE_PATH=/customer-apps/acme/sales/")
        );
    }

    #[test]
    fn accepts_matching_base_path() {
        let html = br#"<html><head><script src="/customer-apps/acme/sales/app.js"></script></head><body></body></html>"#;
        let f = files(&[("index.html", html)]);
        assert!(validate_bundle(&f, "acme", "sales").is_ok());
    }

    #[test]
    fn skips_base_path_check_when_no_prefix_baked() {
        // Built without OXY_APP_BASE_PATH — serve-time injection handles it.
        let html = br#"<html><head><script src="/app.js"></script></head><body></body></html>"#;
        let f = files(&[("index.html", html)]);
        assert!(validate_bundle(&f, "acme", "sales").is_ok());
    }

    fn art_manifest(art: &str) -> serde_json::Value {
        serde_json::json!({ "slug": "sales", "art": art })
    }

    #[test]
    fn warns_about_an_art_image_over_the_limit() {
        let big = vec![0u8; ART_WARN_BYTES + 1];
        let f = vec![("card.png".to_string(), big)];
        let w = oversized_art_warning(&f, Some(&art_manifest("card.png")))
            .expect("an oversized card image must warn");
        assert!(
            w.contains("card.png"),
            "the warning must name the file: {w}"
        );
    }

    #[test]
    fn an_art_image_at_the_limit_is_silent() {
        let f = vec![("shots/card.webp".to_string(), vec![0u8; ART_WARN_BYTES])];
        assert_eq!(
            oversized_art_warning(&f, Some(&art_manifest("shots/card.webp"))),
            None
        );
    }

    /// Only the file `art` names counts: a large image elsewhere in the bundle
    /// is not the launcher's problem, and a missing one is not this check's.
    #[test]
    fn is_silent_without_an_art_file_to_measure() {
        let big = vec![0u8; ART_WARN_BYTES + 1];
        let f = vec![("assets/hero.png".to_string(), big)];
        assert_eq!(oversized_art_warning(&f, None), None);
        assert_eq!(
            oversized_art_warning(&f, Some(&serde_json::json!({ "slug": "sales" }))),
            None
        );
        assert_eq!(
            oversized_art_warning(&f, Some(&art_manifest("card.png"))),
            None
        );
    }
}
