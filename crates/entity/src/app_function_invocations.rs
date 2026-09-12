use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

/// One row per Oxy Functions invocation (route, schedule, or airway).
/// See `internal-docs/customer-apps-functions.md` §11.12.
#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "app_function_invocations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub app_id: Uuid,
    pub build_id: Uuid,
    pub function_name: String,
    /// `"route"` | `"schedule"` | `"airway"`.
    pub mode: String,
    /// `None` for system (schedule/airway) invocations.
    pub user_id: Option<Uuid>,
    /// `"running"` | `"success"` | `"error"` | `"cancelled"` | `"timeout"`.
    pub status: String,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
    pub cancel_requested_at: Option<DateTimeWithTimeZone>,
    pub created_at: DateTimeWithTimeZone,
    /// Caller-supplied idempotency key (route mode); unique per
    /// (app, function, user). `None` when the caller sent none.
    pub idempotency_key: Option<String>,
    /// Stored response body of a successful invocation, kept only when an
    /// `idempotency_key` is present so a retry can replay it.
    pub result_body: Option<String>,
    /// Hash of the request body for a keyed invocation, so a key reused with a
    /// different body is rejected instead of silently replaying the first result.
    pub request_hash: Option<i64>,
    /// The HTTP status the function returned, stored beside the body it belongs
    /// to so an idempotent replay reports the same status as the first call.
    /// `None` on rows written before the column existed — read as 200, which is
    /// what they were already being reported as.
    pub result_status: Option<i16>,
    /// A digest of how the invocation failed, with the message's data taken
    /// out — `None` when it did not fail, and on rows from before the column.
    /// Written with `error`; see `custom_apps_functions::failure_signal`.
    pub failure_fingerprint: Option<String>,
}

impl ActiveModelBehavior for ActiveModel {}
