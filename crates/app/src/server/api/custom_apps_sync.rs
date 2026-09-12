//! Customer-app bundle channels.
//!
//! The legacy S3 → state-dir sync pipeline (CI `POST /sync`, draft/published
//! prefix copy, `oxy apps sync`) was retired when the publish pipeline landed
//! — bundles now arrive via `POST /customer-apps/publish`, are stored per
//! build under `customer-apps/<app_id>/builds/<build_id>/` in S3, and are
//! served directly from S3 through an in-memory cache (see
//! `custom_apps_build_store`, `custom_apps_bundle_cache`, and
//! `custom_apps_serve::serve_from_s3_build`).
//!
//! Only the [`Channel`] enum survives here: the serve path and the manifest
//! resolver use it to pick which channel pointer (`apps.draft_build_id` vs
//! `apps.published_build_id`) to resolve per request.

/// One of the two channels a custom-app bundle is served from. `Draft`
/// is what `oxyc publish` writes by default (admin-preview only);
/// `Published` is what viewers see after a promote.
#[derive(Debug, Clone, Copy)]
pub enum Channel {
    Draft,
    Published,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Draft => "draft",
            Channel::Published => "published",
        }
    }
}
