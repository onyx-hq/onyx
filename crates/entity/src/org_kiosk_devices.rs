//! An enrolled kiosk — the device a frontline PIN may be entered on.
//!
//! Two lifetimes in one row. Before binding, `enrol_token_hash` and
//! `enrol_expires_at` hold a one-time link an org admin hands to the tablet;
//! after, `secret_hash` holds the long-lived credential the browser carries in
//! the `oxy_kiosk` cookie and the token columns are NULL. `revoked_at` is how a
//! lost tablet is switched off; the row stays, because which kiosk a shift was
//! signed in on is part of the audit trail. See the migration and
//! `internal-docs/frontline-identity.md`.

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};

#[sea_orm::model]
#[derive(Clone, Debug, PartialEq, DeriveEntityModel, Eq, Serialize, Deserialize)]
#[sea_orm(table_name = "org_kiosk_devices")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub org_id: Uuid,
    /// The admin's label — "Front counter", "Drive-thru iPad".
    pub name: String,
    /// The app this kiosk was enrolled for, if the admin named one. The login
    /// page sends a signed-in worker here when no `return_to` was given.
    pub return_to: Option<String>,
    /// SHA-256 of the one-time enrol token; NULL once bound or never issued.
    pub enrol_token_hash: Option<String>,
    pub enrol_expires_at: Option<DateTimeWithTimeZone>,
    /// SHA-256 of the device secret the cookie carries; NULL until bound.
    pub secret_hash: Option<String>,
    pub created_by: Option<Uuid>,
    pub created_at: DateTimeWithTimeZone,
    pub bound_at: Option<DateTimeWithTimeZone>,
    pub last_seen_at: Option<DateTimeWithTimeZone>,
    pub revoked_at: Option<DateTimeWithTimeZone>,
    /// Where the tablet sits. A physical object has a place; the login page
    /// names it, and the crew roster is narrowed to it.
    pub location_id: Option<Uuid>,
    /// Seconds of inactivity after which the app signs the shift session out.
    /// NULL is "the default" (`frontline_devices::DEFAULT_IDLE_TIMEOUT_SECONDS`)
    /// rather than "no timeout", so existing kiosks acquire the behaviour with
    /// no backfill. The platform only carries the number — the countdown runs
    /// in the custom app.
    pub idle_timeout_seconds: Option<i32>,
}

impl ActiveModelBehavior for ActiveModel {}
