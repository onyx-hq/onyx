//! The user row a dev sign-in lands on — found, minted, or refused.

use axum::http::StatusCode;
use entity::{org_members, prelude::OrgMembers, prelude::Users, users, users::UserStatus};
use sea_orm::{ActiveValue, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set};
use uuid::Uuid;

use oxy::database::filters::UserQueryFilterExt;

use super::super::ops::insert_user_or_fetch_existing;
use super::personas::{self, DevLoginRefusal, Persona, Provisioning};

/// Find the user for `email`, or mint one when the request may.
///
/// Refusals, in order: a deleted row is `401` (never resurrected, exactly as the
/// OAuth handlers refuse); a persona naming a seeded identity whose rows are
/// missing is `409`, pointing at the seed rather than minting a stranger.
pub(super) async fn find_or_provision(
    email: &str,
    persona: Option<&Persona>,
    connection: &DatabaseConnection,
) -> Result<users::Model, DevLoginRefusal> {
    let existing = Users::find()
        .filter_by_email(email)
        .one(connection)
        .await
        .map_err(|e| {
            tracing::error!("dev-login: failed to query user: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    match existing {
        Some(user) if user.status == UserStatus::Deleted => {
            tracing::warn!("dev-login: refused {email} — user is deleted");
            Err(StatusCode::UNAUTHORIZED.into())
        }
        Some(user) => {
            require_membership_if_seeded(&user, email, persona, connection).await?;
            Ok(user)
        }
        None if personas::may_mint(persona) => mint(email, connection).await,
        None => Err(refuse_unseeded(email, persona)),
    }
}

/// A persona that names an org member is only "seeded" once the membership
/// exists — a bare user row (say, from an earlier email sign-in) would land on
/// onboarding, which is the symptom this refusal exists to replace.
async fn require_membership_if_seeded(
    user: &users::Model,
    email: &str,
    persona: Option<&Persona>,
    connection: &DatabaseConnection,
) -> Result<(), DevLoginRefusal> {
    if persona.is_none_or(|p| p.provisioning != Provisioning::OrgMember) {
        return Ok(());
    }
    let membership = OrgMembers::find()
        .filter(org_members::Column::UserId.eq(user.id))
        .one(connection)
        .await
        .map_err(|e| {
            tracing::error!("dev-login: failed to query org membership: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    match membership {
        Some(_) => Ok(()),
        None => Err(refuse_unseeded(email, persona)),
    }
}

fn refuse_unseeded(email: &str, persona: Option<&Persona>) -> DevLoginRefusal {
    tracing::warn!("dev-login: refused {email} — persona identity is not seeded");
    match persona {
        Some(persona) => persona.not_seeded(email),
        // Unreachable by construction (`may_mint(None)` is true), but a bare 409
        // is the honest answer if that ever changes.
        None => StatusCode::CONFLICT.into(),
    }
}

/// An ordinary user row, created on first use — so a fresh database lands on
/// onboarding exactly like a real new user.
async fn mint(
    email: &str,
    connection: &DatabaseConnection,
) -> Result<users::Model, DevLoginRefusal> {
    let name = email.split('@').next().unwrap_or(email).to_string();
    let new_user = users::ActiveModel {
        id: Set(Uuid::new_v4()),
        email: Set(Some(email.to_string())),
        name: Set(name),
        picture: Set(None),
        email_verified: Set(true),
        magic_link_token: ActiveValue::NotSet,
        magic_link_token_expires_at: ActiveValue::NotSet,
        status: Set(UserStatus::Active),
        created_at: ActiveValue::NotSet,
        last_login_at: ActiveValue::NotSet,
    };
    Ok(insert_user_or_fetch_existing(new_user, email, connection).await?)
}
