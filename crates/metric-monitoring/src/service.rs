//! Top-level orchestrator: load monitor config → fetch each series via the
//! injected [`MetricTreeRunner`] → run [`detect`] → return a flat scan
//! result. Persistence lives in the host (the `oxy-app` handler converts
//! [`MonitorOutcome`]s into SeaORM rows); the service itself is database-free
//! so it tests cleanly with a fake runner.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use agentic_analytics::{MetricTreeRunner, MetricTreeRunnerError};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};

#[cfg(test)]
use crate::config::WeekStart;
use crate::config::{
    Direction, Granularity, LoadError, MonitorConfig, MonitorEntry, MonitorFilter, load_from_file,
};
use crate::detect::{
    Continuation, DetectError, DetectInputs, DetectedAnomaly, Observation, detect as run_detect,
};
use crate::gates::min_history_buckets;

/// Result of scanning a workspace.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// One entry per monitor that ran successfully (with or without
    /// flagged anomalies).
    pub outcomes: Vec<MonitorOutcome>,
    /// One entry per monitor that errored out — kept separate so a single
    /// broken monitor doesn't fail the whole scan.
    pub failures: Vec<MonitorFailure>,
    /// The file-level `calendar:`, carried through so persistence can name the
    /// day a cohort landed on.
    ///
    /// On the scan rather than on each [`MonitorEntry`]: a cohort spans
    /// segments, so the label belongs to the scan that observed it, and
    /// `apply_defaults` would otherwise clone the whole map per segment.
    pub calendar: Option<HashMap<NaiveDate, String>>,
}

/// What one monitor produced.
#[derive(Debug)]
pub struct MonitorOutcome {
    pub entry: MonitorEntry,
    pub anomalies: Vec<DetectedAnomaly>,
    /// How much history this segment had. An empty `anomalies` means "nothing
    /// found" only when this is not warming up — otherwise the segment was
    /// never scored, and the two cases must not be conflated.
    pub coverage: Coverage,
}

/// How much history a segment has, against how much it needs to be scored.
///
/// A skipped segment is deliberately not a failure (see [`scan_one`]), which
/// leaves an empty inbox looking identical to a healthy one. This is what lets
/// a caller tell the difference and say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// Buckets the warehouse actually returned; zero-filled gaps excluded.
    pub measured: usize,
    /// [`min_history_buckets`] for this granularity and seasonality.
    pub required: usize,
}

impl Coverage {
    /// The segment is accumulating history rather than being scored.
    pub fn is_warming_up(&self) -> bool {
        self.measured < self.required
    }
}

/// Identifies one monitored segment — the grain at which anomaly events, and
/// therefore continuations, are tracked. Mirrors the `metric_anomalies` key
/// minus `period_start`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SegmentKey {
    pub measure: String,
    pub time_dimension: String,
    pub granularity: String,
    pub dimension_key: String,
}

impl SegmentKey {
    pub fn for_entry(entry: &MonitorEntry) -> Self {
        Self {
            measure: entry.measure.clone(),
            time_dimension: entry.time_dimension.clone(),
            granularity: entry.granularity.airlayer_str().to_string(),
            dimension_key: MonitorFilter::key_for(&entry.filters),
        }
    }
}

/// Anomaly events already on record, keyed by segment.
///
/// Loaded once by the caller and passed in, rather than looked up per segment,
/// so this crate stays database-free and testable against a fake runner.
pub type OpenEvents = std::collections::HashMap<SegmentKey, Continuation>;

/// One segment's scan: what it flagged, and whether it was scored at all.
#[derive(Debug)]
pub struct SegmentScan {
    pub anomalies: Vec<DetectedAnomaly>,
    pub coverage: Coverage,
}

#[derive(Debug)]
pub struct MonitorFailure {
    pub entry: MonitorEntry,
    pub error: ScanError,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("loading monitor config: {0}")]
    LoadConfig(#[from] LoadError),
    #[error("fetching time series: {0}")]
    FetchSeries(#[from] MetricTreeRunnerError),
    #[error("detecting anomalies: {0}")]
    Detect(#[from] DetectError),
    #[error("parsing timestamp '{ts}' from warehouse: {source}")]
    ParseTimestamp {
        ts: String,
        #[source]
        source: chrono::ParseError,
    },
}

/// Scan every monitor in the workspace's `.monitor.yml`.
///
/// `config_path` is usually [`crate::config::default_config_path`] but the
/// admin scan endpoint may accept overrides for one-shot what-if scans.
///
/// `now` is injected so tests are deterministic; production passes
/// `Utc::now()`.
pub async fn scan_workspace(
    runner: Arc<dyn MetricTreeRunner>,
    config_path: &Path,
    now: DateTime<Utc>,
    granularity_filter: Option<Granularity>,
    open_events: &OpenEvents,
) -> Result<ScanResult, ScanError> {
    let cfg: MonitorConfig = load_from_file(config_path)?;
    let mut result = ScanResult {
        calendar: cfg.calendar.clone(),
        ..Default::default()
    };

    // Apply granularity filter before expanding group_by entries.
    let monitors: Vec<MonitorEntry> = if let Some(gran) = granularity_filter {
        cfg.monitors
            .into_iter()
            .filter(|m| m.granularity == gran)
            .collect()
    } else {
        cfg.monitors
    };

    // Expand group_by entries into one entry per discovered dimension value.
    // Entries without group_by pass through unchanged.
    let mut expanded: Vec<MonitorEntry> = Vec::new();
    for entry in monitors {
        if let Some(ref dim) = entry.group_by {
            match runner
                .get_dimension_values(dim.clone(), entry.measure.clone(), entry.lookback_days)
                .await
            {
                Ok(values) => {
                    if values.is_empty() {
                        tracing::warn!(
                            target: "metric_monitoring",
                            measure = %entry.measure,
                            dimension = %dim,
                            "group_by produced no dimension values — skipping"
                        );
                    }
                    for value in values {
                        let mut segment = entry.clone();
                        segment.group_by = None;
                        // AND the group_by filter with any existing filters so
                        // e.g. `filters: [{region: US}]` + `group_by: restaurant_id`
                        // queries each restaurant scoped to US only.
                        segment.filters.push(MonitorFilter {
                            member: dim.clone(),
                            values: vec![value],
                        });
                        expanded.push(segment);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "metric_monitoring",
                        measure = %entry.measure,
                        dimension = %dim,
                        error = %e,
                        "failed to discover group_by dimension values"
                    );
                    result.failures.push(MonitorFailure {
                        entry,
                        error: ScanError::FetchSeries(e),
                    });
                }
            }
        } else {
            expanded.push(entry);
        }
    }

    // Run monitors concurrently so a slow warehouse query on one monitor does
    // not block the others. Results arrive in completion order via JoinSet.
    let mut set = tokio::task::JoinSet::new();
    for entry in expanded {
        let runner = runner.clone();
        let continuation = open_events.get(&SegmentKey::for_entry(&entry)).copied();
        set.spawn(async move {
            let outcome = scan_one(runner, &entry, now, continuation).await;
            (entry, outcome)
        });
    }

    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok((entry, Ok(scan))) => result.outcomes.push(MonitorOutcome {
                entry,
                anomalies: scan.anomalies,
                coverage: scan.coverage,
            }),
            Ok((entry, Err(error))) => {
                tracing::warn!(
                    target: "metric_monitoring",
                    measure = %entry.measure,
                    error = %error,
                    "monitor scan failed"
                );
                result.failures.push(MonitorFailure { entry, error });
            }
            Err(join_err) => {
                tracing::error!(
                    target: "metric_monitoring",
                    error = %join_err,
                    "monitor task panicked"
                );
            }
        }
    }

    Ok(result)
}

