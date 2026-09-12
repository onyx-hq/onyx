use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// One row per successful publish of a custom app. The bundle's files
/// live in S3 under `s3_prefix`; `apps.draft_build_id` /
/// `apps.published_build_id` point at the build currently serving each
/// channel. Keeping every build (bounded by a keep-last-N GC) is what
/// makes one-click rollback cheap.
#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "app_builds")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub app_id: Uuid,
    /// Engineer-facing version string (git sha or CI run id). Unique per
    /// app via `(app_id, build_id)`.
    pub build_id: String,
    /// S3 prefix holding this build's files:
    /// `customer-apps/<app_id>/builds/<build_id>/`.
    pub s3_prefix: String,
    /// Optional build/runtime manifest captured at publish time
    /// (`oxy-app.json`). Drives the future `artifact_type` serve branch.
    pub manifest_json: Option<Json>,
    pub created_at: DateTimeWithTimeZone,
    /// User (app-admin) who ran the publish. NULL for builds created before
    /// this column existed. Powers the "who deployed" audit in the admin UI.
    pub published_by: Option<Uuid>,
    /// Git remote URL of the app's source at publish time (raw, e.g.
    /// `git@github.com:org/repo.git` or `https://github.com/org/repo`).
    /// Captured best-effort by `oxyc publish`; NULL for non-git / legacy builds.
    pub source_repo: Option<String>,
    /// Commit sha the build was published from.
    pub commit_sha: Option<String>,
    /// Branch the build was published from.
    pub source_branch: Option<String>,
    /// Recorded bundle-validation outcome: `passed` | `pending` | `failed`.
    /// Promotion to live is gated on `passed` (the validator-can't-be-bypassed
    /// invariant). Gate 1 (fast byte-level checks at publish) stamps `passed`;
    /// a deeper deploy-time render probe (gate 2 — tracked follow-up) may
    /// downgrade to `failed`. Defaults to `passed` for builds predating the
    /// column (they are already serving).
    pub validation_status: String,
    /// Human-readable reason when `validation_status = failed`.
    pub validation_detail: Option<String>,
    #[sea_orm(
        belongs_to,
        from = "app_id",
        to = "id",
        on_update = "NoAction",
        on_delete = "Cascade"
    )]
    #[serde(skip)]
    pub apps: BelongsTo<super::apps::Entity>,
}

impl ActiveModelBehavior for ActiveModel {}
