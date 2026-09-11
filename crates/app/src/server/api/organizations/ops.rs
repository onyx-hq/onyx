use std::collections::HashMap;

use axum::http::StatusCode;
use chrono::Utc;
use email_address::EmailAddress;
use entity::org_invitations;
use entity::org_members;
use entity::org_members::OrgRole;
use entity::organizations;
use entity::prelude::*;
use entity::workspaces;
use handlebars::Handlebars;
use once_cell::sync::Lazy;
use sea_orm::prelude::DateTimeWithTimeZone;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, FromQueryResult, QueryFilter, QuerySelect,
};
use uuid::Uuid;

use oxy_shared::errors::OxyError;

use super::dto::OrgResponse;
use sea_orm::ExprTrait;

pub(super) fn org_response(org: &organizations::Model, role: &OrgRole) -> OrgResponse {
    OrgResponse {
        id: org.id,
        name: org.name.clone(),
        slug: org.slug.clone(),
        role: role.as_str().to_string(),
        created_at: org.created_at.to_rfc3339(),
        updated_at: org.updated_at.to_rfc3339(),
        workspace_count: None,
        member_count: None,
    }
}

/// Canonical slug generation. The frontend has a preview slugify for UX, but
/// this function is the source of truth — the stored slug always comes from here.
pub fn slugify_name(name: &str) -> String {
    slugify::slugify(name, "", "-", None)
}

/// Slugs that collide with top-level frontend routes. An org with one of these
/// slugs would be unreachable at `/{slug}` because React Router resolves the
/// static path first. Keep in sync with the routes declared in
/// `web-app/src/App.tsx` and any future top-level additions.
const RESERVED_ORG_SLUGS: &[&str] = &[
    "admin",
    "api",
    "app",
    "apps",
    "auth",
    "github",
    "invite",
    "invitations",
    "login",
    "logout",
    "onboarding",
    "orgs",
    "settings",
    "signin",
    "signup",
    "static",
    "workspace",
    "workspaces",
];

pub fn is_reserved_slug(slug: &str) -> bool {
    RESERVED_ORG_SLUGS.contains(&slug)
}

/// Trims, lowercases, and validates an invitee email. Returns the normalized
/// form on success or `BAD_REQUEST` for empty / malformed input. Centralizing
/// here keeps the single-invite and bulk-invite paths in sync.
pub fn normalize_invite_email(raw: &str) -> Result<String, StatusCode> {
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() || !EmailAddress::is_valid(&normalized) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(normalized)
}