/// Run a single monitor end-to-end.
pub async fn scan_one(
    runner: Arc<dyn MetricTreeRunner>,
    entry: &MonitorEntry,
    now: DateTime<Utc>,
    continuation: Option<Continuation>,
) -> Result<SegmentScan, ScanError> {
    let window = lookback_period(now, entry);
    let required = min_history_buckets(entry.granularity, &entry.effective_seasonality());
    let tz = entry.effective_timezone();

    // `MetricTreeRunner::run_time_series` owns the airlayer date_range/bucket
    // timezone-conversion quirk (padding the underlying query and trimming the
    // result back down when the zone is non-UTC) — see its doc comment. So
    // `scan_one` sends its window unpadded; the only trim it still needs is
    // the freshness one below (a bucket whose period has not fully elapsed).
    let raw = runner
        .run_time_series(
            entry.measure.clone(),
            entry.time_dimension.clone(),
            entry.granularity.airlayer_str().to_string(),
            (
                window.start.format("%Y-%m-%d").to_string(),
                window.end.format("%Y-%m-%d").to_string(),
            ),
            monitor_filters_to_query_filters(&entry.filters),
            Some(tz.name().to_string()),
        )
        .await?;

    let mut dated = parse_bucket_dates(&raw)?;
    // Drop any bucket whose period has not fully elapsed.
    dated.retain(|(d, _)| bucket_is_complete(*d, &window, entry.granularity));
    if dated.is_empty() {
        return Ok(SegmentScan {
            anomalies: vec![],
            coverage: Coverage {
                measured: 0,
                required,
            },
        });
    }
    // Zero-fill gaps so MSTL receives a uniformly-spaced array. Sparse series
    // (e.g. no orders on a particular day) would otherwise shift the seasonal
    // alignment and produce false positives or missed anomalies.
    let observations = to_observations(fill_gaps(dated, entry.granularity), tz);

    // Enough history to fit is not the same as enough history to be right —
    // see `gates::min_history_buckets`. This is a skip, not a
    // `DetectError::SeriesTooShort`: a young series is a monitor waiting for
    // data, and routing it through the error path would report weeks of
    // `monitors_failed` for every newly-opened segment.
    let coverage = Coverage {
        measured: observations.iter().filter(|o| !o.imputed).count(),
        required,
    };
    if coverage.is_warming_up() {
        // Debug, not warn: a `group_by` over hundreds of young segments would
        // otherwise warn once per segment per scan. The coverage row is the
        // durable signal — see `persist::upsert_coverage`.
        tracing::debug!(
            target: "metric_monitoring",
            measure = %entry.measure,
            measured = coverage.measured,
            required = coverage.required,
            "skipping monitor: not enough history to score reliably"
        );
        return Ok(SegmentScan {
            anomalies: vec![],
            coverage,
        });
    }

    let test_window = pick_test_window(&observations, entry.granularity);
    let inputs = DetectInputs {
        series: &observations,
        seasonal_periods: entry.effective_seasonality(),
        test_window,
        sensitivity: entry.sensitivity,
        interval_level: 0.95,
        continuation,
    };
    let mut anomalies = run_detect(inputs)?;
    anomalies.retain(|a| match entry.direction {
        Direction::Both => true,
        Direction::Decrease => a.residual < 0.0,
        Direction::Increase => a.residual > 0.0,
    });
    Ok(SegmentScan {
        anomalies,
        coverage,
    })
}

/// Keep a bucket only if it is inside the window AND the period it starts has
/// **fully elapsed** as of the freshness watermark.
///
/// The upper bound deliberately does NOT compare against [`ScanWindow::end`].
/// `week_start` is never sent to airlayer — the warehouse dialect alone labels
/// week buckets (Monday for ClickHouse/Postgres/DuckDB/Snowflake, Sunday for
/// BigQuery/MySQL) — so an `end` derived from the *config's* `week_start` can
/// disagree with the labels that come back and admit the current, still-running
/// week. Testing elapsed-ness instead is dialect-independent: whatever weekday
/// the label lands on, `date + 1 period` is that bucket's exclusive end, and
/// the bucket is trustworthy exactly when that end is at or before the
/// watermark.
///
/// For Day and Month this is *identical* to the old `date < end` test:
/// - Day: `end == watermark`, so `date + 1d <= watermark` ⟺ `date < watermark`.
/// - Month: `end == first-of-watermark's-month`; `first-of-next-month(date)
///   <= watermark` holds exactly when `date`'s month precedes the watermark's,
///   which is exactly `date < end`.
fn bucket_is_complete(date: NaiveDate, window: &ScanWindow, granularity: Granularity) -> bool {
    date >= window.start && advance_period(date, granularity) <= window.watermark
}

/// Convert [`MonitorFilter`]s to the airlayer `QueryFilter` format.
/// Each filter becomes an `equals` clause; multiple filters are ANDed by
/// airlayer when passed as a flat list.
fn monitor_filters_to_query_filters(
    filters: &[MonitorFilter],
) -> Vec<oxy_airlayer_compat::engine::query::QueryFilter> {
    use oxy_airlayer_compat::engine::query::{FilterOperator, QueryFilter};
    filters
        .iter()
        .map(|f| QueryFilter {
            member: Some(f.member.clone()),
            operator: Some(FilterOperator::Equals),
            values: f.values.clone(),
            and: None,
            or: None,
        })
        .collect()
}

/// The resolved scan window, as **local** dates in the monitor's timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScanWindow {
    /// Inclusive first local date of the window.
    start: NaiveDate,
    /// Exclusive end — the start of the current (incomplete) period, aligned
    /// to the *config's* `week_start`. Used only to bound the request and to
    /// derive `start`; it must NOT be used to trim buckets, because the
    /// warehouse dialect (not the config) decides where a week bucket starts.
    end: NaiveDate,
    /// Local date of `now - freshness` — the freshness watermark. A period is
    /// trusted only once it has fully elapsed as of this date.
    watermark: NaiveDate,
}

