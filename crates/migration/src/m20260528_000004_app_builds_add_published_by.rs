//! Adds `app_builds.published_by` — the user (app-admin) who ran the
//! `oxyc publish` that created the build. Powers the "who deployed" audit in
//! the customer-apps admin console, recovering the trail CI logs used to give
//! us now that engineers publish directly.
//!
//! Nullable + `ON DELETE SET NULL`: builds created before this column stay
//! NULL, and hard-deleting a user leaves a clean NULL rather than a dangling
//! UUID. Guarded with `has_column` so dev databases that already have the
//! column pick up cleanly.
//!
//! Note: this records the *original publish* author only. Who later
//! *promoted* a build live (promote draft or Make Live/rollback) is stamped
//! separately on `apps.last_promoted_by` (migration 000005).

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AppBuilds {
    Table,
    PublishedBy,
}

#[derive(DeriveIden)]
enum Users {
    Table,
    Id,
}

const FK_NAME: &str = "fk_app_builds_published_by";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if !manager.has_column("app_builds", "published_by").await? {
            manager
                .alter_table(
                    Table::alter()
                        .table(AppBuilds::Table)
                        .add_column(ColumnDef::new(AppBuilds::PublishedBy).uuid())
                        .to_owned(),
                )
                .await?;
            manager
                .create_foreign_key(
                    ForeignKey::create()
                        .name(FK_NAME)
                        .from(AppBuilds::Table, AppBuilds::PublishedBy)
                        .to(Users::Table, Users::Id)
                        .on_delete(ForeignKeyAction::SetNull)
                        .on_update(ForeignKeyAction::NoAction)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        if manager.has_column("app_builds", "published_by").await? {
            manager
                .drop_foreign_key(
                    ForeignKey::drop()
                        .table(AppBuilds::Table)
                        .name(FK_NAME)
                        .to_owned(),
                )
                .await?;
            manager
                .alter_table(
                    Table::alter()
                        .table(AppBuilds::Table)
                        .drop_column(AppBuilds::PublishedBy)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}
