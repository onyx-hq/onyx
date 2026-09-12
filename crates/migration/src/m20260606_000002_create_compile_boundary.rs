//! Phase 1.6a — Compile boundary foundation.
//!
//! Introduces the schema that lets `oxy compile` write a snapshot of a
//! workspace's parsed YAML/SQL into Postgres. See
//! `internal-docs/compile-boundary.md` for the operator runbook; the headline
//! points are:
//!
//! - One `revisions` row per successful (or failed) compile.
//! - Each entity type has its own table tagged by `revision_id` —
//!   uniform shape, point lookups by name/path.
//! - `workspaces.current_revision_id` gets added so the workspace can
//!   eventually point at "the one revision the runtime should read."
//!   In Phase 1.6a the column exists but the worker does NOT promote
//!   to it (observation mode); Phase 1.6b will start promoting.
//! - All FKs from per-entity tables to `revisions` cascade on delete
//!   so cleaning up a failed compile is one DELETE on the revision row.
//! - `workspaces.current_revision_id` uses SET NULL on delete — losing
//!   a current revision is recoverable by re-compiling.
//!
//! All `create_table` + `create_index` calls use `if_not_exists()` so
//! a dev database that ran a partial migration before is replayable.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // ── revisions ─────────────────────────────────────────────────────────
        // The root table. Each compile produces exactly one row here,
        // and every per-entity row downstream references it via
        // (revision_id) FK. A revisions row's `status` transitions
        // compiling → ready | failed; a `ready` revision can be
        // promoted to `workspaces.current_revision_id`.
        manager
            .create_table(
                Table::create()
                    .table(Revisions::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(Revisions::RevisionId)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Revisions::WorkspaceId).uuid().not_null())
                    .col(ColumnDef::new(Revisions::GitSha).text().not_null())
                    .col(ColumnDef::new(Revisions::Branch).text())
                    // Versioned compile contract; bumped when the
                    // shape of compiled rows changes (Vercel Build
                    // Output API analogue).
                    .col(
                        ColumnDef::new(Revisions::SchemaVersion)
                            .integer()
                            .not_null()
                            .default(1),
                    )
                    // 'compiling' | 'ready' | 'failed'.
                    .col(
                        ColumnDef::new(Revisions::Status)
                            .text()
                            .not_null()
                            .default("compiling"),
                    )
                    // 'main' | 'draft'. Draft revisions are scoped to
                    // a single owner_user_id and are not eligible for
                    // promotion to current_revision_id.
                    .col(
                        ColumnDef::new(Revisions::Kind)
                            .text()
                            .not_null()
                            .default("main"),
                    )
                    .col(ColumnDef::new(Revisions::OwnerUserId).uuid())
                    // Recorded so we can reproduce a compile against
                    // a known compiler version. Helps diagnose schema
                    // drift across rolling deploys.
                    .col(ColumnDef::new(Revisions::CompilerVersion).text().not_null())
                    .col(
                        ColumnDef::new(Revisions::StartedAt)
                            .timestamp_with_time_zone()
                            .not_null()
                            .default(Expr::current_timestamp()),
                    )
                    .col(ColumnDef::new(Revisions::FinishedAt).timestamp_with_time_zone())
                    .col(
                        ColumnDef::new(Revisions::FileCountSeen)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Revisions::FileCountCompiled)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(Revisions::FileCountFailed)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    // Structured per-file failure summary; null on
                    // success.
                    .col(ColumnDef::new(Revisions::ErrorSummary).json_binary())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_revisions_workspace_id")
                            .from(Revisions::Table, Revisions::WorkspaceId)
                            .to(Workspaces::Table, Workspaces::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // Drives the "recent revisions per workspace" admin query.
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_revisions_workspace_started")
                    .table(Revisions::Table)
                    .col(Revisions::WorkspaceId)
                    .col(Revisions::StartedAt)
                    .to_owned(),
            )
            .await?;

        // Drives the reaper that marks stuck compiles as failed.
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_revisions_status_started")
                    .table(Revisions::Table)
                    .col(Revisions::Status)
                    .col(Revisions::StartedAt)
                    .to_owned(),
            )
            .await?;

        // ── workspaces.current_revision_id ────────────────────────────────────
        // Forward pointer from a workspace to "the revision the
        // runtime should read." Phase 1.6a leaves this NULL; Phase
        // 1.6b begins promoting successful compiles.
        if !manager
            .has_column("workspaces", "current_revision_id")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(Workspaces::Table)
                        .add_column(ColumnDef::new(Workspaces::CurrentRevisionId).uuid())
                        .to_owned(),
                )
                .await?;
        }
        // Add the FK as `NOT VALID` via raw SQL: a plain `ADD CONSTRAINT ...
        // FOREIGN KEY` takes ACCESS EXCLUSIVE on `workspaces` AND scans it to
        // validate. `NOT VALID` skips the scan (existing rows are all NULL
        // here anyway) and still enforces the constraint for every future
        // write — so we never need a follow-up `VALIDATE`. The `DO` guard keeps
        // `up()` replayable on a partially-applied dev DB (SeaORM's
        // `create_foreign_key` has no `IF NOT EXISTS`).
        manager
            .get_connection()
            .execute_unprepared(
                "DO $$ BEGIN \
                   IF NOT EXISTS ( \
                     SELECT 1 FROM pg_constraint \
                     WHERE conname = 'fk_workspaces_current_revision_id' \
                   ) THEN \
                     ALTER TABLE workspaces \
                       ADD CONSTRAINT fk_workspaces_current_revision_id \
                       FOREIGN KEY (current_revision_id) \
                       REFERENCES revisions(revision_id) \
                       ON DELETE SET NULL NOT VALID; \
                   END IF; \
                 END $$;",
            )
            .await?;

        // ── workspace_compiled_configs ────────────────────────────────────────
        // The compiled view of config.yml. One row per revision.
        // Fields are unstructured JSONB rather than fully normalised
        // because the existing `Config` struct is huge and the
        // structure is the dbt-shaped source format — we mirror it
        // 1:1 so the runtime read can reconstruct a `Config` without
        // a translation layer.
        manager
            .create_table(
                Table::create()
                    .table(WorkspaceCompiledConfigs::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(WorkspaceCompiledConfigs::RevisionId)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(WorkspaceCompiledConfigs::Databases)
                            .json_binary()
                            .not_null(),
                    )
                    .col(ColumnDef::new(WorkspaceCompiledConfigs::Models).json_binary())
                    .col(ColumnDef::new(WorkspaceCompiledConfigs::Integrations).json_binary())
                    .col(ColumnDef::new(WorkspaceCompiledConfigs::Repositories).json_binary())
                    .col(ColumnDef::new(WorkspaceCompiledConfigs::BuilderAgent).json_binary())
                    .col(ColumnDef::new(WorkspaceCompiledConfigs::Mcp).json_binary())
                    // Catch-all for fields not surfaced above (defaults,
                    // env vars, etc.). Lets us add new top-level keys to
                    // config.yml without a migration on day one.
                    .col(ColumnDef::new(WorkspaceCompiledConfigs::Other).json_binary())
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_compiled_configs_revision")
                            .from(
                                WorkspaceCompiledConfigs::Table,
                                WorkspaceCompiledConfigs::RevisionId,
                            )
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── agent_definitions (.agentic.yml) ──────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(AgentDefinitions::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AgentDefinitions::RevisionId)
                            .uuid()
                            .not_null(),
                    )
                    .col(ColumnDef::new(AgentDefinitions::Name).text().not_null())
                    .col(ColumnDef::new(AgentDefinitions::FilePath).text().not_null())
                    .col(
                        ColumnDef::new(AgentDefinitions::Definition)
                            .json_binary()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(AgentDefinitions::RevisionId)
                            .col(AgentDefinitions::Name),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_agent_definitions_revision")
                            .from(AgentDefinitions::Table, AgentDefinitions::RevisionId)
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── semantic_views (.view.yml) ────────────────────────────────────────
        // `definition` mirrors the parsed view; `compiled_sql_blob_key`
        // is reserved for Phase 1.6b when we move large pre-dialect
        // SQL bodies into S3.
        manager
            .create_table(
                Table::create()
                    .table(SemanticViews::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(SemanticViews::RevisionId).uuid().not_null())
                    .col(ColumnDef::new(SemanticViews::Name).text().not_null())
                    .col(ColumnDef::new(SemanticViews::FilePath).text().not_null())
                    .col(
                        ColumnDef::new(SemanticViews::Definition)
                            .json_binary()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SemanticViews::CompiledSqlBlobKey).text())
                    .primary_key(
                        Index::create()
                            .col(SemanticViews::RevisionId)
                            .col(SemanticViews::Name),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_semantic_views_revision")
                            .from(SemanticViews::Table, SemanticViews::RevisionId)
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── semantic_topics (.topic.yml) ──────────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(SemanticTopics::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(SemanticTopics::RevisionId).uuid().not_null())
                    .col(ColumnDef::new(SemanticTopics::Name).text().not_null())
                    .col(ColumnDef::new(SemanticTopics::FilePath).text().not_null())
                    .col(
                        ColumnDef::new(SemanticTopics::Definition)
                            .json_binary()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SemanticTopics::CompiledSqlBlobKey).text())
                    .primary_key(
                        Index::create()
                            .col(SemanticTopics::RevisionId)
                            .col(SemanticTopics::Name),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_semantic_topics_revision")
                            .from(SemanticTopics::Table, SemanticTopics::RevisionId)
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── app_definitions (.app.yml) ────────────────────────────────────────
        // Keyed by file_path because .app.yml files are most-often
        // referenced by path in the UI ("/apps/<pathb64>") and a
        // workspace can have multiple apps with the same configured
        // name across folders.
        manager
            .create_table(
                Table::create()
                    .table(AppDefinitions::Table)
                    .if_not_exists()
                    .col(ColumnDef::new(AppDefinitions::RevisionId).uuid().not_null())
                    .col(ColumnDef::new(AppDefinitions::FilePath).text().not_null())
                    .col(ColumnDef::new(AppDefinitions::Name).text().not_null())
                    .col(
                        ColumnDef::new(AppDefinitions::Definition)
                            .json_binary()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AppDefinitions::Published)
                            .boolean()
                            .not_null()
                            .default(false),
                    )
                    .primary_key(
                        Index::create()
                            .col(AppDefinitions::RevisionId)
                            .col(AppDefinitions::FilePath),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_app_definitions_revision")
                            .from(AppDefinitions::Table, AppDefinitions::RevisionId)
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── procedure_definitions ─────────────────────────────────────────────
        // Covers .procedure.yml (legacy, back-compat) and .automation.yml
        // (canonical) — both parse to the same Workflow type. `extension`
        // is recorded so a future deprecation step can grep for the legacy one.
        manager
            .create_table(
                Table::create()
                    .table(ProcedureDefinitions::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ProcedureDefinitions::RevisionId)
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ProcedureDefinitions::FilePath)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(ProcedureDefinitions::Name).text().not_null())
                    .col(
                        ColumnDef::new(ProcedureDefinitions::Extension)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ProcedureDefinitions::Definition)
                            .json_binary()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(ProcedureDefinitions::RevisionId)
                            .col(ProcedureDefinitions::FilePath),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_procedure_definitions_revision")
                            .from(
                                ProcedureDefinitions::Table,
                                ProcedureDefinitions::RevisionId,
                            )
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── verified_queries (.sql files) ─────────────────────────────────────
        // `content_sha256` lets the runtime answer "is this verified
        // query equivalent to the one the analytics agent matched on
        // its last run?" without re-hashing.
        manager
            .create_table(
                Table::create()
                    .table(VerifiedQueries::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(VerifiedQueries::RevisionId)
                            .uuid()
                            .not_null(),
                    )
                    .col(ColumnDef::new(VerifiedQueries::FilePath).text().not_null())
                    .col(
                        ColumnDef::new(VerifiedQueries::ContentSha256)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(VerifiedQueries::Content).text().not_null())
                    .primary_key(
                        Index::create()
                            .col(VerifiedQueries::RevisionId)
                            .col(VerifiedQueries::FilePath),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_verified_queries_revision")
                            .from(VerifiedQueries::Table, VerifiedQueries::RevisionId)
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── airway_pipelines (.airway.yml) ────────────────────────────────────
        manager
            .create_table(
                Table::create()
                    .table(AirwayPipelines::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AirwayPipelines::RevisionId)
                            .uuid()
                            .not_null(),
                    )
                    .col(ColumnDef::new(AirwayPipelines::Name).text().not_null())
                    .col(ColumnDef::new(AirwayPipelines::FilePath).text().not_null())
                    .col(
                        ColumnDef::new(AirwayPipelines::Definition)
                            .json_binary()
                            .not_null(),
                    )
                    .primary_key(
                        Index::create()
                            .col(AirwayPipelines::RevisionId)
                            .col(AirwayPipelines::Name),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_airway_pipelines_revision")
                            .from(AirwayPipelines::Table, AirwayPipelines::RevisionId)
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        // ── compiled_references ───────────────────────────────────────────────
        // Cross-entity reference graph. Powers fast "who calls X" /
        // "what does Y depend on" without scanning every entity's
        // JSON body. Already maps naturally to the Context Graph
        // page, which currently re-derives this from FS walks.
        manager
            .create_table(
                Table::create()
                    .table(CompiledReferences::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(CompiledReferences::RevisionId)
                            .uuid()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CompiledReferences::FromKind)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(CompiledReferences::FromName)
                            .text()
                            .not_null(),
                    )
                    .col(ColumnDef::new(CompiledReferences::ToKind).text().not_null())
                    .col(ColumnDef::new(CompiledReferences::ToName).text().not_null())
                    .primary_key(
                        Index::create()
                            .col(CompiledReferences::RevisionId)
                            .col(CompiledReferences::FromKind)
                            .col(CompiledReferences::FromName)
                            .col(CompiledReferences::ToKind)
                            .col(CompiledReferences::ToName),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_compiled_references_revision")
                            .from(CompiledReferences::Table, CompiledReferences::RevisionId)
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_compiled_references_from")
                    .table(CompiledReferences::Table)
                    .col(CompiledReferences::RevisionId)
                    .col(CompiledReferences::FromKind)
                    .col(CompiledReferences::FromName)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .if_not_exists()
                    .name("idx_compiled_references_to")
                    .table(CompiledReferences::Table)
                    .col(CompiledReferences::RevisionId)
                    .col(CompiledReferences::ToKind)
                    .col(CompiledReferences::ToName)
                    .to_owned(),
            )
            .await?;

        // ── multi-worker idempotency safety net ───────────────────────────────
        // Partial unique index on `revisions(workspace_id, git_sha)`
        // WHERE `status = 'ready' AND kind = 'main'`. Closes the TOCTOU
        // window between two workers' idempotency lookup + finalise tx
        // by turning it into a clean Postgres unique-violation that the
        // writer catches and converts into SupersededBy.
        //
        // Partial because compiling / failed / draft rows MUST coexist
        // — only ready+main is the runtime-readable, promotion-eligible
        // state we need uniqueness on.
        //
        // Raw SQL because Sea-ORM doesn't expose partial-index syntax
        // (the `WHERE` clause after the column list). Plain `CREATE
        // INDEX` (no CONCURRENTLY) is fine — the table is brand-new in
        // this migration, zero rows. Future migrations adding a
        // comparable index to an already-busy `revisions` must use
        // CONCURRENTLY via `execute_unprepared`, in a migration whose
        // `use_transaction()` returns `Some(false)` — sea-orm-migration 2.0
        // runs every other migration in a transaction, where CONCURRENTLY
        // can't run. See `m20260911_000002_function_failure_fingerprint_index`.
        manager
            .get_connection()
            .execute_unprepared(
                "CREATE UNIQUE INDEX IF NOT EXISTS idx_revisions_idempotent_ready_main \
                 ON revisions (workspace_id, git_sha) \
                 WHERE status = 'ready' AND kind = 'main'",
            )
            .await?;

        // ── monitor_configs (.monitor.yml) ────────────────────────────────────
        // Singleton per revision — one `.monitor.yml` per workspace, so
        // the PK is just `revision_id`. The full payload (top-level
        // schedule + monitors) lives in a single JSONB column;
        // runtime round-trips it back into the strict-typed
        // MonitorConfig via serde_json::from_value.
        manager
            .create_table(
                Table::create()
                    .table(MonitorConfigs::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(MonitorConfigs::RevisionId)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(MonitorConfigs::Definition)
                            .json_binary()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_monitor_configs_revision")
                            .from(MonitorConfigs::Table, MonitorConfigs::RevisionId)
                            .to(Revisions::Table, Revisions::RevisionId)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;

        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Drop in reverse dependency order: children → parent →
        // workspace FK → workspaces column → revisions table.
        manager
            .get_connection()
            .execute_unprepared("DROP INDEX IF EXISTS idx_revisions_idempotent_ready_main")
            .await?;

        // Raw `DROP INDEX IF EXISTS` (not SeaORM's `drop_index`) so a partially
        // applied `up()` can still be rolled back — matches the guarded,
        // idempotent style of the rest of this migration.
        for idx in [
            "idx_compiled_references_to",
            "idx_compiled_references_from",
            "idx_revisions_status_started",
            "idx_revisions_workspace_started",
        ] {
            manager
                .get_connection()
                .execute_unprepared(&format!("DROP INDEX IF EXISTS {idx}"))
                .await?;
        }

        for table in [
            MonitorConfigs::Table.into_iden(),
            CompiledReferences::Table.into_iden(),
            AirwayPipelines::Table.into_iden(),
            VerifiedQueries::Table.into_iden(),
            ProcedureDefinitions::Table.into_iden(),
            AppDefinitions::Table.into_iden(),
            SemanticTopics::Table.into_iden(),
            SemanticViews::Table.into_iden(),
            AgentDefinitions::Table.into_iden(),
            WorkspaceCompiledConfigs::Table.into_iden(),
        ] {
            manager
                .drop_table(Table::drop().table(table).if_exists().to_owned())
                .await?;
        }

        // FK on workspaces → revisions must drop before the column or
        // the revisions table. `IF EXISTS` so a partial rollback doesn't abort.
        manager
            .get_connection()
            .execute_unprepared(
                "ALTER TABLE workspaces \
                 DROP CONSTRAINT IF EXISTS fk_workspaces_current_revision_id",
            )
            .await?;
        if manager
            .has_column("workspaces", "current_revision_id")
            .await?
        {
            manager
                .alter_table(
                    Table::alter()
                        .table(Workspaces::Table)
                        .drop_column(Workspaces::CurrentRevisionId)
                        .to_owned(),
                )
                .await?;
        }

        manager
            .drop_table(Table::drop().table(Revisions::Table).if_exists().to_owned())
            .await?;

        Ok(())
    }
}

#[derive(DeriveIden)]
enum Workspaces {
    Table,
    Id,
    CurrentRevisionId,
}

#[derive(DeriveIden)]
enum Revisions {
    Table,
    RevisionId,
    WorkspaceId,
    GitSha,
    Branch,
    SchemaVersion,
    Status,
    Kind,
    OwnerUserId,
    CompilerVersion,
    StartedAt,
    FinishedAt,
    FileCountSeen,
    FileCountCompiled,
    FileCountFailed,
    ErrorSummary,
}

#[derive(DeriveIden)]
enum WorkspaceCompiledConfigs {
    Table,
    RevisionId,
    Databases,
    Models,
    Integrations,
    Repositories,
    BuilderAgent,
    Mcp,
    Other,
}

#[derive(DeriveIden)]
enum AgentDefinitions {
    Table,
    RevisionId,
    Name,
    FilePath,
    Definition,
}

#[derive(DeriveIden)]
enum SemanticViews {
    Table,
    RevisionId,
    Name,
    FilePath,
    Definition,
    CompiledSqlBlobKey,
}

#[derive(DeriveIden)]
enum SemanticTopics {
    Table,
    RevisionId,
    Name,
    FilePath,
    Definition,
    CompiledSqlBlobKey,
}

#[derive(DeriveIden)]
enum AppDefinitions {
    Table,
    RevisionId,
    FilePath,
    Name,
    Definition,
    Published,
}

#[derive(DeriveIden)]
enum ProcedureDefinitions {
    Table,
    RevisionId,
    FilePath,
    Name,
    Extension,
    Definition,
}

#[derive(DeriveIden)]
enum VerifiedQueries {
    Table,
    RevisionId,
    FilePath,
    ContentSha256,
    Content,
}

#[derive(DeriveIden)]
enum AirwayPipelines {
    Table,
    RevisionId,
    Name,
    FilePath,
    Definition,
}

#[derive(DeriveIden)]
enum CompiledReferences {
    Table,
    RevisionId,
    FromKind,
    FromName,
    ToKind,
    ToName,
}

#[derive(DeriveIden)]
enum MonitorConfigs {
    Table,
    RevisionId,
    Definition,
}