/// The invitation for `(org, email)` that is still usable, if any.
///
/// Only a *live* invite blocks a new one: re-inviting someone who already
/// holds a working link should be a no-op rather than mint a second token.
/// An expired one must not block — [`supersede_expired_invitations`] clears
/// it instead. Shared by the single, bulk, and partner-console invite paths
/// so they can't drift apart.
pub async fn find_live_invitation<C: ConnectionTrait>(
    conn: &C,
    org_id: Uuid,
    email: &str,
    now: DateTimeWithTimeZone,
) -> Result<Option<org_invitations::Model>, StatusCode> {
    OrgInvitations::find()
        .filter(org_invitations::Column::OrgId.eq(org_id))
        .filter(org_invitations::Column::Email.eq(email))
        .filter(org_invitations::live_pending(now))
        .one(conn)
        .await
        .map_err(|e| {
            tracing::error!("Failed to check existing invitation: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// Delete the lapsed `pending` rows for `(org, email)`, returning how many
/// went. Call immediately before inserting the replacement, in the same
/// transaction: the new invite supersedes them, and leaving them behind is
/// what used to make the email permanently un-invitable.
///
/// Deleting rather than marking them expired also retires the old token, so a
/// stale link 404s instead of resolving to a row that then refuses it.
pub async fn supersede_expired_invitations<C: ConnectionTrait>(
    conn: &C,
    org_id: Uuid,
    email: &str,
    now: DateTimeWithTimeZone,
) -> Result<u64, StatusCode> {
    let result = OrgInvitations::delete_many()
        .filter(org_invitations::Column::OrgId.eq(org_id))
        .filter(org_invitations::Column::Email.eq(email))
        .filter(org_invitations::expired_pending(now))
        .exec(conn)
        .await
        .map_err(|e| {
            tracing::error!("Failed to supersede expired invitations: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(result.rows_affected)
}

#[derive(FromQueryResult)]
struct OrgCountRow {
    org_id: Uuid,
    count: i64,
}

pub(super) async fn count_members_per_org(
    db: &sea_orm::DatabaseConnection,
    org_ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, StatusCode> {
    let rows: Vec<OrgCountRow> = OrgMembers::find()
        .filter(org_members::Column::OrgId.is_in(org_ids.to_vec()))
        .select_only()
        .column(org_members::Column::OrgId)
        .column_as(Expr::col(org_members::Column::Id).count(), "count")
        .group_by(org_members::Column::OrgId)
        .into_model::<OrgCountRow>()
        .all(db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to count members per org: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(rows.into_iter().map(|r| (r.org_id, r.count)).collect())
}

pub(super) async fn count_workspaces_per_org(
    db: &sea_orm::DatabaseConnection,
    org_ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, StatusCode> {
    let rows: Vec<OrgCountRow> = Workspaces::find()
        .filter(workspaces::Column::OrgId.is_in(org_ids.to_vec()))
        // Same exclusion as `list_workspaces`: the legacy `--local` nil-UUID
        // workspace can't be opened in cloud, so counting it would disagree with
        // the list (a count of 1 over an empty list).
        .filter(workspaces::Column::Id.ne(Uuid::nil()))
        .select_only()
        .column(workspaces::Column::OrgId)
        .column_as(Expr::col(workspaces::Column::Id).count(), "count")
        .group_by(workspaces::Column::OrgId)
        .into_model::<OrgCountRow>()
        .all(db)
        .await
        .map_err(|e| {
            tracing::error!("Failed to count workspaces per org: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(rows.into_iter().map(|r| (r.org_id, r.count)).collect())
}

static INVITATION_TEMPLATE: Lazy<Handlebars<'static>> = Lazy::new(|| {
    let mut hbs = Handlebars::new();
    hbs.register_template_string("invitation", include_str!("../../../emails/invitation.hbs"))
        .expect("invitation.hbs is valid");
    hbs
});

/// Sends an invitation email. Piggybacks on the magic-link SES config for the
/// sender identity; if magic-link auth is not configured, this is a no-op so
/// the admin can still share the copy-able invite token manually.
pub async fn send_invitation_email(
    to_email: &str,
    token: &str,
    base_url: &str,
    inviter_name: &str,
    inviter_email: &str,
    org_name: &str,
) -> Result<(), OxyError> {
    use crate::emails::{
        EmailMessage, EmailProvider, local_test::LocalTestEmailProvider, ses::SesEmailProvider,
    };

    let magic_link_config = oxy::config::oxy::get_oxy_config()
        .ok()
        .and_then(|c| c.authentication)
        .and_then(|a| a.magic_link);

    let Some(config) = magic_link_config else {
        tracing::warn!(
            "Invitation email not sent — magic-link email config missing. Token can still be shared via the Pending Invitations UI."
        );
        return Ok(());
    };

    let invite_url = format!("{base_url}/invite/{token}");
    let subject = format!("You've been invited to {org_name} on Oxygen");
    // An inviter with no email address renders as `Alice ()` — both invite
    // paths can pass `""` now that `users.email` is nullable, and a parenthetical
    // around nothing reads as a broken template rather than as a missing detail.
    let inviter = if inviter_email.is_empty() {
        inviter_name.to_string()
    } else {
        format!("{inviter_name} ({inviter_email})")
    };
    let text_body = format!(
        "{inviter} has invited you to join {org_name} on Oxygen.\n\nAccept the invitation:\n{invite_url}\n\nThis invitation expires in 7 days. If you weren't expecting this, you can safely ignore this email."
    );
    let message = EmailMessage {
        subject,
        html_body: build_invitation_email_html(&invite_url, to_email, &inviter, org_name)?,
        text_body,
    };

    if std::env::var("MAGIC_LINK_LOCAL_TEST").is_ok() {
        LocalTestEmailProvider
            .send(&config.from_email, to_email, message)
            .await
    } else {
        SesEmailProvider::new(config.aws_region.as_deref())
            .await
            .send(&config.from_email, to_email, message)
            .await
    }
}

/// `inviter` is the ALREADY-COMPOSED display string, not a name and an address.
///
/// Composing it once and handing the same string to both bodies is what stops
/// them drifting: the first version of this fix dropped the empty parenthetical
/// from the text body only, and the HTML — the part most clients actually
/// render — went on saying `Alice () has invited you`.
fn build_invitation_email_html(
    invite_url: &str,
    to_email: &str,
    inviter: &str,
    org_name: &str,
) -> Result<String, OxyError> {
    let data = serde_json::json!({
        "invite_url": invite_url,
        "to_email": to_email,
        "invited_by": inviter,
        "org_name": org_name,
        "year": Utc::now().format("%Y").to_string(),
    });

    INVITATION_TEMPLATE
        .render("invitation", &data)
        .map_err(|e| OxyError::RuntimeError(format!("Failed to render invitation template: {e}")))
}

#[cfg(test)]
mod invitation_email_tests {
    use super::*;

    /// Both bodies are rendered from one composed string, so a mailbox-less
    /// inviter cannot produce `Alice ()` in either.
    ///
    /// The HTML half is the one that matters: it is what most clients render,
    /// and it is the half the first version of this fix missed while the text
    /// body looked correct.
    #[test]
    fn a_mailbox_less_inviter_leaves_no_empty_parenthetical() {
        let html =
            build_invitation_email_html("https://x/invite/t", "b@example.com", "Alice", "Acme")
                .expect("render");
        assert!(html.contains("Alice"), "the inviter's name is missing");
        assert!(
            !html.contains("()") && !html.contains("( )"),
            "an empty parenthetical reached the HTML body"
        );
    }

    /// And the ordinary case still shows the address.
    #[test]
    fn an_inviter_with_an_address_still_shows_it() {
        let html = build_invitation_email_html(
            "https://x/invite/t",
            "b@example.com",
            "Alice (alice@acme.com)",
            "Acme",
        )
        .expect("render");
        assert!(html.contains("alice@acme.com"));
    }
}