/// Resolve the query window as **local** dates in the monitor's timezone.
///
/// One rule covers every grain: subtract `freshness` from local
/// `now` (the [`ScanWindow::watermark`]), then snap that watermark to the
/// start of the period containing it. The returned `end` is the first instant
/// of the current (incomplete) period and is therefore **exclusive** — a
/// mid-period scan never sees a partial bucket, and a 3-day freshness resolves to
/// a 4-day, 0-or-1-week, 0-or-1-month shift depending on where the watermark
/// falls.
///
/// Because the watermark is a datetime rather than a date, sub-day freshness values
/// work with no special case: `6h` is a no-op mid-afternoon but withholds
/// yesterday from a 2am scan.
///
/// The watermark is returned rather than recomputed at the trim site: a second
/// copy of `now - freshness` is exactly how the window and the trim would drift
/// apart again.
fn lookback_period(now: DateTime<Utc>, entry: &MonitorEntry) -> ScanWindow {
    use chrono::Datelike;
    let tz = entry.effective_timezone();
    let watermark = now.with_timezone(&tz) - entry.effective_freshness();
    let d = watermark.date_naive();

    let end: NaiveDate = match entry.granularity {
        Granularity::Day => d,
        Granularity::Week => {
            let ws = entry.effective_week_start().num_days_from_monday();
            let delta = (7 + d.weekday().num_days_from_monday() as i64 - ws) % 7;
            d - Duration::days(delta)
        }
        Granularity::Month => NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap_or(d),
    };

    ScanWindow {
        start: end - effective_lookback(entry),
        end,
        watermark: d,
    }
}

/// Bump the user-declared lookback when it would fetch less history than the
/// detector needs.
///
/// Two floors apply and the larger wins. The old one is algebraic — MSTL needs
/// ≥2 cycles per seasonal period, and 3 gives it room. The new one is
/// statistical: [`min_history_buckets`] is the point at which the fit stops
/// being a projection of a ramp. Requesting fewer buckets than that would make
/// the guard in [`scan_one`] unsatisfiable and silence the monitor forever
/// rather than for its first few cycles.
fn effective_lookback(entry: &MonitorEntry) -> Duration {
    let base = Duration::days(entry.lookback_days as i64);
    let seasonality = entry.effective_seasonality();
    let max_period = seasonality.iter().copied().max().unwrap_or(1);
    // The guard counts every measured bucket, training and test alike, so the
    // window has to cover the training depth *plus* the tail being scored —
    // otherwise a window sized to exactly `min_history` fails the guard the
    // first time the warehouse is missing a single day.
    let buckets = (min_history_buckets(entry.granularity, &seasonality)
        + default_test_window(entry.granularity))
    .max(max_period * 3) as i64;
    let floor = match entry.granularity {
        Granularity::Day => Duration::days(buckets),
        Granularity::Week => Duration::days(buckets * 7),
        Granularity::Month => Duration::days(buckets * 31),
    };
    if base > floor { base } else { floor }
}

/// Convert raw `(bucket_label, value)` rows into local calendar dates.
///
/// With a timezone set, airlayer's labels are LOCAL wall-clock, so they are
/// read as naive dates here and only anchored to an instant in
/// [`to_observations`]. Reading them as UTC instants would shift every bucket
/// by the zone's offset.
fn parse_bucket_dates(rows: &[(String, f64)]) -> Result<Vec<(NaiveDate, f64)>, ScanError> {
    let mut out = Vec::with_capacity(rows.len());
    for (ts, value) in rows {
        out.push((parse_bucket_date(ts)?, *value));
    }
    Ok(out)
}

fn parse_bucket_date(ts: &str) -> Result<NaiveDate, ScanError> {
    if let Ok(d) = NaiveDate::parse_from_str(ts, "%Y-%m-%d") {
        return Ok(d);
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S") {
        return Ok(dt.date());
    }
    DateTime::parse_from_rfc3339(ts)
        .map(|d| d.date_naive())
        .map_err(|source| ScanError::ParseTimestamp {
            ts: ts.to_string(),
            source,
        })
}

/// Zero-fill missing periods so the series is uniformly spaced.
///
/// MSTL treats the array as uniformly spaced regardless of the timestamps, so
/// sparse warehouse output would misalign the seasonal index. Walking the
/// LOCAL calendar keeps 23- and 25-hour DST days as exactly one bucket each.
/// Returns `(date, value, imputed)`; `imputed` marks a bucket the warehouse
/// never returned. Downstream that flag is load-bearing, not cosmetic: an
/// invented `0.0` must not count as evidence about what the metric normally
/// does, or one gap drags a seasonal phase's floor to zero and swallows every
/// real drop in that phase.
pub(crate) fn fill_gaps(
    rows: Vec<(NaiveDate, f64)>,
    granularity: Granularity,
) -> Vec<(NaiveDate, f64, bool)> {
    use std::collections::HashMap;
    if rows.len() < 2 {
        return rows.into_iter().map(|(d, v)| (d, v, false)).collect();
    }
    let by_date: HashMap<NaiveDate, f64> = rows.iter().copied().collect();
    let first = rows.first().unwrap().0;
    let last = rows.last().unwrap().0;
    let mut out = Vec::new();
    let mut cur = first;
    while cur <= last {
        match by_date.get(&cur) {
            Some(value) => out.push((cur, *value, false)),
            None => out.push((cur, 0.0, true)),
        }
        cur = advance_period(cur, granularity);
    }
    out
}

/// Anchor each local bucket date at local midnight and store the resulting
/// instant. `.earliest()` covers zones that skip local midnight on a DST
/// spring-forward (e.g. America/Santiago), where `single()` would be `None`
/// and silently drop the bucket.
fn to_observations(rows: Vec<(NaiveDate, f64, bool)>, tz: chrono_tz::Tz) -> Vec<Observation> {
    rows.into_iter()
        .map(|(date, value, imputed)| {
            let timestamp = resolve_local_midnight(date, tz);
            if imputed {
                Observation::filled(timestamp)
            } else {
                Observation::measured(timestamp, value)
            }
        })
        .collect()
}

/// Resolve a local calendar date to the UTC instant of its local midnight.
///
/// Shared by [`to_observations`] (anchors each series bucket) and
/// [`crate::persist::period_end_for`] (computes period boundaries) so both
/// resolve the same class of instant the same way — they used to diverge
/// (one granularity-aware, one a bare `+1 day`), which could make a stored
/// `period_end` disagree with the bucket `to_observations` produced for the
/// same local date whenever granularity != Day.
///
/// `.earliest()` covers the common case; on a DST spring-forward that skips
/// local midnight entirely (e.g. America/Santiago on 2026-09-06), step
/// forward hour by hour until a valid local time is found. The result lands
/// within the gap's width of true midnight — negligible next to a day/week/
/// month bucket — rather than dropping the bucket or (as the granularity-
/// blind `+1 day` fallback did) silently reporting the wrong boundary for
/// Week/Month monitors.
pub(crate) fn resolve_local_midnight(date: NaiveDate, tz: chrono_tz::Tz) -> DateTime<Utc> {
    let naive = date.and_hms_opt(0, 0, 0).expect("midnight is always valid");
    let local = tz
        .from_local_datetime(&naive)
        .earliest()
        .unwrap_or_else(|| {
            (1..=24)
                .find_map(|h| {
                    tz.from_local_datetime(&(naive + Duration::hours(h)))
                        .earliest()
                })
                .unwrap_or_else(|| Utc.from_utc_datetime(&naive).with_timezone(&tz))
        });
    local.with_timezone(&Utc)
}

