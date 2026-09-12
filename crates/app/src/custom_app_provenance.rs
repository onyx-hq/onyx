//! What a custom-app build records about where its code came from.
//!
//! `app_builds.source_repo` + `commit_sha` is the only path from a running
//! custom app back to its code — the build store holds build output, and
//! there is no source export (see `internal-docs/customer-apps.md` §12). Two
//! places ask whether that path exists, for different audiences:
//!
//! - `oxyc publish` warns the engineer at publish time, when it's still cheap
//!   to fix;
//! - the admin apps list flags builds already in the field, which is where
//!   you find the ones published before anybody was warning.
//!
//! They used to answer it separately and disagreed: the CLI stayed quiet
//! unless BOTH halves were missing while the server flagged EITHER, so a
//! `--dir` publish from CI (commit from `$GITHUB_SHA`, no `origin` to read)
//! published silently and then sat amber in the admin list forever — the
//! exact surprise the publish-time warning exists to prevent. Hence one rule,
//! stated once per language: this module for the server, and — since the
//! publish CLI is TypeScript and can't call it — `provenanceGaps` /
//! `isRecorded` in `sdk/cli/src/publish/provenance.ts`. Change one, change
//! both.
//!
//! The rule: **both halves are required**. A repo with no commit points at a
//! moving branch, and a commit with no repo points nowhere; neither gets you
//! to the code that is actually running. Blank and whitespace-only count as
//! missing — `--repo ""` is not provenance.

/// How completely a build records its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceProvenance {
    /// Repo and commit both present — the link works.
    Complete,
    /// Repo recorded, commit missing: points at a branch, which moves.
    MissingCommit,
    /// Commit recorded, repo missing: a sha with nowhere to resolve it.
    MissingRepo,
    /// Neither recorded. Nothing ties the build to code at all.
    Absent,
}

impl SourceProvenance {
    /// Can someone holding this build actually reach its source?
    pub fn is_traceable(self) -> bool {
        matches!(self, SourceProvenance::Complete)
    }
}

/// Classify a build's recorded `(source_repo, commit_sha)`.
pub fn classify(source_repo: Option<&str>, commit_sha: Option<&str>) -> SourceProvenance {
    let repo = is_recorded(source_repo);
    let commit = is_recorded(commit_sha);
    match (repo, commit) {
        (true, true) => SourceProvenance::Complete,
        (true, false) => SourceProvenance::MissingCommit,
        (false, true) => SourceProvenance::MissingRepo,
        (false, false) => SourceProvenance::Absent,
    }
}

/// Is one half recorded? A field counts only when it has non-whitespace
/// content, so `--repo ""` and `--commit "  "` are not provenance.
///
/// Public because callers sometimes need one half on its own — publish asks
/// "is there a commit for a dirty tree to contradict?" (`oxyc publish`'s
/// `isRecorded` mirrors this) — and they must use the same blank handling as
/// [`classify`] rather than reaching for `is_some()`, which is precisely how
/// the two sides drifted before.
pub fn is_recorded(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_halves_present_is_the_only_traceable_state() {
        assert_eq!(
            classify(Some("git@github.com:o/r.git"), Some("abc123")),
            SourceProvenance::Complete
        );
        assert!(classify(Some("git@github.com:o/r.git"), Some("abc123")).is_traceable());
    }

    #[test]
    fn either_half_missing_is_not_traceable() {
        // The CI shape that used to slip through the CLI silently: `--dir`
        // publish with no checkout, so `$GITHUB_SHA` fills the commit but
        // there's no `origin` remote to read.
        assert_eq!(
            classify(None, Some("abc123")),
            SourceProvenance::MissingRepo
        );
        assert_eq!(
            classify(Some("git@github.com:o/r.git"), None),
            SourceProvenance::MissingCommit
        );
        for p in [
            classify(None, Some("abc123")),
            classify(Some("git@github.com:o/r.git"), None),
            classify(None, None),
        ] {
            assert!(!p.is_traceable(), "{p:?} must not count as traceable");
        }
    }

    #[test]
    fn nothing_recorded_is_absent() {
        assert_eq!(classify(None, None), SourceProvenance::Absent);
    }

    /// `--repo ""` is not provenance. The CLI half used to test `is_none()`
    /// and the server half `trim().is_empty()`, so a blank string split them.
    #[test]
    fn blank_and_whitespace_count_as_missing() {
        assert_eq!(
            classify(Some(""), Some("abc123")),
            SourceProvenance::MissingRepo
        );
        assert_eq!(
            classify(Some("   "), Some("abc123")),
            SourceProvenance::MissingRepo
        );
        assert_eq!(
            classify(Some("git@github.com:o/r.git"), Some("  ")),
            SourceProvenance::MissingCommit
        );
        assert_eq!(classify(Some(""), Some("")), SourceProvenance::Absent);
    }
}
