//! Compile + promote what `oxy seed` created, so a seeded workspace serves on
//! the first request instead of answering `503 needs_recompile`.
//!
//! In-process, through the same entry point as
//! `oxy compile --workspace-id <id> --promote --skip-migrations`: one code path
//! for the revision write, the config gate, and the OLTP apply-then-promote
//! settle — a second copy here is how the two would drift.
//!
//! Every target is attempted even after one fails, and the failures come back as
//! one error at the end: a wrapper script (`just up`) must exit non-zero, but a
//! bad file in one workspace should not hide whether the other nine compile.

use std::path::{Path, PathBuf};

use entity::prelude::Workspaces;
use oxy::theme::StyledText;
use oxy_shared::errors::OxyError;
use sea_orm::{DatabaseConnection, EntityTrait};
use uuid::Uuid;

use super::compile::{CompileArgs, run_compile};

/// One workspace row the seed owns — the unit both the compile and the LLM-key
/// copy act on.
pub(crate) struct SeededWorkspace {
    /// `Demo` or `<org-slug> / <workspace name>`, for the printed summary.
    pub label: String,
    pub id: Uuid,
    pub org_id: Option<Uuid>,
    /// The working copy the row points at, if any.
    pub path: Option<PathBuf>,
}

/// The demo workspace plus every partner/tenant workspace whose row exists.
/// Rows only — a missing one (the tenant seed skips on a non-local DB) is simply
/// not a target.
pub(crate) async fn seeded_workspaces(
    conn: &DatabaseConnection,
    demo_id: Uuid,
) -> Result<Vec<SeededWorkspace>, OxyError> {
    let mut ids = vec![("Demo".to_string(), demo_id)];
    ids.extend(super::seed_partners::seeded_workspace_ids(conn).await?);

    let mut targets = Vec::with_capacity(ids.len());
    for (label, id) in ids {
        let row = Workspaces::find_by_id(id)
            .one(conn)
            .await
            .map_err(|e| OxyError::DBError(format!("query workspace {label}: {e}")))?;
        if let Some(row) = row {
            targets.push(SeededWorkspace {
                label,
                id,
                org_id: row.org_id,
                path: row.path.map(PathBuf::from),
            });
        }
    }
    Ok(targets)
}

enum Outcome {
    Compiled,
    Skipped(&'static str),
    Failed(String),
}

/// Compile + promote every target, print one line per workspace, and fail at the
/// end if any compile did.
pub(crate) async fn compile_seeded(targets: &[SeededWorkspace]) -> Result<(), OxyError> {
    println!(
        "{} compiling + promoting {} seeded workspace{} (skip with --no-compile)",
        "🛠️".info(),
        targets.len(),
        if targets.len() == 1 { "" } else { "s" }
    );
    let mut results = Vec::with_capacity(targets.len());
    for target in targets {
        results.push((target, compile_one(target).await));
    }

    println!("{} compile results:", "📋".info());
    let mut failed = Vec::new();
    for (target, outcome) in &results {
        match outcome {
            Outcome::Compiled => println!("  {} {}", "✓".success(), target.label),
            Outcome::Skipped(why) => println!("  {} {} — skipped: {why}", "–".info(), target.label),
            Outcome::Failed(e) => {
                println!("  {} {} — {e}", "✗".error(), target.label);
                failed.push(target.label.as_str());
            }
        }
    }
    if failed.is_empty() {
        return Ok(());
    }
    Err(OxyError::RuntimeError(format!(
        "compile failed for {} seeded workspace(s): {}. Everything else seeded; fix the \
         files and re-run `oxy seed` (or pass --no-compile to skip)",
        failed.len(),
        failed.join(", ")
    )))
}

async fn compile_one(target: &SeededWorkspace) -> Outcome {
    let path = match working_copy(target.path.as_deref()) {
        Ok(path) => path,
        Err(why) => return Outcome::Skipped(why),
    };
    let args = CompileArgs {
        workspace_path: Some(path),
        workspace_id: Some(target.id),
        git_sha: None,
        branch: None,
        // The seed just wrote rows through this connection, so the schema is there.
        skip_migrations: true,
        json: false,
        promote: true,
        // Only selects a migration set, and migrations are skipped.
        enterprise: false,
    };
    match run_compile(args).await {
        Ok(()) => Outcome::Compiled,
        Err(e) => Outcome::Failed(e.to_string()),
    }
}

/// The absolute working copy to compile, or why there is nothing to compile.
///
/// Absolute because a relative `--workspace-path` under-compiles (see the
/// `airhouse-precompile` recipe); the seed writes absolute paths, but an older
/// row may not carry one.
fn working_copy(path: Option<&Path>) -> Result<PathBuf, &'static str> {
    let path = path.ok_or("no working copy recorded")?;
    let absolute = std::path::absolute(path).map_err(|_| "unresolvable path")?;
    if !absolute.is_dir() {
        return Err("working copy not on disk");
    }
    if !absolute.join("config.yml").is_file() {
        return Err("no config.yml in the working copy");
    }
    Ok(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_workspace_without_files_is_skipped_not_failed() {
        assert_eq!(working_copy(None), Err("no working copy recorded"));
        assert_eq!(
            working_copy(Some(Path::new("/definitely/not/a/seeded/workspace"))),
            Err("working copy not on disk")
        );
        // A real directory that isn't a workspace. Not CARGO_MANIFEST_DIR:
        // `crates/app/config.yml` is a tracked fixture.
        let empty = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            working_copy(Some(empty.path())),
            Err("no config.yml in the working copy")
        );
    }

    #[test]
    fn the_demo_project_is_a_compilable_working_copy() {
        let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let resolved = working_copy(Some(&examples)).expect("examples/ has a config.yml");
        assert!(resolved.is_absolute());
    }
}
