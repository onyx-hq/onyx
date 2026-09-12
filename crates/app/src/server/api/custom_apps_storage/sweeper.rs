//! The storage sweeper: periodically re-measures each app's silo into
//! `app_storage_usage`, appends a sample, and prunes the sample history.
//!
//! ## Cadence, and why it is tiered
//!
//! Measuring costs one LIST per 1,000 objects, so the cost tracks object count,
//! not value. Apps are picked **oldest-measured-first** and only a bounded batch
//! runs per tick, which gives small apps a fast refresh and large ones a slower
//! one without needing a scheduler that understands sizes.
//!
//! Vercel Blob snapshots every 15 minutes and averages into GB-month. That is the
//! ceiling to aim at, not the starting point: Supabase ships a 1–4 hour lag
//! without complaint, and an hourly sample averages into a defensible GB-month.
//!
//! ## Why not a `TaskSpec`
//!
//! `oxy-task-spec-default` exists so long-running work survives instance death
//! and isn't a spawn in an HTTP handler. This is neither: it is a fixed-cost
//! periodic maintenance loop with no per-request trigger and nothing to resume —
//! a missed tick is simply picked up by the next one, because the work is
//! idempotent by construction (it recomputes from S3). It follows
//! `automation_run::spawn_periodic_sweep`, the established shape for exactly this.
//! If per-app measurement ever needs to fan out across the worker fleet, THAT is
//! the point to make it a queued task.

use std::time::Duration;

use sea_orm::{
    ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect,
};

use entity::prelude::{AppBuilds, AppStorageUsage, AppStorageUsageSamples, Apps};
use entity::{app_storage_usage, app_storage_usage_samples, apps};
use uuid::Uuid;

use super::usage::{self, UsageMeasurement};

/// How often the sweeper wakes. Each tick measures a bounded batch, so this is
/// the *scheduling* granularity, not the per-app refresh interval.
const DEFAULT_INTERVAL_SECS: u64 = 15 * 60;

/// Apps measured per tick. Bounded so the sweeper's cost per tick is predictable
/// no matter how many apps exist — with the default interval this refreshes 96
/// apps/hour, and the oldest-first ordering means nothing starves.
const DEFAULT_BATCH_SIZE: u64 = 24;

/// Sample history kept. 400 days covers a year-over-year read plus slack, and
/// bounds a table that would otherwise grow forever at one row per app per tick.
const SAMPLE_RETENTION_DAYS: i64 = 400;

fn interval() -> Duration {
    let secs = std::env::var("OXY_CUSTOMER_APPS_STORAGE_SWEEP_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Apps one sweep will measure. `pub` because the manual-sweep 409 quotes it:
/// telling an operator to wait without saying the run is bounded is how "a sweep
/// covering your apps is running" becomes untrue for a grant larger than a batch.
pub fn batch_size() -> u64 {
    std::env::var("OXY_CUSTOMER_APPS_STORAGE_SWEEP_BATCH")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_BATCH_SIZE)
}

/// Outcome counts for one sweep. `measured` and `failed` **partition** the batch
/// — every app lands in exactly one — so they can be summed or compared against
/// the batch size without double-counting.
///
/// `incomplete` is orthogonal and overlaps `measured`: an app whose walk was
/// truncated is still persisted (as a floor, with its status attached), so it
/// counts as measured *and* incomplete. Folding it into `failed`, as an earlier
/// version did, let a single app increment two counters and pushed the reported
/// total past the batch size.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Apps whose row was written this tick.
    pub measured: usize,
    /// Apps whose row could NOT be written — the row is now stale.
    pub failed: usize,
    /// Subset of `measured` whose numbers are a floor, not a total.
    pub incomplete: usize,
    pub samples_pruned: u64,
}

