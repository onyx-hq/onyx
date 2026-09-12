//! Adds per-build git provenance to `app_builds` — the source repo remote URL,
//! commit sha, and branch captured (best-effort) by `oxyc publish` — so the
//! admin UI can link each build back to its source (Vercel-style). All columns
//! are nullable: existing rows and non-git publishes simply carry none.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[derive(DeriveIden)]
enum AppBuilds {
    Table,
    SourceRepo,
    CommitSha,
    SourceBranch,
}

const COLS: [(&str, AppBuilds); 3] = [
    ("source_repo", AppBuilds::SourceRepo),
    ("commit_sha", AppBuilds::CommitSha),
    ("source_branch", AppBuilds::SourceBranch),
];

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (name, col) in COLS {
            if !manager.has_column("app_builds", name).await? {
                manager
                    .alter_table(
                        Table::alter()
                            .table(AppBuilds::Table)
                            .add_column(ColumnDef::new(col).text())
                            .to_owned(),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for (name, col) in COLS {
            if manager.has_column("app_builds", name).await? {
                manager
                    .alter_table(
                        Table::alter()
                            .table(AppBuilds::Table)
                            .drop_column(col)
                            .to_owned(),
                    )
                    .await?;
            }
        }
        Ok(())
    }
}