pub(crate) fn advance_period(date: NaiveDate, granularity: Granularity) -> NaiveDate {
    use chrono::Datelike;
    match granularity {
        Granularity::Day => date + Duration::days(1),
        Granularity::Week => date + Duration::weeks(1),
        Granularity::Month => if date.month() == 12 {
            NaiveDate::from_ymd_opt(date.year() + 1, 1, 1)
        } else {
            NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
        }
        .unwrap_or(date + Duration::days(31)),
    }
}

/// The start of the period immediately before `date` — the inverse of
/// [`advance_period`], in the same calendar-aware terms (a month is not 31
/// days). Used to walk back a bounded number of buckets when deciding whether
/// two flagged buckets belong to the same event.
pub(crate) fn retreat_period(date: NaiveDate, granularity: Granularity) -> NaiveDate {
    use chrono::Datelike;
    match granularity {
        Granularity::Day => date - Duration::days(1),
        Granularity::Week => date - Duration::days(7),
        Granularity::Month => if date.month() == 1 {
            NaiveDate::from_ymd_opt(date.year() - 1, 12, 1)
        } else {
            NaiveDate::from_ymd_opt(date.year(), date.month() - 1, 1)
        }
        .unwrap_or(date - Duration::days(28)),
    }
}

/// Choose how many tail observations to evaluate. For daily series this is
/// the last week; for weekly/monthly it's a single bucket. Keeps the inbox
/// signal-dense without flagging entire weeks at once.
fn pick_test_window(observations: &[Observation], granularity: Granularity) -> usize {
    default_test_window(granularity).min(observations.len().saturating_sub(1).max(1))
}