/// Apps due for measurement: never-measured first, then oldest-measured.
///
/// One query, with the ordering and the `LIMIT` pushed into the database.
///
/// An earlier version loaded every measured app id into memory and built a
/// `NOT IN (…)` from it, which grows with the fleet on every tick — the exact
/// cost this batching exists to keep flat. `NULLS FIRST` on the joined
/// `measured_at` gives the same "never measured, then stalest" ordering for free,
/// because an unmeasured app has no row to join to.
///
/// Postgres orders NULLs **last** by default, which would put never-measured apps
/// at the back of the queue — the opposite of what we want — so the null ordering
/// is stated explicitly rather than left to the dialect.
///
/// `pub` so the org filter can be tested directly — inline in
/// [`sweep_once`] it is only reachable by running a real S3 walk.
pub async fn apps_due(
    db: &DatabaseConnection,
    limit: u64,
    orgs: Option<&[Uuid]>,
) -> Result<Vec<apps::Model>, sea_orm::DbErr> {
    use sea_orm::sea_query::NullOrdering;
    use sea_orm::{JoinType, Order, RelationTrait};

    let mut q = Apps::find()
        // `Relation::Apps` points app_storage_usage -> apps, so reversing it
        // joins apps -> app_storage_usage and keeps apps with no row.
        .join_rev(JoinType::LeftJoin, app_storage_usage::Relation::Apps.def());
    if let Some(orgs) = orgs {
        q = q.filter(apps::Column::OrgId.is_in(orgs.to_vec()));
    }
    q.order_by_with_nulls(
        app_storage_usage::Column::MeasuredAt,
        Order::Asc,
        NullOrdering::First,
    )
    .limit(limit)
    .all(db)
    .await
}

/// Resolve the retention policy the app's live build declares, so the untagged
/// split reflects the policy actually in force.
async fn policy_for(db: &DatabaseConnection, app: &apps::Model) -> super::RetentionPolicy {
    let Some(build_pk) = app.published_build_id.or(app.draft_build_id) else {
        return super::RetentionPolicy::default();
    };
    let manifest = AppBuilds::find_by_id(build_pk)
        .one(db)
        .await
        .ok()
        .flatten()
        .and_then(|b| b.manifest_json);
    super::super::custom_apps_manifest::retention_policy_from_build_manifest(
        manifest.as_ref(),
        app.id,
    )
}

/// Write one app's measurement to the rollup and the sample series.
async fn persist(
    db: &DatabaseConnection,
    app: &apps::Model,
    m: &UsageMeasurement,
) -> Result<(), sea_orm::DbErr> {
    let now = chrono::Utc::now().into();
    let model = app_storage_usage::ActiveModel {
        app_id: Set(app.id),
        org_id: Set(app.org_id),
        bytes: Set(m.bytes),
        object_count: Set(m.object_count),
        untagged_bytes: Set(m.untagged_bytes),
        untagged_object_count: Set(m.untagged_object_count),
        prefix_breakdown: Set(Some(m.prefix_breakdown_json())),
        measured_at: Set(now),
        measure_status: Set(m.status.to_string()),
        measure_detail: Set(m.detail.clone()),
    };
    AppStorageUsage::insert(model)
        .on_conflict(
            sea_orm::sea_query::OnConflict::column(app_storage_usage::Column::AppId)
                .update_columns([
                    app_storage_usage::Column::OrgId,
                    app_storage_usage::Column::Bytes,
                    app_storage_usage::Column::ObjectCount,
                    app_storage_usage::Column::UntaggedBytes,
                    app_storage_usage::Column::UntaggedObjectCount,
                    app_storage_usage::Column::PrefixBreakdown,
                    app_storage_usage::Column::MeasuredAt,
                    app_storage_usage::Column::MeasureStatus,
                    app_storage_usage::Column::MeasureDetail,
                ])
                .to_owned(),
        )
        .exec(db)
        .await?;

    // Only exact measurements enter the series. A partial walk is a floor, and
    // averaging floors into a GB-month would under-bill silently — the one
    // direction of error nobody notices until an audit.
    if m.is_exact() {
        let sample = app_storage_usage_samples::ActiveModel {
            app_id: Set(app.id),
            measured_at: Set(now),
            bytes: Set(m.bytes),
            object_count: Set(m.object_count),
        };
        AppStorageUsageSamples::insert(sample)
            .on_conflict(
                sea_orm::sea_query::OnConflict::columns([
                    app_storage_usage_samples::Column::AppId,
                    app_storage_usage_samples::Column::MeasuredAt,
                ])
                .do_nothing()
                .to_owned(),
            )
            .do_nothing()
            .exec(db)
            .await?;
    }
    Ok(())
}

async fn prune_samples(db: &DatabaseConnection) -> Result<u64, sea_orm::DbErr> {
    let cutoff: chrono::DateTime<chrono::FixedOffset> =
        (chrono::Utc::now() - chrono::Duration::days(SAMPLE_RETENTION_DAYS)).into();
    let res = AppStorageUsageSamples::delete_many()
        .filter(app_storage_usage_samples::Column::MeasuredAt.lt(cutoff))
        .exec(db)
        .await?;
    Ok(res.rows_affected)
}

