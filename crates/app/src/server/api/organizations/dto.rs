use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct UpdateOrgRequest {
    pub name: Option<String>,
    pub slug: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct OrgResponse {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub role: String,
    pub created_at: String,
    /// Bumped on any org update (incl. logo upload/remove). The frontend
    /// uses it to cache-bust the `/{workspace_id}/logo` <img>.
    pub updated_at: String,
    /// Populated by list endpoints only. Single-org endpoints leave this None
    /// to avoid an extra query on the hot path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_count: Option<i64>,
}

#[derive(Deserialize)]
pub struct UpdateRoleRequest {
    pub role: String,
}

#[derive(Deserialize)]
pub struct InviteRequest {
    pub email: String,
    pub role: String,
}

#[derive(Deserialize)]
pub struct BulkInviteRequest {
    pub invitations: Vec<InviteRequest>,
}

#[derive(Serialize)]
pub struct BulkInviteResponse {
    pub invitations: Vec<InvitationResponse>,
}

#[derive(Serialize)]
pub struct MemberResponse {
    pub id: Uuid,
    pub user_id: Uuid,
    pub email: String,
    pub name: String,
    pub role: String,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct InvitationResponse {
    pub id: Uuid,
    pub email: String,
    pub role: String,
    pub token: String,
    pub status: String,
    pub expires_at: String,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct InvitationSummary {
    pub id: Uuid,
    pub email: String,
    pub role: String,
    pub token: String,
    pub status: String,
    pub expires_at: String,
    pub created_at: String,
    /// Past `expires_at` — the link no longer works and the row is waiting to
    /// be revoked or superseded. Listed rather than filtered out: hiding these
    /// is what left admins with a row they could neither see nor clear.
    pub is_expired: bool,
}

/// Pending invitation addressed to the authenticated user, enriched with the
/// org it's for so the UI can render a meaningful accept screen.
#[derive(Serialize)]
pub struct MyInvitationResponse {
    pub id: Uuid,
    pub token: String,
    pub role: String,
    pub expires_at: String,
    pub created_at: String,
    pub org_id: Uuid,
    pub org_name: String,
    pub org_slug: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invited_by_name: Option<String>,
}