/// How many tail buckets a scan of this granularity scores, before clamping to
/// the series length. Also sizes the headroom in [`effective_lookback`].
fn default_test_window(granularity: Granularity) -> usize {
    match granularity {
        Granularity::Day => 7,
        Granularity::Week => 1,
        Granularity::Month => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Sensitivity;
    use chrono::Timelike;

    #[test]
    fn granularity_filter_skips_non_matching() {
        let all = vec![
            MonitorEntry {
                measure: "a.b".into(),
                time_dimension: "a.t".into(),
                granularity: Granularity::Day,
                lookback_days: 30,
                seasonality: None,
                sensitivity: Sensitivity::Medium,
                label: None,
                filters: vec![],
                group_by: None,
                direction: Direction::Both,
                timezone: None,
                freshness: None,
                week_start: None,
            },
            MonitorEntry {
                measure: "c.d".into(),
                time_dimension: "c.t".into(),
                granularity: Granularity::Week,
                lookback_days: 90,
                seasonality: None,
                sensitivity: Sensitivity::Medium,
                label: None,
                filters: vec![],
                group_by: None,
                direction: Direction::Both,
                timezone: None,
                freshness: None,
                week_start: None,
            },
        ];
        let filtered: Vec<_> = all
            .into_iter()
            .filter(|m| m.granularity == Granularity::Day)
            .collect();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].measure, "a.b");
    }

    #[test]
    fn lookback_floor_kicks_in_for_short_user_value() {
        let entry = MonitorEntry {
            measure: "x.y".into(),
            time_dimension: "x.t".into(),
            granularity: Granularity::Day,
            lookback_days: 5, // way too short for weekly seasonality
            seasonality: Some(vec![7]),
            sensitivity: Sensitivity::Medium,
            label: None,
            filters: vec![],
            group_by: None,
            direction: Direction::Both,
            timezone: None,
            freshness: None,
            week_start: None,
        };
        let lb = effective_lookback(&entry);
        assert!(
            lb.num_days() >= 7 * 3,
            "expected floor of 21 days, got {lb}"
        );
    }

    #[test]
    fn timestamp_formats() {
        assert!(parse_bucket_date("2024-01-15").is_ok());
        assert!(parse_bucket_date("2024-01-15 12:34:56").is_ok());
        assert!(parse_bucket_date("not a date").is_err());
    }

    #[test]
    fn lookback_period_day_excludes_today() {
        // Mid-day scan: end should be midnight today (= start of today, excludes partial day)
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 14, 30, 0).unwrap();
        let entry = MonitorEntry {
            measure: "x.y".into(),
            time_dimension: "x.t".into(),
            granularity: Granularity::Day,
            lookback_days: 30,
            seasonality: Some(vec![7]),
            sensitivity: Sensitivity::Medium,
            label: None,
            filters: vec![],
            group_by: None,
            direction: Direction::Both,
            timezone: None,
            freshness: None,
            week_start: None,
        };
        let end = lookback_period(now, &entry).end;
        assert_eq!(
            end,
            NaiveDate::from_ymd_opt(2024, 6, 15).unwrap(),
            "end should be today's date, excluding the partial day"
        );
    }

    #[test]
    fn lookback_period_week_excludes_current_week() {
        // Wednesday 2024-06-12: end should be Monday 2024-06-10 (start of the
        // current Monday-start week — week_start defaults to Monday).
        let now = Utc.with_ymd_and_hms(2024, 6, 12, 10, 0, 0).unwrap();
        let entry = MonitorEntry {
            measure: "x.y".into(),
            time_dimension: "x.t".into(),
            granularity: Granularity::Week,
            lookback_days: 90,
            seasonality: Some(vec![4]),
            sensitivity: Sensitivity::Medium,
            label: None,
            filters: vec![],
            group_by: None,
            direction: Direction::Both,
            timezone: None,
            freshness: None,
            week_start: None,
        };
        let end = lookback_period(now, &entry).end;
        assert_eq!(
            end,
            NaiveDate::from_ymd_opt(2024, 6, 10).unwrap(),
            "end should be Monday of the current week, excluding the partial week"
        );
    }

    #[test]
    fn lookback_period_month_excludes_current_month() {
        // June 15: end should be June 1 (start of current month)
        let now = Utc.with_ymd_and_hms(2024, 6, 15, 8, 0, 0).unwrap();
        let entry = MonitorEntry {
            measure: "x.y".into(),
            time_dimension: "x.t".into(),
            granularity: Granularity::Month,
            lookback_days: 365,
            seasonality: Some(vec![12]),
            sensitivity: Sensitivity::Medium,
            label: None,
            filters: vec![],
            group_by: None,
            direction: Direction::Both,
            timezone: None,
            freshness: None,
            week_start: None,
        };
        let end = lookback_period(now, &entry).end;
        assert_eq!(
            end,
            NaiveDate::from_ymd_opt(2024, 6, 1).unwrap(),
            "end should be the 1st of the current month, excluding the partial month"
        );
    }

    /// A daily entry with everything defaulted; tests override what they need.
    fn entry_at(granularity: Granularity) -> MonitorEntry {
        MonitorEntry {
            measure: "x.y".into(),
            time_dimension: "x.t".into(),
            granularity,
            lookback_days: 90,
            seasonality: Some(vec![7]),
            sensitivity: Sensitivity::Medium,
            label: None,
            filters: vec![],
            group_by: None,
            direction: Direction::Both,
            timezone: None,
            freshness: None,
            week_start: None,
        }
    }

    #[test]
    fn snaps_to_the_local_day_not_the_utc_day() {
        // 2026-07-27 03:00 UTC is still 2026-07-26 20:00 in Los Angeles.
        // A UTC snap would end at Jul 27; the local snap must end at Jul 26.
        let now = Utc.with_ymd_and_hms(2026, 7, 27, 3, 0, 0).unwrap();
        let mut entry = entry_at(Granularity::Day);
        entry.timezone = Some("America/Los_Angeles".into());

        let end = lookback_period(now, &entry).end;
        assert_eq!(end, NaiveDate::from_ymd_opt(2026, 7, 26).unwrap());
    }

    #[test]
    fn freshness_snaps_per_grain_from_one_duration() {
        // now = Mon 2026-07-27 12:00 LA, freshness 3d -> watermark Fri 2026-07-24.
        let now = Utc.with_ymd_and_hms(2026, 7, 27, 19, 0, 0).unwrap(); // 12:00 PDT
        let cases = [
            (
                Granularity::Day,
                NaiveDate::from_ymd_opt(2026, 7, 24).unwrap(),
            ),
            // Monday-start week containing Jul 24 (a Friday) begins Mon Jul 20.
            (
                Granularity::Week,
                NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(),
            ),
            (
                Granularity::Month,
                NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(),
            ),
        ];
        for (granularity, expected_end) in cases {
            let mut entry = entry_at(granularity);
            entry.timezone = Some("America/Los_Angeles".into());
            entry.freshness = Some(std::time::Duration::from_secs(3 * 86_400));
            let end = lookback_period(now, &entry).end;
            assert_eq!(end, expected_end, "grain {granularity:?}");
        }
    }

    #[test]
    fn week_freshness_is_zero_when_the_watermark_stays_in_this_week() {
        // now = Fri 2026-07-31 12:00 LA, freshness 3d -> watermark Tue Jul 28,
        // still inside the Monday-start week beginning Jul 27.
        let now = Utc.with_ymd_and_hms(2026, 7, 31, 19, 0, 0).unwrap();
        let mut entry = entry_at(Granularity::Week);
        entry.timezone = Some("America/Los_Angeles".into());
        entry.freshness = Some(std::time::Duration::from_secs(3 * 86_400));
        let end = lookback_period(now, &entry).end;
        assert_eq!(end, NaiveDate::from_ymd_opt(2026, 7, 27).unwrap());
    }

    #[test]
    fn month_offset_becomes_one_just_after_a_month_boundary() {
        // now = Sun 2026-08-02 12:00 LA, freshness 3d -> watermark Thu Jul 30,
        // so the last complete month is June, not July.
        let now = Utc.with_ymd_and_hms(2026, 8, 2, 19, 0, 0).unwrap();
        let mut entry = entry_at(Granularity::Month);
        entry.timezone = Some("America/Los_Angeles".into());
        entry.freshness = Some(std::time::Duration::from_secs(3 * 86_400));
        let end = lookback_period(now, &entry).end;
        assert_eq!(end, NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
    }

    #[test]
    fn sub_grain_offset_only_bites_near_the_boundary() {
        let mut entry = entry_at(Granularity::Day);
        entry.timezone = Some("America/Los_Angeles".into());
        entry.freshness = Some(std::time::Duration::from_secs(6 * 3_600));

        // 12:00 local minus 6h is still the same local day -> no-op.
        let midday = Utc.with_ymd_and_hms(2026, 7, 27, 19, 0, 0).unwrap();
        assert_eq!(
            lookback_period(midday, &entry).end,
            NaiveDate::from_ymd_opt(2026, 7, 27).unwrap()
        );

        // 02:00 local minus 6h lands on the previous local day -> withholds it.
        let early = Utc.with_ymd_and_hms(2026, 7, 27, 9, 0, 0).unwrap();
        assert_eq!(
            lookback_period(early, &entry).end,
            NaiveDate::from_ymd_opt(2026, 7, 26).unwrap()
        );
    }

    #[test]
    fn week_start_switches_the_boundary() {
        // Wed 2026-07-29 UTC. Sunday-start week began Jul 26; Monday-start Jul 27.
        let now = Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap();

        let mut sunday = entry_at(Granularity::Week);
        sunday.week_start = Some(WeekStart::Sunday);
        assert_eq!(
            lookback_period(now, &sunday).end,
            NaiveDate::from_ymd_opt(2026, 7, 26).unwrap()
        );

        let mut monday = entry_at(Granularity::Week);
        monday.week_start = Some(WeekStart::Monday);
        assert_eq!(
            lookback_period(now, &monday).end,
            NaiveDate::from_ymd_opt(2026, 7, 27).unwrap()
        );
    }

    #[test]
    fn dst_transition_days_snap_cleanly() {
        // This test asserts that computing the watermark as *instant*
        // arithmetic (convert `now` to local, subtract a sub-day freshness,
        // then read the local date) resolves correctly on both sides of a
        // DST transition — not merely that a normal date on a transition day
        // round-trips. `now`'s UTC calendar date and its LA calendar date
        // are deliberately made to differ from the watermark's LA calendar
        // date, so a pure-UTC implementation (ignoring `effective_timezone`
        // entirely) or a naive local-wall-clock subtraction (ignoring the
        // gap/fold) would both produce the wrong day here.

        // Spring-forward: 2026-03-08 02:00 PST jumps straight to 03:00 PDT
        // (02:00-03:00 never exists locally). That instant is 10:00 UTC
        // (02:00 PST = UTC-8 => 10:00 UTC); LA is PST before 10:00 UTC and
        // PDT at/after it.
        //
        // now = 2026-03-08 12:00 UTC = 2026-03-08 05:00 PDT (after the gap).
        // freshness = 5h -> watermark instant = 2026-03-08 07:00 UTC, which is
        // *before* the 10:00 UTC transition, so LA reads it as PST:
        // 07:00 - 8h = -1:00 => 2026-03-07 23:00 PST. The window between the
        // watermark and `now` (07:00-12:00 UTC) straddles the 10:00 UTC
        // transition, so 5 real hours land the watermark a full calendar day
        // back — confirmed by hand and cross-checked with a standalone
        // chrono/chrono-tz computation (`local_now - Duration::hours(5)` on
        // this instant prints `2026-03-07 23:00:00 PST`).
        let mut spring_entry = entry_at(Granularity::Day);
        spring_entry.timezone = Some("America/Los_Angeles".into());
        spring_entry.freshness = Some(std::time::Duration::from_secs(5 * 3_600));
        let spring = Utc.with_ymd_and_hms(2026, 3, 8, 12, 0, 0).unwrap();
        assert_eq!(
            lookback_period(spring, &spring_entry).end,
            NaiveDate::from_ymd_opt(2026, 3, 7).unwrap(),
            "watermark must cross the spring-forward gap without panicking or off-by-one"
        );

        // Fall-back: 2026-11-01 02:00 PDT repeats as 01:00 PST (01:00-02:00
        // happens twice). That instant is 09:00 UTC (02:00 PDT = UTC-7 =>
        // 09:00 UTC); LA is PDT before 09:00 UTC and PST at/after it.
        //
        // now = 2026-11-01 12:00 UTC = 2026-11-01 04:00 PST (after the fold).
        // freshness = 6h -> watermark instant = 2026-11-01 06:00 UTC, which is
        // *before* the 09:00 UTC transition, so LA reads it as PDT:
        // 06:00 - 7h = -1:00 => 2026-10-31 23:00 PDT. Cross-checked the same
        // way (`local_now - Duration::hours(6)` prints `2026-10-31 23:00:00
        // PDT`).
        let mut fall_entry = entry_at(Granularity::Day);
        fall_entry.timezone = Some("America/Los_Angeles".into());
        fall_entry.freshness = Some(std::time::Duration::from_secs(6 * 3_600));
        let fall = Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap();
        assert_eq!(
            lookback_period(fall, &fall_entry).end,
            NaiveDate::from_ymd_opt(2026, 10, 31).unwrap(),
            "watermark must cross the fall-back fold without panicking or off-by-one"
        );
    }

    #[test]
    fn defaults_reproduce_the_pre_change_utc_window() {
        // THE BACKWARD-COMPATIBILITY GUARANTEE. An entry with no timezone and
        // no freshness must produce exactly what the UTC-only implementation did:
        // day -> today, week -> Monday of this week, month -> the 1st.
        let now = Utc.with_ymd_and_hms(2024, 6, 12, 10, 0, 0).unwrap(); // a Wednesday

        let day = entry_at(Granularity::Day);
        assert_eq!(
            lookback_period(now, &day).end,
            NaiveDate::from_ymd_opt(2024, 6, 12).unwrap()
        );

        // The old code hardcoded Monday and `week_start` defaults to Monday,
        // so the weekly boundary is unchanged too — there is no grain for
        // which an existing `.monitor.yml` moves.
        let mut week = entry_at(Granularity::Week);
        week.seasonality = Some(vec![4]);
        assert_eq!(
            lookback_period(now, &week).end,
            NaiveDate::from_ymd_opt(2024, 6, 10).unwrap(),
            "Monday-start week containing Wed Jun 12 begins Mon Jun 10"
        );

        let mut month = entry_at(Granularity::Month);
        month.seasonality = Some(vec![12]);
        assert_eq!(
            lookback_period(now, &month).end,
            NaiveDate::from_ymd_opt(2024, 6, 1).unwrap()
        );
    }

    #[test]
    fn direction_decrease_keeps_drops_removes_spikes() {
        use crate::detect::{DetectedAnomaly, Severity};
        let t = Utc.with_ymd_and_hms(2024, 6, 14, 0, 0, 0).unwrap();
        let drop = DetectedAnomaly {
            timestamp: t,
            observed: 400.0,
            expected: 1000.0,
            lower: 800.0,
            upper: 1200.0,
            residual: -600.0,
            z_score: -3.5,
            severity: Severity::High,
        };
        let spike = DetectedAnomaly {
            timestamp: t,
            observed: 1600.0,
            expected: 1000.0,
            lower: 800.0,
            upper: 1200.0,
            residual: 600.0,
            z_score: 3.5,
            severity: Severity::High,
        };
        let mut anomalies = vec![drop, spike];
        anomalies.retain(|a| match Direction::Decrease {
            Direction::Both => true,
            Direction::Decrease => a.residual < 0.0,
            Direction::Increase => a.residual > 0.0,
        });
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].residual, -600.0);
    }

    #[test]
    fn direction_increase_keeps_spikes_removes_drops() {
        use crate::detect::{DetectedAnomaly, Severity};
        let t = Utc.with_ymd_and_hms(2024, 6, 14, 0, 0, 0).unwrap();
        let drop = DetectedAnomaly {
            timestamp: t,
            observed: 400.0,
            expected: 1000.0,
            lower: 800.0,
            upper: 1200.0,
            residual: -600.0,
            z_score: -3.5,
            severity: Severity::High,
        };
        let spike = DetectedAnomaly {
            timestamp: t,
            observed: 1600.0,
            expected: 1000.0,
            lower: 800.0,
            upper: 1200.0,
            residual: 600.0,
            z_score: 3.5,
            severity: Severity::High,
        };
        let mut anomalies = vec![drop, spike];
        anomalies.retain(|a| match Direction::Increase {
            Direction::Both => true,
            Direction::Decrease => a.residual < 0.0,
            Direction::Increase => a.residual > 0.0,
        });
        assert_eq!(anomalies.len(), 1);
        assert_eq!(anomalies[0].residual, 600.0);
    }

    #[test]
    fn trims_out_of_window_and_incomplete_buckets() {
        // A bucket outside the window, or the watermark-incomplete current
        // period, must not reach the detector — regardless of what produced
        // it (the runner's own timezone pad/trim, or just the still-running
        // current period). Calls the same `bucket_is_complete` predicate
        // `scan_one` uses (not a hand-copied re-implementation), so a future
        // off-by-one regression in `scan_one`'s trim is caught here too.
        let window = ScanWindow {
            start: NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(),
            end: NaiveDate::from_ymd_opt(2026, 7, 24).unwrap(), // exclusive
            watermark: NaiveDate::from_ymd_opt(2026, 7, 24).unwrap(),
        };
        let rows = vec![
            (NaiveDate::from_ymd_opt(2026, 7, 19).unwrap(), 1.0), // before the window
            (NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(), 2.0),
            (NaiveDate::from_ymd_opt(2026, 7, 23).unwrap(), 3.0),
            (NaiveDate::from_ymd_opt(2026, 7, 24).unwrap(), 4.0), // the partial day
            (NaiveDate::from_ymd_opt(2026, 7, 25).unwrap(), 5.0), // after the window
        ];
        let kept: Vec<f64> = rows
            .into_iter()
            .filter(|(d, _)| bucket_is_complete(*d, &window, Granularity::Day))
            .map(|(_, v)| v)
            .collect();
        assert_eq!(kept, vec![2.0, 3.0]);
    }

    #[test]
    fn an_unfinished_week_bucket_is_trimmed_when_the_dialect_disagrees() {
        // `week_start` aligns Oxy's OWN boundary; it is never sent to the
        // warehouse, so the dialect alone decides where a week bucket starts.
        // Here the config says Sunday (BigQuery/MySQL-shaped) but the
        // warehouse is a Monday dialect (ClickHouse/Postgres/...).
        //
        // now = Sun 2026-08-02 12:00 UTC, freshness 0 -> watermark Sun Aug 2.
        // The config's Sunday-start snap makes `end` = Aug 2, so a
        // `date < end` trim would ADMIT the Monday-labelled bucket Jul 27 —
        // which covers Jul 27..Aug 3 and is still running. Since
        // `pick_test_window(Week)` is 1, that partial bucket would be the ONLY
        // bucket evaluated: a systematic weekly false decrease.
        let now = Utc.with_ymd_and_hms(2026, 8, 2, 12, 0, 0).unwrap();
        let mut entry = entry_at(Granularity::Week);
        entry.seasonality = Some(vec![4]);
        entry.week_start = Some(WeekStart::Sunday);
        let window = lookback_period(now, &entry);
        assert_eq!(
            window.end,
            NaiveDate::from_ymd_opt(2026, 8, 2).unwrap(),
            "precondition: the Sunday-start snap lands on Aug 2"
        );

        let unfinished = NaiveDate::from_ymd_opt(2026, 7, 27).unwrap(); // a Monday
        let complete = NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(); // a Monday
        assert!(
            !bucket_is_complete(unfinished, &window, Granularity::Week),
            "the week starting Mon Jul 27 ends Aug 3, after the Aug 2 watermark"
        );
        assert!(
            bucket_is_complete(complete, &window, Granularity::Week),
            "the week starting Mon Jul 20 ended Jul 27, well before the watermark"
        );
    }

    /// What `scan_one` asked the warehouse for.
    #[derive(Debug, Clone)]
    struct TimeSeriesCall {
        granularity: String,
        period: (String, String),
        timezone: Option<String>,
    }

    /// A [`MetricTreeRunner`] that records the time-series call and replays
    /// canned rows. Only `run_time_series` is exercised by [`scan_one`]; the
    /// rest of the trait errors out so an accidental new call site is loud.
    struct RecordingRunner {
        rows: Vec<(String, f64)>,
        seen: std::sync::Mutex<Option<TimeSeriesCall>>,
    }

    #[async_trait::async_trait]
    impl MetricTreeRunner for RecordingRunner {
        async fn load_layer(
            &self,
        ) -> Result<oxy_airlayer_compat::SemanticLayer, MetricTreeRunnerError> {
            Err(MetricTreeRunnerError::LayerLoad(
                "unused by scan_one".into(),
            ))
        }
        async fn list_databases(&self) -> Vec<oxy_airlayer_compat::DatabaseConfig> {
            vec![]
        }
        async fn run_explain(
            &self,
            _: String,
            _: String,
            _: (String, String),
            _: (String, String),
            _: Vec<oxy_airlayer_compat::engine::query::QueryFilter>,
            _: oxy_airlayer_compat::engine::metric_tree_ops::ExplainConfig,
        ) -> Result<
            oxy_airlayer_compat::engine::metric_tree_ops::ExplainResult,
            MetricTreeRunnerError,
        > {
            Err(MetricTreeRunnerError::Op("unused by scan_one".into()))
        }
        async fn run_opportunity(
            &self,
            _: String,
            _: String,
            _: (String, String),
            _: oxy_airlayer_compat::engine::metric_tree_ops::BenchmarkStatistic,
        ) -> Result<
            oxy_airlayer_compat::engine::metric_tree_ops::OpportunityResult,
            MetricTreeRunnerError,
        > {
            Err(MetricTreeRunnerError::Op("unused by scan_one".into()))
        }
        async fn get_dimension_values(
            &self,
            _: String,
            _: String,
            _: u32,
        ) -> Result<Vec<String>, MetricTreeRunnerError> {
            Ok(vec![])
        }
        async fn run_time_series(
            &self,
            _measure: String,
            _time_dimension: String,
            granularity: String,
            period: (String, String),
            _filters: Vec<oxy_airlayer_compat::engine::query::QueryFilter>,
            timezone: Option<String>,
        ) -> Result<Vec<(String, f64)>, MetricTreeRunnerError> {
            *self.seen.lock().unwrap() = Some(TimeSeriesCall {
                granularity,
                period,
                timezone,
            });
            Ok(self.rows.clone())
        }
    }

    #[tokio::test]
    async fn scan_one_sends_the_unpadded_window_passes_the_zone_and_drops_out_of_window_rows() {
        // The feature's central seam, end to end against a fake runner:
        // (a) `scan_one` requests its own window unpadded — padding for the
        //     airlayer date_range/bucket timezone quirk is now
        //     `MetricTreeRunner::run_time_series`'s job, not the monitor
        //     scanner's (see its doc comment) —, (b) the monitor's resolved
        //     zone is handed to the runner, and (c) a runner that (whether by
        //     the timezone pad or otherwise) hands back a bucket outside the
        //     window, or the current still-running partial day, must not
        //     reach the detector — each of those three out-of-window buckets
        //     carries a colossal spike, so if any of them survived the trim
        //     the daily test window (7 buckets) would flag it.
        //
        // now = 2026-07-27 19:00Z = 12:00 PDT, so the local day is Jul 27 and
        // (Day grain, 90-day lookback) the window is [2026-04-28, 2026-07-27).
        let now = Utc.with_ymd_and_hms(2026, 7, 27, 19, 0, 0).unwrap();
        let mut entry = entry_at(Granularity::Day);
        entry.timezone = Some("America/Los_Angeles".into());

        // The fake runner ignores the requested period and just replays
        // whatever rows the test hands it — including some outside the
        // window scan_one actually asked for, to prove scan_one's own
        // freshness trim (`bucket_is_complete`) still catches them even
        // though scan_one no longer pads its own request. These bounds are
        // the fake's *reply* range, not what scan_one requests (that's
        // asserted separately below via `call.period`).
        let first_returned = NaiveDate::from_ymd_opt(2026, 4, 27).unwrap();
        let last_returned = NaiveDate::from_ymd_opt(2026, 7, 28).unwrap();
        let out_of_window = [
            NaiveDate::from_ymd_opt(2026, 4, 27).unwrap(), // before the window
            NaiveDate::from_ymd_opt(2026, 7, 27).unwrap(), // the partial day
            NaiveDate::from_ymd_opt(2026, 7, 28).unwrap(), // after the window
        ];

        // A calm weekly-seasonal series, with a 1000x spike on every bucket
        // that must be trimmed.
        let mut rows = Vec::new();
        let mut d = first_returned;
        let mut i = 0usize;
        while d <= last_returned {
            let base = 100.0 + 5.0 * (2.0 * std::f64::consts::PI * (i % 7) as f64 / 7.0).sin();
            let value = if out_of_window.contains(&d) {
                100_000.0
            } else {
                base
            };
            rows.push((d.format("%Y-%m-%d").to_string(), value));
            d += Duration::days(1);
            i += 1;
        }

        let runner = Arc::new(RecordingRunner {
            rows,
            seen: std::sync::Mutex::new(None),
        });
        let scan = scan_one(runner.clone(), &entry, now, None).await.unwrap();
        let anomalies = scan.anomalies;

        let call = runner.seen.lock().unwrap().clone().expect("runner called");
        assert_eq!(call.granularity, "day");
        assert_eq!(
            call.period,
            ("2026-04-28".to_string(), "2026-07-27".to_string()),
            "scan_one must send its own window unpadded — padding is the runner's job now"
        );
        assert_eq!(
            call.timezone.as_deref(),
            Some("America/Los_Angeles"),
            "the monitor's resolved zone must reach airlayer"
        );
        assert!(
            anomalies.is_empty(),
            "out-of-window buckets and the partial day must be trimmed before \
             detection, but got {anomalies:?}"
        );
        // The other side of `a_young_series_is_skipped_rather_than_failed`:
        // this series IS being scored and simply found nothing. Both cases
        // return zero anomalies, so coverage is the only thing that tells them
        // apart — and an operator reading an empty inbox needs that.
        assert!(
            !scan.coverage.is_warming_up(),
            "a dense 90-day series must be scored, not skipped: {:?}",
            scan.coverage
        );
    }

    #[test]
    fn bucket_labels_are_read_as_local_dates_not_utc_instants() {
        // airlayer returns wall-clock labels once a timezone is set. Reading
        // "2026-07-20" as UTC midnight and then rendering it in LA would show
        // Jul 19 — the bucket must anchor at LOCAL midnight.
        let tz: chrono_tz::Tz = "America/Los_Angeles".parse().unwrap();
        let rows = vec![(NaiveDate::from_ymd_opt(2026, 7, 20).unwrap(), 10.0, false)];
        let obs = to_observations(rows, tz);
        assert_eq!(obs.len(), 1);
        assert_eq!(
            obs[0].timestamp.with_timezone(&tz).date_naive(),
            NaiveDate::from_ymd_opt(2026, 7, 20).unwrap()
        );
        // 2026-07-20 00:00 PDT is 07:00 UTC.
        assert_eq!(obs[0].timestamp.hour(), 7);
    }

    #[test]
    fn to_observations_survives_a_spring_forward_midnight() {
        // Some zones (e.g. America/Santiago) skip local midnight on the DST
        // transition, so `from_local_datetime` returns None for it. The
        // conversion must fall back rather than drop the bucket or panic.
        let tz: chrono_tz::Tz = "America/Santiago".parse().unwrap();
        let rows = vec![(NaiveDate::from_ymd_opt(2026, 9, 6).unwrap(), 42.0, false)];
        let obs = to_observations(rows, tz);
        assert_eq!(obs.len(), 1, "the bucket must not be dropped");
        assert_eq!(obs[0].value, 42.0);
    }

    #[tokio::test]
    async fn a_young_series_is_skipped_rather_than_failed() {
        // The new-store case: 24 daily buckets clears detect()'s algebraic
        // floor of 21, so before the history guard this series was scored —
        // and forecast off its own opening ramp, which pinned the expectation
        // at its first week's level and flagged every subsequent day at up to
        // 22σ. It must now be skipped, and skipped *quietly*: routing it
        // through ScanError would report weeks of `monitors_failed` for every
        // newly-opened segment.
        let now = Utc.with_ymd_and_hms(2026, 7, 27, 19, 0, 0).unwrap();
        let entry = entry_at(Granularity::Day);

        // A store's opening ramp: a slow first two and a half weeks, then the
        // business finding its feet. Training ends inside the slow stretch, so
        // the forecast is pinned near 500 while the store is actually running
        // 1,000-2,000/day — every day of the test window reads as a huge,
        // high-severity spike. This is the ae79d063 shape, and the reason a
        // linear ramp would not do: AutoETS fits a straight line perfectly well
        // and flags nothing.
        let ramp = [
            460.0, 480.0, 455.0, 500.0, 520.0, 610.0, 590.0, // week 1
            505.0, 495.0, 530.0, 545.0, 560.0, 700.0, 660.0, // week 2
            580.0, 600.0, 620.0, // ...and training ends here
            1000.0, 1250.0, 1500.0, 1700.0, 1900.0, 2000.0, 1950.0,
        ];
        let mut rows = Vec::new();
        let mut d = NaiveDate::from_ymd_opt(2026, 7, 3).unwrap();
        for value in ramp {
            rows.push((d.format("%Y-%m-%d").to_string(), value));
            d += Duration::days(1);
        }
        // detect()'s algebraic floor for seasonality [7] with a 7-bucket test
        // window is `(7*2).max(10) + 7` == 21. The fixture must sit above that
        // and below the statistical floor, or it proves nothing: the point is
        // that these two thresholds disagree.
        assert!(rows.len() >= 21, "fixture must clear detect()'s own floor");
        assert!(rows.len() < min_history_buckets(Granularity::Day, &[7]));

        let runner = Arc::new(RecordingRunner {
            rows,
            seen: std::sync::Mutex::new(None),
        });
        let scan = scan_one(runner, &entry, now, None)
            .await
            .expect("a young series is a skip, never an error");
        assert!(
            scan.anomalies.is_empty(),
            "a series below the history floor must not be scored, got {:?}",
            scan.anomalies
        );
        // The durable half of the fix: the skip must be *reported*, not just
        // performed, or the Monitors tab cannot tell this from a healthy
        // monitor that simply found nothing.
        assert!(scan.coverage.is_warming_up());
        assert_eq!(scan.coverage.measured, 24);
        assert_eq!(
            scan.coverage.required,
            min_history_buckets(Granularity::Day, &[7])
        );
    }

    #[test]
    fn the_lookback_floor_can_always_satisfy_the_history_guard() {
        // These two numbers are set in different functions and drifted apart
        // once already. If the window is ever sized below what the guard
        // demands, every monitor at that granularity goes permanently silent
        // — a failure with no error message anywhere.
        for granularity in [Granularity::Day, Granularity::Week, Granularity::Month] {
            let mut entry = entry_at(granularity);
            entry.lookback_days = 1; // force the floor to be what applies
            entry.seasonality = Some(granularity.default_seasonality());
            let days = effective_lookback(&entry).num_days();
            let buckets = match granularity {
                Granularity::Day => days,
                Granularity::Week => days / 7,
                Granularity::Month => days / 31,
            };
            let needed = min_history_buckets(granularity, &entry.effective_seasonality()) as i64;
            assert!(
                buckets > needed,
                "{granularity:?}: window holds {buckets} buckets but the guard needs {needed}"
            );
        }
    }

    #[test]
    fn fill_gaps_zero_fills_a_missing_bucket() {
        // `fill_gaps` walks bare `NaiveDate`s, so it is calendar-agnostic —
        // this pins the zero-fill itself, not any timezone behavior.
        let rows = vec![
            (NaiveDate::from_ymd_opt(2026, 3, 7).unwrap(), 1.0),
            // Mar 8 missing.
            (NaiveDate::from_ymd_opt(2026, 3, 9).unwrap(), 3.0),
        ];
        let filled = fill_gaps(rows, Granularity::Day);
        assert_eq!(filled.len(), 3, "the missing day must be filled in");
        assert_eq!(filled[1].0, NaiveDate::from_ymd_opt(2026, 3, 8).unwrap());
        assert_eq!(filled[1].1, 0.0);
        assert!(
            filled[1].2,
            "an invented bucket must be marked imputed — downstream gates rely \
             on telling 'no data' apart from a measured zero"
        );
        assert!(!filled[0].2 && !filled[2].2, "real rows are not imputed");
    }
}