/// One sweep pass. Public so the admin console can trigger it on demand — an
/// operator looking at a stale number should be able to refresh it rather than
/// wait out the interval.
///
/// `orgs: Some(..)` bounds the walk to a scoped grant's reach. The periodic sweep
/// passes `None`: it runs as the singleton with no principal, so it is unbounded
/// by construction, the same way `list_apps_scoped` reads `None` as no scope.
///
/// Sample pruning is deliberately NOT scoped — it is retention housekeeping on a
/// cutoff, not a read, and skipping it for a bounded caller would let a fleet
/// whose only sweeps are manual grow its sample table without bound.
pub async fn sweep_once(
    db: &DatabaseConnection,
    orgs: Option<&[Uuid]>,
) -> Result<SweepReport, sea_orm::DbErr> {
    let mut report = SweepReport::default();
    for app in apps_due(db, batch_size(), orgs).await? {
        let policy = policy_for(db, &app).await;
        let measurement = usage::measure_app(app.id, &policy).await;
        if !measurement.is_exact() {
            report.incomplete += 1;
            tracing::warn!(
                app_id = %app.id,
                status = measurement.status,
                detail = measurement.detail.as_deref().unwrap_or(""),
                "storage measurement incomplete; recorded as a floor"
            );
        }
        // Persist regardless: a floor with its status attached beats a stale row
        // that claims to be current.
        if let Err(e) = persist(db, &app, &measurement).await {
            report.failed += 1;
            tracing::warn!(app_id = %app.id, error = %e, "storage usage upsert failed");
            continue;
        }
        report.measured += 1;
    }
    debug_assert!(
        report.measured + report.failed <= batch_size() as usize,
        "a sweep reported more outcomes than the apps in its batch"
    );
    report.samples_pruned = prune_samples(db).await.unwrap_or_default();
    Ok(report)
}

/// Count of apps with no usage row yet — surfaced by the admin console so
/// "0 GB" can be told apart from "never measured".
/// `orgs: Some(..)` narrows both counts to a bounded grant's reach. Narrowing only
/// one of the two would be worse than narrowing neither: subtracting a fleet-wide
/// measured count from a scoped total underflows to 0 and reports "everything is
/// measured" to the one operator who most needs to know it isn't.
pub async fn unmeasured_app_count(
    db: &DatabaseConnection,
    orgs: Option<&[Uuid]>,
) -> Result<u64, sea_orm::DbErr> {
    let (mut total_q, mut measured_q) = (Apps::find(), AppStorageUsage::find());
    if let Some(orgs) = orgs {
        total_q = total_q.filter(apps::Column::OrgId.is_in(orgs.to_vec()));
        measured_q = measured_q.filter(app_storage_usage::Column::OrgId.is_in(orgs.to_vec()));
    }
    let total = total_q.count(db).await?;
    let measured = measured_q.count(db).await?;
    Ok(total.saturating_sub(measured))
}

/// Run [`sweep_once`] on an interval until shutdown.
///
/// `is_singleton_role` is passed in so the role decision stays at the
/// composition root. Every replica sweeping would multiply LIST cost by the
/// replica count for identical results.
pub fn spawn_periodic_sweep(
    db: DatabaseConnection,
    shutdown: tokio_util::sync::CancellationToken,
    is_singleton_role: bool,
) {
    if !is_singleton_role {
        return;
    }
    if super::bucket().is_none() {
        // The filesystem fallback would "work" but measure a dev laptop's temp
        // dir, so the numbers would be meaningless rather than merely absent.
        tracing::debug!(
            "custom-app storage sweeper: no asset bucket configured; not measuring usage"
        );
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval());
        // Skip the immediate first tick — startup is busy enough, and the first
        // real tick is only minutes away.
        ticker.tick().await;
        loop {
            tokio::select! {
                // `None` — the periodic sweep runs as the singleton with no
                // principal, so it is unbounded by construction.
                _ = ticker.tick() => match sweep_once(&db, None).await {
                    Ok(report) if report.measured > 0 || report.failed > 0 => {
                        tracing::info!(
                            measured = report.measured,
                            failed = report.failed,
                            // Without this, "24 measured, 0 failed" reads as a
                            // clean sweep even when every row is a floor.
                            incomplete = report.incomplete,
                            samples_pruned = report.samples_pruned,
                            "custom-app storage usage swept",
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "custom-app storage sweep failed"),
                },
                _ = shutdown.cancelled() => {
                    tracing::debug!("custom-app storage sweeper shutting down");
                    return;
                }
            }
        }
    });
}
