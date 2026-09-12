//! THE "is this workspace a live workspace of this org?" predicate.
//!
//! Three call sites ask exactly this question, and each used to carry its own
//! copy of the `match`:
//!
//! - `custom_apps_publish::validate_project` — the cross-org guard on publish.
//!   Anything but [`WorkspaceOrgMatch::InOrg`] is
//!   `PublishError::UnknownProject`.
//! - `custom_apps_publish::ensure_same_workspace` — decides whether a publish
//!   may re-home an app. Anything but `InOrg` means the app's *current*
//!   workspace is not a live workspace of its org, so the re-home is allowed;
//!   `InOrg` is `PublishError::ProjectMismatch`.
//! - `admin::apps::ops::ensure_workspace_in_org` — the data-integrity rule on
//!   the admin create/move endpoints. All three failures share one **422** body
//!   and differ only in the logged `reason`.
//!
//! **Each caller keeps its own error mapping; only the decision is shared.**
//! The error types genuinely differ — `ApiErr` (a 422 with a JSON body) vs
//! `PublishError::UnknownProject` vs `PublishError::ProjectMismatch` — and two
//! of the three deliberately collapse the failure variants while the admin one
//! tells them apart in its log line. Folding the mapping in here would force
//! one of those shapes onto all three.
//!
//! What this module buys is that the *predicate* cannot drift. If one side
//! later starts accepting `org_id.is_none()`, or starts filtering on
//! `WorkspaceStatus`, it changes here and all three move together. Kept apart,
//! they would diverge silently and publish and the admin API would disagree
//! about the same row.
//!
//! **`WorkspaceStatus` is deliberately NOT consulted today.** A `Cloning`,
//! `Failed` or `NotOxyProject` workspace counts as in-org, exactly as it did
//! before this module existed. That is a preserved behaviour, not an
//! endorsement — when the rule changes, [`classify`] is the one place it lands.
//!
//! Not to be folded in: `custom_apps_publish::org_for_project` answers the
//! opposite question. It *derives* the org from a workspace when the publisher
//! named none, so it has no `org_id` to compare against and no notion of a
//! mismatch — only "unknown" and "orphaned", which it reports as two distinct
//! `UnknownProject` messages.

use entity::workspaces;
use sea_orm::{DatabaseConnection, DbErr, EntityTrait};
use uuid::Uuid;

/// The four ways the question can be answered. Callers that only care
/// yes-or-no match on [`WorkspaceOrgMatch::InOrg`] and lump the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkspaceOrgMatch {
    /// The workspace exists and its `org_id` is this org.
    InOrg,
    /// No `workspaces` row with that id. `apps.project_id` has no foreign key,
    /// so deleting a workspace leaves `apps` rows pointing at nothing.
    Missing,
    /// The row exists but its `org_id` is NULL — a workspace from before
    /// multi-tenancy, belonging to no org.
    Orphaned,
    /// The row exists and belongs to a different org.
    OtherOrg,
}

/// The decision, with no database — `ws` is the already-looked-up row.
///
/// Pure so the rule above is unit-testable; [`workspace_in_org`] is the thin
/// async wrapper that fetches the row for it.
pub(crate) fn classify(ws: Option<&workspaces::Model>, org_id: Uuid) -> WorkspaceOrgMatch {
    match ws {
        Some(w) if w.org_id == Some(org_id) => WorkspaceOrgMatch::InOrg,
        Some(w) if w.org_id.is_none() => WorkspaceOrgMatch::Orphaned,
        Some(_) => WorkspaceOrgMatch::OtherOrg,
        None => WorkspaceOrgMatch::Missing,
    }
}

/// One `find_by_id`, then [`classify`].
///
/// The `DbErr` is returned untouched: each caller maps it its own way, and the
/// admin path logs it before converting, so swallowing it here would lose a log
/// line.
pub(crate) async fn workspace_in_org(
    db: &DatabaseConnection,
    workspace_id: Uuid,
    org_id: Uuid,
) -> Result<WorkspaceOrgMatch, DbErr> {
    let ws = workspaces::Entity::find_by_id(workspace_id).one(db).await?;
    Ok(classify(ws.as_ref(), org_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(org_id: Option<Uuid>) -> workspaces::Model {
        let now = chrono::Utc::now().fixed_offset();
        workspaces::Model {
            id: Uuid::new_v4(),
            name: "test-ws".into(),
            git_namespace_id: None,
            git_remote_url: None,
            created_at: now,
            updated_at: now,
            path: None,
            last_opened_at: None,
            created_by: None,
            org_id,
            status: workspaces::WorkspaceStatus::Ready,
            error: None,
            monthly_vlm_budget_micros: None,
            current_revision_id: None,
        }
    }

    #[test]
    fn workspace_of_this_org_is_in_org() {
        let org = Uuid::new_v4();
        assert_eq!(
            classify(Some(&ws(Some(org))), org),
            WorkspaceOrgMatch::InOrg
        );
    }

    #[test]
    fn absent_row_is_missing() {
        assert_eq!(classify(None, Uuid::new_v4()), WorkspaceOrgMatch::Missing);
    }

    /// The distinction that makes the enum worth having: a NULL `org_id` is
    /// `Orphaned`, not `OtherOrg`. The admin endpoint logs them as different
    /// reasons, and a future rule change may want to treat them differently.
    #[test]
    fn null_org_id_is_orphaned_not_other_org() {
        assert_eq!(
            classify(Some(&ws(None)), Uuid::new_v4()),
            WorkspaceOrgMatch::Orphaned
        );
    }

    #[test]
    fn another_orgs_workspace_is_other_org() {
        assert_eq!(
            classify(Some(&ws(Some(Uuid::new_v4()))), Uuid::new_v4()),
            WorkspaceOrgMatch::OtherOrg
        );
    }

    /// Guards the documented invariant: status is not part of the predicate
    /// today. If someone makes it part, this test should be updated
    /// deliberately rather than discovered by a publish failing in prod.
    #[test]
    fn status_is_not_consulted() {
        let org = Uuid::new_v4();
        for status in [
            workspaces::WorkspaceStatus::Ready,
            workspaces::WorkspaceStatus::Cloning,
            workspaces::WorkspaceStatus::Failed,
            workspaces::WorkspaceStatus::NotOxyProject,
        ] {
            let mut row = ws(Some(org));
            row.status = status;
            assert_eq!(classify(Some(&row), org), WorkspaceOrgMatch::InOrg);
        }
    }
}
