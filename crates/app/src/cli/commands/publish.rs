//! `oxy publish` — self-contained, CI-free deploy for custom apps.
//!
//! From an app directory an engineer runs `oxy publish --env <env>`. The
//! command reads `oxy-app.json`, builds the bundle per its `build` section
//! (or defaults), resolves the target oxy from its `environments` (or
//! defaults), auto-resolves the project from `<target>/api/apps/<org>/<app>/
//! build-config`, authenticates with the token cached by `oxy login` (or
//! `OXY_TOKEN`), and POSTs the tarball to `<target>/api/customer-apps/publish`.
//!
//! Nothing lives in GitHub: no project id, no target var, no per-app build
//! steps — the manifest + `oxy login` carry it all. `--dir` skips the build
//! and uploads a pre-built directory (escape hatch / CI).

use std::path::{Path, PathBuf};

use clap::Parser;
use flate2::Compression;
use flate2::write::GzEncoder;
use oxy::theme::StyledText;
use oxy_shared::errors::OxyError;
use serde::Deserialize;

use crate::custom_app_provenance::{SourceProvenance, classify, is_recorded};
use crate::server::api::custom_apps_publish::is_valid_function_name;

use super::app_manifest::{OxyAppManifest, resolve_target};
use super::login;

#[derive(Parser, Debug)]
pub struct PublishArgs {
    /// Environment to publish to (resolves the target oxy from oxy-app.json
    /// `environments`, else a built-in default). E.g. local, dev, production.
    #[arg(long, default_value = "production")]
    env: String,
    /// Explicit oxy base URL; overrides `--env`.
    #[arg(long)]
    target: Option<String>,
    /// Org identity. Accepts either a slug (`acme`) or a UUID — the
    /// server auto-detects which form was passed. UUIDs are useful
    /// when the same engineering team publishes to multiple envs where
    /// the slug has drifted (renamed in prod but not staging) and you
    /// want a stable handle that works everywhere. Default: oxy-app.json
    /// `orgSlug`, then OXY_ORG, then the `<org>` segment of an
    /// `apps/<org>/<app>/` working directory.
    #[arg(long)]
    org: Option<String>,
    /// App slug. Default: oxy-app.json `slug`, then OXY_APP, then the
    /// `<app>` segment of an `apps/<org>/<app>/` working directory.
    #[arg(long)]
    app: Option<String>,
    /// Project id (UUID). Default: resolved from the target's build-config
    /// endpoint. Override only for unusual setups. Env: OXY_PROJECT.
    #[arg(long)]
    project: Option<String>,
    /// Name of the env var holding the bearer token. Default: OXY_TOKEN.
    /// Falls back to the `oxy login` cache for the target host.
    #[arg(long = "token-env", default_value = "OXY_TOKEN")]
    token_env: String,
    /// Engineer-facing build version. Must be unique per app — the server
    /// rejects a reused one (409). Default: $GITHUB_SHA qualified by
    /// $GITHUB_RUN_ID/$GITHUB_RUN_ATTEMPT so a CI re-run gets its own id,
    /// else random.
    #[arg(long)]
    build_id: Option<String>,
    /// Skip the build and publish this pre-built directory as-is. When
    /// omitted, `oxy publish` runs the manifest's build and uploads its
    /// output dir.
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Publish straight to the live (published) channel instead of draft.
    #[arg(long)]
    promote: bool,
    /// Optional display name override for the app row.
    #[arg(long)]
    name: Option<String>,
    /// Git remote URL to record for this build. Defaults to the working
    /// tree's `origin` remote; override in CI.
    #[arg(long)]
    repo: Option<String>,
    /// Commit sha to record. Defaults to the working tree's HEAD (or
    /// `$GITHUB_SHA`).
    #[arg(long)]
    commit: Option<String>,
    /// Branch to record. Defaults to the working tree's current branch (or
    /// `$GITHUB_REF_NAME`).
    #[arg(long)]
    branch: Option<String>,
}

/// Infer `(org, app)` from a working dir shaped like `.../apps/<org>/<app>[/...]`.
fn infer_org_app_from_cwd() -> Option<(String, String)> {
    let cwd = std::env::current_dir().ok()?;
    let parts: Vec<String> = cwd
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(str::to_string),
            _ => None,
        })
        .collect();
    let idx = parts.iter().rposition(|p| p == "apps")?;
    let org = parts.get(idx + 1)?.clone();
    let app = parts.get(idx + 2)?.clone();
    Some((org, app))
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.trim().is_empty())
}

/// Default build id under CI: the commit, qualified by the run that built it.
///
/// `GITHUB_SHA` alone is **not** unique per publish — "Re-run all jobs" on the
/// same commit publishes under the same id a second time. That id is the S3
/// prefix a build's bytes live under and the key `custom_apps_bundle_cache`
/// caches by, including its cached *absences*, so a second publish under a
/// reused id merges into a build replicas have already cached: a file the
/// rebuild adds reads as permanently missing until the process restarts.
///
/// `GITHUB_RUN_ID` is unique per workflow run and `GITHUB_RUN_ATTEMPT`
/// distinguishes re-runs of that same run, so the pair is exactly the axis
/// `GITHUB_SHA` is missing. The commit stays the prefix because it is what
/// makes a build id readable in the admin UI. Each falls back independently —
/// a non-GitHub CI that sets only `GITHUB_SHA` keeps today's behavior, and the
/// server rejects the collision if it happens (`custom_apps_publish`).
fn ci_build_id() -> Option<String> {
    let sha = env_var("GITHUB_SHA")?;
    Some(
        match (env_var("GITHUB_RUN_ID"), env_var("GITHUB_RUN_ATTEMPT")) {
            (Some(run), Some(attempt)) => format!("{sha}-{run}.{attempt}"),
            (Some(run), None) => format!("{sha}-{run}"),
            (None, _) => sha,
        },
    )
}

/// Best-effort git provenance for the working tree: `(remote_url, commit_sha,
/// branch)`. Each entry is `None` when the command fails (not a repo, no
/// `origin`, detached HEAD) — publish must never fail because git is
/// unavailable (e.g. a `--dir` publish from CI with no working tree).
fn capture_git_source(dir: &std::path::Path) -> (Option<String>, Option<String>, Option<String>) {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
        (!s.is_empty()).then_some(s)
    };
    (
        git(&["remote", "get-url", "origin"]),
        git(&["rev-parse", "HEAD"]),
        git(&["rev-parse", "--abbrev-ref", "HEAD"]),
    )
}

/// Does the working tree have uncommitted changes? `None` when git can't
/// answer (not a repo, no git binary) — indistinguishable from "clean" for
/// our purposes, and never a reason to say anything.
///
/// `--porcelain` covers staged, unstaged, and untracked files. Untracked
/// counts on purpose: a new `src/hooks/useThing.ts` that was never added is
/// exactly the file that won't be at the recorded commit.
///
/// **Scoped to `dir` by the `-- .` pathspec, not just `current_dir`.**
/// `current_dir` only decides where git is *invoked*; `git status` still
/// reports the whole repository. The layout this command is built around puts
/// many apps in one repo (`apps/<org>/<app>/`, which `infer_org_app_from_cwd`
/// relies on), so unscoped it fires on a colleague's edit to a different app,
/// or any stray non-ignored file anywhere in the tree — and a warning that
/// cries wolf is one people learn to skip past, which for this warning is the
/// whole failure mode.
///
/// The trade-off, taken knowingly: dirt in a shared workspace package that the
/// bundle imports no longer counts, so a monorepo app can still ship changes
/// its recorded commit lacks. Narrow-and-believed beats broad-and-ignored.
fn git_worktree_is_dirty(dir: &std::path::Path) -> Option<bool> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["status", "--porcelain", "--", "."])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(!String::from_utf8_lossy(&out.stdout).trim().is_empty())
}

/// A way the provenance we're about to record fails to describe what's being
/// shipped. All are warnings, never errors — publishing from a scratch
/// directory or mid-edit is legitimate, and a publish path that can refuse
/// for a *bookkeeping* reason is a publish path people route around.
///
/// Flat variants rather than one wrapping a [`SourceProvenance`]: the wrapper
/// could hold `Complete`/`Absent`, states that are not gaps at all, and the
/// wildcard arm needed to make `message()` compile would have handed a new
/// `SourceProvenance` variant the wrong copy instead of failing the build.
#[derive(Debug, PartialEq, Eq)]
enum ProvenanceGap {
    /// Nothing links the build to a repo, so if the author moves on, nobody
    /// can find the code. The admin UI flags these builds too; this is the
    /// earlier, cheaper place to hear about it.
    NoSource,
    /// Commit recorded, repo missing. Reported separately from `NoSource`
    /// because the fix is much smaller — you already publish from a checkout,
    /// you're just missing a flag.
    MissingRepo,
    /// Repo recorded, commit missing. Same reasoning, different flag.
    MissingCommit,
    /// A commit is recorded, but the bundle was built from something else.
    /// Silently wrong is worse than absent: an operator following the commit
    /// link gets code that looks right and isn't.
    DirtyTree,
}

impl ProvenanceGap {
    fn message(&self) -> &'static str {
        match self {
            ProvenanceGap::NoSource => {
                "warning: no git source recorded for this build — nobody will be able to trace \
                 it back to code. Publish from the app's git checkout, or pass --repo/--commit."
            }
            ProvenanceGap::MissingCommit => {
                "warning: a git repo is recorded for this build but no commit — the link points \
                 at a branch, which moves, so it won't identify the code that is running. \
                 Pass --commit, or publish from the checkout."
            }
            ProvenanceGap::MissingRepo => {
                "warning: a commit is recorded for this build but no git repo — there is nowhere \
                 to resolve that sha. Pass --repo, or publish from the checkout."
            }
            ProvenanceGap::DirtyTree => {
                "warning: publishing from a dirty working tree — the commit recorded for this \
                 build does NOT contain the changes you are shipping. Commit and re-publish if \
                 this build needs to be reproducible."
            }
        }
    }
}

/// Pure decision, split from the git call + `println!` so it can be tested.
///
/// Traceability is delegated to [`custom_app_provenance::classify`], the same
/// call the admin apps list makes. Answering it here independently is what let
/// the two drift: this warned only when BOTH halves were missing while the
/// server flagged EITHER, so a `--dir` CI publish (commit from `$GITHUB_SHA`,
/// no `origin` to read) shipped silently and then sat amber in the admin list.
///
/// Returns **every** gap, not the worst one. Incompleteness and dirtiness are
/// orthogonal — a `--dir` CI publish can be missing its repo *and* built from
/// a dirty tree — so ranking them drops one, and the one that got dropped was
/// `DirtyTree`, which the doc calls the worst of the three.
///
/// `dirty` is `None` when git couldn't answer, which we treat as clean: a
/// `--dir` publish from a directory that isn't a checkout has nothing to be
/// dirty about, and guessing would nag every CI run.
fn provenance_gap(
    dirty: Option<bool>,
    commit_sha: Option<&str>,
    source_repo: Option<&str>,
) -> Vec<ProvenanceGap> {
    let mut gaps = Vec::new();
    match classify(source_repo, commit_sha) {
        SourceProvenance::Absent => gaps.push(ProvenanceGap::NoSource),
        SourceProvenance::MissingRepo => gaps.push(ProvenanceGap::MissingRepo),
        SourceProvenance::MissingCommit => gaps.push(ProvenanceGap::MissingCommit),
        SourceProvenance::Complete => {}
    }
    // Keyed on the commit alone, not on a complete record: a recorded sha goes
    // into the `app_builds` row and is contradicted by a dirty tree whether or
    // not the repo half made it too. Only `MissingCommit`/`Absent` have no
    // claim for the working copy to disagree with.
    if dirty == Some(true) && is_recorded(commit_sha) {
        gaps.push(ProvenanceGap::DirtyTree);
    }
    gaps
}

fn warn_on_weak_provenance(dir: &Path, commit_sha: Option<&str>, source_repo: Option<&str>) {
    // Only shell out to git when its answer can change the output — a build
    // with no recorded commit can't produce `DirtyTree` no matter what the
    // working copy looks like.
    let dirty = is_recorded(commit_sha)
        .then(|| git_worktree_is_dirty(dir))
        .flatten();
    for gap in provenance_gap(dirty, commit_sha, source_repo) {
        println!("{}", gap.message().warning());
    }
}

/// What's sitting in the bundle's `functions/` dir that this publish didn't
/// put there, split by what it most likely is. Both lists are sorted, because
/// `read_dir` order is unspecified and naming one arbitrary file per run makes
/// an author fix a build output one publish at a time.
#[derive(Debug, Default, PartialEq, Eq)]
struct FunctionsDirConflicts {
    /// A file Oxy itself plausibly wrote: `<name>.js` where `<name>` satisfies
    /// [`is_valid_function_name`], for a function no longer declared. Gets its
    /// own sentence — diagnosing it as a frontend collision told the author to
    /// "emit your frontend somewhere else" about Oxy's own output.
    ///
    /// **A guess, and known to be imperfect.** It matches the grammar that
    /// produced our artifacts rather than the `.js` suffix, which keeps hashed
    /// chunks and `_app.js` out — but plenty of unhashed bundler output
    /// (`main.js`, `index.js`, `runtime.js`, `polyfills.js`; webpack's default
    /// `output.filename`, Angular's `--output-hashing=none`) satisfies
    /// `^[a-z][a-z0-9-]{0,63}$` and lands here too. A filename alone cannot
    /// settle it.
    ///
    /// That residual is tolerable *because the bucket no longer decides the
    /// outcome* — see [`enforce_reserved_functions_dir`]. Landing a frontend
    /// file here costs it a slightly wrong sentence, not a silent merge.
    ///
    /// **The exact test is available when it's worth having**: add
    /// `--banner:js=//# oxy-function:<name>` to the esbuild args in
    /// [`bundle_functions`] and match on the first line here. That is the
    /// prerequisite for ever giving this split authority over severity again
    /// (a mild path for leftovers, say) — the grammar is not strong enough to
    /// carry that, and the banner would be. Forward-only, so it only becomes
    /// useful once pre-banner builds have aged out.
    stale_artifacts: Vec<String>,
    /// Anything else — a page, an asset, a bundler chunk, a nested directory.
    /// A real collision between the frontend's output and a reserved path.
    frontend: Vec<String>,
}

impl FunctionsDirConflicts {
    fn is_empty(&self) -> bool {
        self.stale_artifacts.is_empty() && self.frontend.is_empty()
    }
}

/// Could this filename have been written by a previous `bundle_functions` run?
fn looks_like_our_artifact(name: &str) -> bool {
    name.strip_suffix(".js").is_some_and(is_valid_function_name)
}

/// Inspect the reserved directory. "Ours" is `<declared-function-name>.js`.
///
/// An absent directory is the normal case and yields nothing; any other
/// `read_dir` failure propagates, because "can't tell" must not read as "no
/// collision" — that would surface later as a bare `mkdir` error instead of
/// the reserved-path explanation. Per-entry failures propagate for the same
/// reason: silently dropping a filename we couldn't stat would under-report
/// the conflict rather than admit we didn't look.
fn functions_dir_conflicts<'a>(
    out_fns: &Path,
    declared: impl Iterator<Item = &'a String>,
) -> Result<FunctionsDirConflicts, OxyError> {
    let expected: std::collections::HashSet<String> =
        declared.map(|name| format!("{name}.js")).collect();
    let entries = match std::fs::read_dir(out_fns) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FunctionsDirConflicts::default());
        }
        Err(e) => {
            return Err(OxyError::RuntimeError(format!(
                "cannot inspect the reserved bundle path {}: {e}",
                out_fns.display()
            )));
        }
    };
    let entries: Vec<std::fs::DirEntry> = entries.collect::<Result<Vec<_>, _>>().map_err(|e| {
        OxyError::RuntimeError(format!(
            "cannot read an entry of the reserved bundle path {}: {e}",
            out_fns.display()
        ))
    })?;

    let mut out = FunctionsDirConflicts::default();
    for name in entries
        .into_iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !expected.contains(name))
    {
        if looks_like_our_artifact(&name) {
            out.stale_artifacts.push(name);
        } else {
            out.frontend.push(name);
        }
    }
    out.stale_artifacts.sort();
    out.frontend.sort();
    Ok(out)
}

/// Render a bounded, stable list of colliding filenames for an operator.
fn describe_entries(entries: &[String]) -> String {
    const SHOWN: usize = 5;
    let head = entries
        .iter()
        .take(SHOWN)
        .map(|e| format!("functions/{e}"))
        .collect::<Vec<_>>()
        .join(", ");
    match entries.len().checked_sub(SHOWN) {
        Some(extra) if extra > 0 => format!("{head} (and {extra} more)"),
        _ => head,
    }
}

/// Enforce `functions/` as a reserved path in the published bundle.
///
/// Runs for **every** app, not just those declaring Oxy Functions: the serve
/// guard blocks `functions/<child>` unconditionally, so an app with no
/// functions that emits into that directory ships files that can never be
/// served. Skipping the check there left the loud error for apps with
/// functions and silent breakage for apps without — backwards, since the
/// second group has no reason to expect the directory to be special.
///
/// Severity turns **only** on whether we are about to *modify* the directory,
/// never on which bucket a file landed in:
///
/// - **Writing** (the manifest declares functions) — any conflict refuses the
///   publish, and one message covers every class of conflict.
/// - **Not writing** — one warning, because erroring would refuse a publish
///   that succeeds today, which §5b-bis's reasoning (never gate on
///   bookkeeping) argues against.
///
/// Letting the bucket pick the severity was a bug: [`looks_like_our_artifact`]
/// is a guess from a filename, and a wrong "leftover" verdict silently
/// permitted exactly the merge this check exists to refuse (`main.js` and
/// `index.js` satisfy the grammar). The misclassification is asymmetric — a
/// wrong "frontend" verdict is a loud stop whose advice still applies — so the
/// guess now chooses wording only. Refusing a genuine leftover is fine: "clean
/// the build directory" is the right instruction either way.
///
/// Both buckets are reported together rather than one error short-circuiting
/// the other's warning, so an author isn't sent round the loop once per class
/// of problem — the same reason [`describe_entries`] names conflicts within a
/// class rather than one arbitrary file.
///
/// When nothing is declared we can't attribute anything to a previous publish
/// and there may be no `oxy-app.json` at all (`--dir` with `--org`/`--app` is
/// a supported shape), so that branch reports both buckets in one sentence
/// that names neither a manifest nor a prior publish.
fn enforce_reserved_functions_dir<'a>(
    bundle_dir: &Path,
    declared: impl Iterator<Item = &'a String>,
    we_will_write: bool,
) -> Result<(), OxyError> {
    let out_fns = bundle_dir.join("functions");
    let conflicts = functions_dir_conflicts(&out_fns, declared)?;
    if conflicts.is_empty() {
        return Ok(());
    }

    if !we_will_write {
        // No declared functions: nothing of ours goes in, nothing came out of
        // a previous publish we can point at, and the manifest this would cite
        // may not exist. One sentence, no unfounded attribution.
        //
        // Capped per bucket and joined, not capped over a merged list: neither
        // list's wording names its class here, so the split costs nothing —
        // but merging first let alphabetical order decide which class gets
        // named. Six leftover-shaped filenames sorting ahead of `index.html`
        // would push the one genuinely surprising entry into `(and 1 more)`.
        let listed = [&conflicts.frontend, &conflicts.stale_artifacts]
            .into_iter()
            .filter(|list| !list.is_empty())
            .map(|list| describe_entries(list))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "{}",
            format!(
                "warning: this bundle declares no Oxy Functions, but its build output contains \
                 {listed}. `functions/` is reserved in a published bundle — those files will be \
                 uploaded and will NOT be served. Emit them somewhere else."
            )
            .warning()
        );
        return Ok(());
    }

    // About to write: EVERY conflict stops the publish, and one message covers
    // both classes. The bucket split now only picks the wording, so a
    // misclassification mis-explains rather than deciding the outcome.
    let mut detail = String::new();
    if !conflicts.frontend.is_empty() {
        detail.push_str(&format!(
            "\n  - {} — looks like frontend output; emit it somewhere else",
            describe_entries(&conflicts.frontend)
        ));
    }
    if !conflicts.stale_artifacts.is_empty() {
        detail.push_str(&format!(
            "\n  - {} — looks like an artifact from a previous publish, for a function \
             oxy-app.json no longer declares",
            describe_entries(&conflicts.stale_artifacts)
        ));
    }
    Err(OxyError::RuntimeError(format!(
        "`functions/` is reserved for Oxy Functions in a published bundle, and this build output \
         already contains files Oxy did not put there:{detail}\n\
         Publishing would upload them into a path the serve plane blocks, leaving them \
         permanently unreachable. Emit them somewhere else, or clean the build directory and \
         re-publish."
    )))
}

/// Strip any `userinfo@` from a scheme URL (e.g. an embedded
/// `https://x-access-token:<TOKEN>@github.com/org/repo`) before it is
/// persisted — a credentialed remote must never reach Postgres or the admin
/// builds API. The scp-like SSH form (`git@github.com:org/repo`) carries no
/// secret (the `git` user is fixed) and is left untouched.
fn sanitize_remote_url(url: &str) -> String {
    let Some(after_scheme) = url.find("://").map(|i| i + 3) else {
        return url.to_string();
    };
    let (scheme, rest) = url.split_at(after_scheme);
    // Userinfo only counts if the `@` is in the authority (before the path).
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].find('@') {
        Some(at) => format!("{scheme}{}", &rest[at + 1..]),
        None => url.to_string(),
    }
}

/// reqwest client with an overall timeout so a hung oxy doesn't wedge the
/// CLI forever. Bundle uploads get a longer budget than the small GETs.
fn http_client(timeout_secs: u64) -> Result<reqwest::Client, OxyError> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| OxyError::RuntimeError(format!("http client init: {e}")))
}

/// gzip-tar `dir`'s contents (files at the archive root).
fn tar_gz_dir(dir: &Path) -> Result<Vec<u8>, OxyError> {
    if !dir.is_dir() {
        return Err(OxyError::ConfigurationError(format!(
            "bundle dir {} does not exist (did the build run?)",
            dir.display()
        )));
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = tar::Builder::new(&mut encoder);
        builder
            .append_dir_all("", dir)
            .map_err(|e| OxyError::RuntimeError(format!("tar {}: {e}", dir.display())))?;
        builder
            .finish()
            .map_err(|e| OxyError::RuntimeError(format!("tar finish: {e}")))?;
    }
    encoder
        .finish()
        .map_err(|e| OxyError::RuntimeError(format!("gzip finish: {e}")))
}

/// Run one build step (`install` / `command`) in `cwd` with the serve base
/// path exported, streaming output to the user's terminal.
fn run_build_step(label: &str, cmd: &str, cwd: &Path, base_path: &str) -> Result<(), OxyError> {
    println!("{}", format!("[{label}] $ {cmd}").tertiary());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(cwd)
        .env("OXY_APP_BASE_PATH", base_path)
        .status()
        .map_err(|e| OxyError::RuntimeError(format!("failed to spawn `{cmd}`: {e}")))?;
    if !status.success() {
        return Err(OxyError::RuntimeError(format!(
            "build step `{cmd}` failed ({status})"
        )));
    }
    Ok(())
}

/// Bundle each declared Oxy Function into `<bundle_dir>/functions/<name>.js`
/// via esbuild, so the server can run it in an isolated runtime. The
/// bundled files ride along in the existing tarball — no separate upload.
///
/// esbuild is invoked through the app's own toolchain (`pnpm exec esbuild`)
/// so the author's dependencies resolve exactly as they do in `pnpm build`.
/// Run from `app_dir` (where `package.json` / `node_modules` live), not the
/// output dir.
fn bundle_functions(
    manifest: &super::app_manifest::OxyAppManifest,
    app_dir: &Path,
    bundle_dir: &Path,
) -> Result<(), OxyError> {
    // The reserved-path check for `functions/` runs in the caller, which sees
    // manifest-free publishes too — see the comment at that call site.
    let Some(functions) = manifest.functions.as_ref().filter(|f| !f.is_empty()) else {
        return Ok(());
    };
    let out_fns = bundle_dir.join("functions");
    std::fs::create_dir_all(&out_fns)
        .map_err(|e| OxyError::RuntimeError(format!("mkdir {}: {e}", out_fns.display())))?;

    for (name, spec) in functions {
        // The manifest key is untrusted; reject anything outside
        // `^[a-z][a-z0-9-]{0,63}$` before it reaches `out_fns.join("<name>.js")`
        // — a key like "../../x" would make esbuild write outside the bundle.
        if !is_valid_function_name(name) {
            return Err(OxyError::RuntimeError(format!(
                "invalid function name {name:?}: must match ^[a-z][a-z0-9-]{{0,63}}$"
            )));
        }
        let entry = spec.entry_for(name);
        let outfile = out_fns.join(format!("{name}.js"));
        // `--platform=neutral` keeps the bundle host-agnostic (the runtime
        // provides `ctx`/`fetch` ops, not Node built-ins); `--format=esm`
        // matches how the runtime loads the module.
        //
        // Passed as argv (not a shell string) so an `entry` path containing
        // spaces or shell metacharacters can't break or inject into the
        // command line.
        // esbuild requires `--outfile=<path>` (a single `=`-joined token);
        // a space-separated `--outfile <path>` is rejected as an invalid flag.
        let outfile_flag = format!("--outfile={}", outfile.display());
        let args = [
            "exec",
            "esbuild",
            entry.as_str(),
            "--bundle",
            "--format=esm",
            "--platform=neutral",
            // Inline source map so a runtime stack trace (surfaced to the app on
            // error) points at the author's original `.ts` line, not the bundled
            // output. deno_core remaps stacks from the inline `//# sourceMappingURL`.
            "--sourcemap=inline",
            // …but WITHOUT `sourcesContent`. By default esbuild embeds every
            // original source file verbatim in the map, so each shipped
            // `functions/<name>.js` carried the author's full TypeScript — for
            // the entry and everything it bundles — base64'd into an artifact
            // that lives in the build store. Stack remapping only needs the
            // `mappings` segment, which this keeps: traces still resolve to
            // `<file>:<line>:<col>`. What's lost is the ability to print the
            // offending source line inline, which nothing does today.
            "--sources-content=false",
            outfile_flag.as_str(),
        ];
        let label = format!("fn:{name}");
        println!(
            "{}",
            format!("[{label}] $ pnpm {}", args.join(" ")).tertiary()
        );
        let status = std::process::Command::new("pnpm")
            .args(args)
            .current_dir(app_dir)
            .status()
            .map_err(|e| {
                OxyError::RuntimeError(format!("failed to spawn `pnpm exec esbuild`: {e}"))
            })?;
        if !status.success() {
            return Err(OxyError::RuntimeError(format!(
                "esbuild for function `{name}` failed ({status})"
            )));
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct BuildConfigResp {
    project_id: String,
}

#[derive(Debug, Deserialize)]
struct OrgForProjectResp {
    org_slug: String,
}

/// Subset of the server's `PublishResult` we render in the CLI.
/// Tolerant of extra fields so server-side additions don't break a
/// pinned CLI. `org_slug` (added 2026-06) lets us render the canonical
/// org name in the success headline even when the publisher passed a
/// UUID via `--org`; older servers that don't emit it fall back to the
/// raw `--org` input via `unwrap_or`.
#[derive(Debug, Deserialize)]
struct PublishResp {
    app_id: String,
    build_id: String,
    url: String,
    channel: String,
    #[serde(default)]
    org_slug: Option<String>,
    #[serde(default)]
    is_new_app: bool,
}

/// Resolve the project id from the target oxy using the app's identity.
/// This is what removes OXY_PROJECT from CI and keeps dev/prod correct.
async fn fetch_project(target: &str, org: &str, app: &str) -> Result<String, OxyError> {
    let url = format!("{target}/api/apps/{org}/{app}/build-config");
    let resp = http_client(30)?
        .get(&url)
        .send()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("GET {url}: {e}")))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(OxyError::ConfigurationError(format!(
            "app {org}/{app} is not registered on {target}. Register it in the oxy admin UI (or pass --project <uuid>) first."
        )));
    }
    if !resp.status().is_success() {
        return Err(OxyError::RuntimeError(format!(
            "build-config lookup failed ({}) at {url}",
            resp.status()
        )));
    }
    let cfg: BuildConfigResp = resp
        .json()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("parse build-config: {e}")))?;
    Ok(cfg.project_id)
}

/// Resolve the org slug for a pinned workspace (`--project` / `OXY_PROJECT`).
/// A workspace belongs to exactly one org, so this lets a from-source publish
/// bake the `/customer-apps/<org>/<app>/` base path without a hardcoded
/// `orgSlug` — the pinned project determines the org.
async fn fetch_org_for_project(target: &str, project_id: &str) -> Result<String, OxyError> {
    let url = format!("{target}/api/org-for-project/{project_id}");
    let resp = http_client(30)?
        .get(&url)
        .send()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("GET {url}: {e}")))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(OxyError::ConfigurationError(format!(
            "workspace {project_id} not found on {target} — check --project / OXY_PROJECT."
        )));
    }
    if !resp.status().is_success() {
        return Err(OxyError::RuntimeError(format!(
            "org-for-project lookup failed ({}) at {url}",
            resp.status()
        )));
    }
    let cfg: OrgForProjectResp = resp
        .json()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("parse org-for-project: {e}")))?;
    Ok(cfg.org_slug)
}

pub async fn handle_publish_command(args: PublishArgs) -> Result<(), OxyError> {
    // Auto-load .env.local then .env so a laptop mirrors any shell exports.
    dotenv::from_filename(".env.local").ok();
    dotenv::dotenv().ok();

    let cwd = std::env::current_dir()
        .map_err(|e| OxyError::RuntimeError(format!("cannot read cwd: {e}")))?;
    // A broken oxy-app.json stops the publish. Treating it as absent used to
    // ship the bundle with no functions and an identity from flags or the cwd.
    let manifest = OxyAppManifest::load_from_dir(&cwd)?;
    let inferred = infer_org_app_from_cwd();

    // Identity: flag → env → manifest → cwd path.
    // Org identity — flag → env → manifest → cwd path. Optional: when a
    // workspace is pinned via `--project` and the bundle is pre-built
    // (`--dir`), the server infers the org from the workspace, so a bare
    // `oxy publish --project <ws>` works. Still required to build from source
    // (the app's base path) or to look up the project id.
    let org: Option<String> = args
        .org
        .clone()
        .or_else(|| env_var("OXY_ORG"))
        .or_else(|| manifest.as_ref().and_then(|m| m.org_slug.clone()))
        .or_else(|| inferred.as_ref().map(|(o, _)| o.clone()));
    let app = args
        .app
        .clone()
        .or_else(|| env_var("OXY_APP"))
        .or_else(|| manifest.as_ref().and_then(|m| m.slug.clone()))
        .or_else(|| inferred.as_ref().map(|(_, a)| a.clone()))
        .ok_or_else(|| {
            OxyError::ConfigurationError(
                "missing app: set oxy-app.json slug, --app, or OXY_APP".into(),
            )
        })?;

    // Validate the slug BEFORE building the tarball — the same shape the server's
    // /publish enforces (`is_valid_slug`, re-exported for this). Without it an
    // author whose CI doesn't run the Vite plugin (or publishes a pre-built
    // `dist/`) uploads the whole bundle and only then learns the server's 422.
    // The slug becomes the app's OLTP schema/role name, a repo_path segment and
    // the served base path — the same reason `is_valid_function_name` above gates
    // the sibling field before esbuild.
    if !crate::server::api::admin::apps::is_valid_slug(&app) {
        return Err(OxyError::ConfigurationError(format!(
            "invalid app slug {app:?}: must be 1-63 lowercase letters, digits and single \
             hyphens (no leading/trailing/double hyphen, no underscore)"
        )));
    }

    // Target oxy: --target → manifest environments → built-in default.
    let target = resolve_target(manifest.as_ref(), Some(&args.env), args.target.as_deref())
        .ok_or_else(|| {
            OxyError::ConfigurationError(format!(
                "could not resolve a target for --env {}. Pass --target <url> or add it to oxy-app.json environments.",
                args.env
            ))
        })?;

    // Auth: token env (name configurable) → `oxy login` cache for this host.
    let token = env_var(&args.token_env)
        .or_else(|| login::load_token(&target))
        .ok_or_else(|| {
            OxyError::ConfigurationError(format!(
                "not authenticated for {target}. Run `oxy login --env {}` (or set {}).",
                args.env, args.token_env
            ))
        })?;

    // If no org was resolved but a workspace is pinned (--project / OXY_PROJECT),
    // infer the org from it — a workspace belongs to exactly one org. This lets
    // `oxy publish --project <uuid>` build from source without a hardcoded
    // orgSlug: the base path /customer-apps/<org>/<app>/ (baked below) needs it.
    let project_pin = args.project.clone().or_else(|| env_var("OXY_PROJECT"));
    let org = match org {
        Some(o) => Some(o),
        None => match &project_pin {
            Some(pid) => Some(fetch_org_for_project(&target, pid).await?),
            None => None,
        },
    };

    // Bundle: build from the manifest, or take a pre-built --dir.
    let bundle_dir = match &args.dir {
        Some(d) => d.clone(),
        None => {
            let m = manifest.as_ref();
            // Building from source bakes the app's public base path into the
            // bundle, so the org must be known here (unlike a pre-built --dir).
            let org = org.as_deref().ok_or_else(|| {
                OxyError::ConfigurationError(
                    "missing org: needed to build the app's base path — set oxy-app.json orgSlug, --org, OXY_ORG, or --project/OXY_PROJECT (a pinned workspace determines its org), or publish a pre-built bundle with --dir".into(),
                )
            })?;
            let base_path = format!("/customer-apps/{org}/{app}/");
            let install = m
                .map(|m| m.build_install())
                .unwrap_or_else(|| "pnpm install".into());
            let command = m
                .map(|m| m.build_command())
                .unwrap_or_else(|| "pnpm build".into());
            let out_dir = m.map(|m| m.build_out_dir()).unwrap_or_else(|| "out".into());
            run_build_step("install", &install, &cwd, &base_path)?;
            run_build_step("build", &command, &cwd, &base_path)?;
            cwd.join(out_dir)
        }
    };

    // `functions/` is reserved in a published bundle, so the check belongs
    // here at the top level rather than inside `bundle_functions` — that runs
    // only when there IS a manifest, and a manifest-free publish
    // (`oxy publish --org acme --app site --dir out`) is a supported shape
    // which by construction declares zero functions. That is exactly the
    // population the serve guard blocks and the check exists to cover, so
    // gating it on the manifest reintroduced the hole it was widened to close.
    let declared = manifest
        .as_ref()
        .and_then(|m| m.functions.as_ref())
        .filter(|f| !f.is_empty());
    enforce_reserved_functions_dir(
        &bundle_dir,
        declared.into_iter().flat_map(|f| f.keys()),
        declared.is_some(),
    )?;

    // Bundle any declared Oxy Functions into <bundle_dir>/functions/<name>.js.
    // No-op when the manifest declares none (today's static-bundle default).
    if let Some(m) = manifest.as_ref() {
        bundle_functions(m, &cwd, &bundle_dir)?;
    }

    // Project: pinned (--project / OXY_PROJECT) → else build-config on the target.
    let project = match project_pin {
        Some(p) => p,
        // Looking up the project by (org, app) needs the org. Pass --project
        // to skip this — then the org can be inferred server-side.
        None => {
            let org = org.as_deref().ok_or_else(|| {
                OxyError::ConfigurationError(
                    "missing org: needed to look up the project — set oxy-app.json orgSlug, --org, or OXY_ORG (or pass --project <uuid>)".into(),
                )
            })?;
            fetch_project(&target, org, &app).await?
        }
    };

    let build_id = args
        .build_id
        .clone()
        .or_else(ci_build_id)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());

    // Best-effort git provenance from the app's working tree, falling back to
    // CI env vars. Recorded per-build so the admin UI can link each build back
    // to its source commit — never fails the publish when git is absent.
    let (git_repo, git_commit, git_branch) = capture_git_source(&cwd);
    // Sanitize whatever we send (auto-captured or --repo): never persist a
    // credentialed remote (`https://user:token@…`) into the DB / builds API.
    let source_repo = args
        .repo
        .clone()
        .or(git_repo)
        .map(|u| sanitize_remote_url(&u));
    let commit_sha = args
        .commit
        .clone()
        .or(git_commit)
        .or_else(|| env_var("GITHUB_SHA"));
    let branch = args
        .branch
        .clone()
        .or(git_branch)
        .or_else(|| env_var("GITHUB_REF_NAME"));

    // Warn AFTER the `--repo` / `--commit` / CI-env fallbacks have been
    // applied: someone passing them explicitly has answered the question, and
    // warning off the raw git capture would nag them on every CI run.
    warn_on_weak_provenance(&cwd, commit_sha.as_deref(), source_repo.as_deref());

    let tarball = tar_gz_dir(&bundle_dir)?;
    let channel = if args.promote { "published" } else { "draft" };
    let who = match &org {
        Some(o) => format!("{o}/{app}"),
        None => format!("{app} → workspace {project}"),
    };
    println!(
        "{}",
        format!(
            "Publishing {who} ({} bytes) → {target} [{channel}]",
            tarball.len()
        )
        .text()
    );

    let mut form = reqwest::multipart::Form::new()
        .text("app", app.clone())
        .text("project", project)
        .text("build_id", build_id)
        .text("channel", channel.to_string())
        .part(
            "bundle",
            reqwest::multipart::Part::bytes(tarball).file_name("bundle.tar.gz"),
        );
    // Org is optional: when omitted (a pre-built `--dir` pinned to `--project`),
    // the server infers it from the workspace.
    if let Some(org) = &org {
        form = form.text("org", org.clone());
    }
    if let Some(name) = &args.name {
        form = form.text("name", name.clone());
    }
    if let Some(v) = &source_repo {
        form = form.text("source_repo", v.clone());
    }
    if let Some(v) = &commit_sha {
        form = form.text("commit_sha", v.clone());
    }
    if let Some(v) = &branch {
        form = form.text("branch", v.clone());
    }

    let url = format!("{target}/api/customer-apps/publish");
    let resp = http_client(120)?
        .post(&url)
        .bearer_auth(&token)
        .multipart(form)
        .send()
        .await
        .map_err(|e| OxyError::RuntimeError(format!("publish request to {url} failed: {e}")))?;

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::UNAUTHORIZED {
        return Err(OxyError::RuntimeError(format!(
            "publish rejected ({status}): {body}\nAre you an app-admin? Run `oxy login --env {}` to check.",
            args.env
        )));
    }
    if !status.is_success() {
        return Err(OxyError::RuntimeError(format!(
            "publish failed ({status}): {body}"
        )));
    }
    // Successful response → render a human-readable summary that calls
    // out whether this was a first publish (new row in `apps`) or a new
    // version of an existing app. Fall back to the raw body if the
    // server's shape ever drifts so we don't swallow a useful response
    // on a pinned CLI.
    match serde_json::from_str::<PublishResp>(&body) {
        Ok(r) => {
            // Prefer the server's canonical org slug so a UUID passed
            // via --org doesn't echo back into a jarring
            // "Registered new app 550e8400-…/store-pulse" headline.
            // The server always echoes the canonical org slug (it resolved or
            // inferred the org); fall back to the CLI's org only if that's ever
            // absent, then to empty when org was inferred and not sent.
            let display_org = r.org_slug.as_deref().or(org.as_deref()).unwrap_or("");
            let headline = if r.is_new_app {
                format!("Registered new app {display_org}/{app} (id {})", r.app_id).success()
            } else {
                format!(
                    "Published new version of {display_org}/{app} (id {})",
                    r.app_id
                )
                .success()
            };
            println!("{headline}");
            println!(
                "{}",
                format!(
                    "  build {} → {} channel · {}{}",
                    r.build_id, r.channel, target, r.url
                )
                .tertiary()
            );
            if r.is_new_app {
                println!(
                    "{}",
                    "  Tip: future `oxy publish` runs for this app will say \"new version\" instead of \"registered\"."
                        .tertiary()
                );
            }
        }
        Err(_) => println!("{}", format!("Published: {body}").success()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_gz_dir_errors_on_missing_dir() {
        let res = tar_gz_dir(Path::new("/nonexistent/bundle/dir/xyz"));
        assert!(res.is_err());
    }

    #[test]
    fn sanitize_remote_url_strips_embedded_credentials() {
        assert_eq!(
            sanitize_remote_url("https://x-access-token:ghs_SECRET@github.com/oxy-hq/app.git"),
            "https://github.com/oxy-hq/app.git"
        );
        assert_eq!(
            sanitize_remote_url("https://user:pass@gitlab.com/o/r"),
            "https://gitlab.com/o/r"
        );
    }

    #[test]
    fn sanitize_remote_url_leaves_clean_and_ssh_urls_untouched() {
        assert_eq!(
            sanitize_remote_url("https://github.com/oxy-hq/app.git"),
            "https://github.com/oxy-hq/app.git"
        );
        // scp-like SSH form: the `git@` user is fixed, not a secret.
        assert_eq!(
            sanitize_remote_url("git@github.com:oxy-hq/app.git"),
            "git@github.com:oxy-hq/app.git"
        );
        // A `@` in the path (no scheme userinfo) must not be stripped.
        assert_eq!(
            sanitize_remote_url("https://github.com/o/r@weird"),
            "https://github.com/o/r@weird"
        );
    }

    #[test]
    fn provenance_gap_flags_a_build_with_no_source_at_all() {
        assert_eq!(provenance_gap(None, None, None), [ProvenanceGap::NoSource]);
        // Nothing recorded means no sha for a dirty tree to contradict, so
        // `DirtyTree` genuinely doesn't apply here (unlike `MissingRepo`).
        assert_eq!(
            provenance_gap(Some(true), None, None),
            [ProvenanceGap::NoSource]
        );
    }

    #[test]
    fn provenance_gap_flags_a_dirty_tree_behind_a_recorded_commit() {
        assert_eq!(
            provenance_gap(Some(true), Some("abc123"), Some("git@github.com:o/r.git")),
            [ProvenanceGap::DirtyTree]
        );
    }

    /// Half-recorded provenance must warn HERE, not just in the admin list.
    /// This is the case that used to publish silently and then sit amber in
    /// the UI forever, because the CLI required both halves to be missing
    /// while the server flagged either.
    #[test]
    fn provenance_gap_flags_a_half_recorded_source() {
        // The CI shape: `--dir` publish, `$GITHUB_SHA` fills the commit, no
        // `origin` remote to read.
        assert_eq!(
            provenance_gap(None, Some("abc123"), None),
            [ProvenanceGap::MissingRepo]
        );
        // `--repo` without `--commit`: the link points at a branch, which moves.
        assert_eq!(
            provenance_gap(Some(false), None, Some("git@github.com:o/r.git")),
            [ProvenanceGap::MissingCommit]
        );
        // Blank is not provenance — `--repo ""` must not read as recorded.
        assert_eq!(
            provenance_gap(Some(false), Some("abc123"), Some("  ")),
            [ProvenanceGap::MissingRepo]
        );
    }

    /// Incompleteness and dirtiness are orthogonal, so both must be reported.
    /// Returning only the "worst" gap dropped `DirtyTree` for exactly the CI
    /// shape this warning was added for: sha from `$GITHUB_SHA`, no `origin`,
    /// built from an edited tree.
    #[test]
    fn provenance_gap_reports_a_missing_repo_and_a_dirty_tree_together() {
        assert_eq!(
            provenance_gap(Some(true), Some("abc123"), None),
            [ProvenanceGap::MissingRepo, ProvenanceGap::DirtyTree]
        );
        // A missing COMMIT is the one case where dirtiness has nothing to
        // contradict — there is no recorded sha making a claim.
        assert_eq!(
            provenance_gap(Some(true), None, Some("git@github.com:o/r.git")),
            [ProvenanceGap::MissingCommit]
        );
        // …and a blank commit is not a claim either.
        assert_eq!(
            provenance_gap(Some(true), Some("   "), Some("git@github.com:o/r.git")),
            [ProvenanceGap::MissingCommit]
        );
    }

    #[test]
    fn provenance_gap_is_silent_on_a_clean_traceable_publish() {
        assert!(
            provenance_gap(Some(false), Some("abc123"), Some("git@github.com:o/r.git")).is_empty()
        );
        // git couldn't answer (a `--dir` publish from a non-checkout): treat
        // as clean rather than nagging every CI run.
        assert!(provenance_gap(None, Some("abc123"), Some("git@github.com:o/r.git")).is_empty());
    }

    /// Every gap has to say something specific enough to act on — an operator
    /// who is missing `--commit` should not be told to "publish from a git
    /// checkout" when they already are.
    #[test]
    fn every_gap_has_a_distinct_actionable_message() {
        let msgs = [
            ProvenanceGap::NoSource.message(),
            ProvenanceGap::MissingRepo.message(),
            ProvenanceGap::MissingCommit.message(),
            ProvenanceGap::DirtyTree.message(),
        ];
        for (i, a) in msgs.iter().enumerate() {
            assert!(a.starts_with("warning: "), "{a:?} should be a warning");
            for b in &msgs[i + 1..] {
                assert_ne!(a, b, "two gaps share a message");
            }
        }
    }

    /// `functions/` is reserved in a published bundle, so a frontend file
    /// landing there is a collision — once uploaded it would be permanently
    /// unreachable behind the serve-plane guard.
    #[test]
    fn conflicts_detect_frontend_files_but_tolerate_our_own_artifacts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("functions");
        std::fs::create_dir_all(&dir).unwrap();
        let declared = ["top-stores".to_string(), "daily-rollup".to_string()];

        // A directory holding only our artifacts is a re-publish into an
        // un-cleaned build dir — normal, must not fail.
        std::fs::write(dir.join("top-stores.js"), b"x").unwrap();
        std::fs::write(dir.join("daily-rollup.js"), b"x").unwrap();
        assert!(
            functions_dir_conflicts(&dir, declared.iter())
                .unwrap()
                .is_empty(),
            "our own leftovers must be overwritable"
        );

        // Frontend files in the same directory are the collision — reported
        // in full and sorted, because `read_dir` order is unspecified and an
        // author shouldn't fix them one publish at a time.
        std::fs::write(dir.join("index.html"), b"<html>").unwrap();
        std::fs::write(dir.join("about.html"), b"<html>").unwrap();
        let conflicts = functions_dir_conflicts(&dir, declared.iter()).unwrap();
        assert_eq!(
            conflicts.frontend,
            vec!["about.html".to_string(), "index.html".to_string()]
        );
        assert!(conflicts.stale_artifacts.is_empty());
    }

    /// A `.js` for a function no longer in the manifest is Oxy's own output
    /// from a previous publish, not the frontend colliding with a reserved
    /// path — telling the author to "emit your frontend somewhere else" about
    /// a file Oxy wrote is the wrong diagnosis even when the advice lands.
    #[test]
    fn conflicts_separate_a_removed_functions_leftover_from_a_frontend_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("functions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("top-stores.js"), b"x").unwrap();
        std::fs::write(dir.join("removed-fn.js"), b"x").unwrap();
        std::fs::write(dir.join("index.html"), b"<html>").unwrap();

        let declared = ["top-stores".to_string()];
        let conflicts = functions_dir_conflicts(&dir, declared.iter()).unwrap();
        assert_eq!(conflicts.stale_artifacts, vec!["removed-fn.js".to_string()]);
        assert_eq!(conflicts.frontend, vec!["index.html".to_string()]);
    }

    /// The leftover bucket is matched on the grammar that *produced* the file,
    /// not on the `.js` suffix, so a bundler chunk is described as frontend
    /// output and gets "emit it somewhere else" rather than being blamed on a
    /// previous publish.
    ///
    /// Wording only — `enforce_reserved_functions_dir` refuses either bucket,
    /// so nothing here decides whether a publish proceeds.
    #[test]
    fn conflicts_treat_bundler_chunks_as_frontend_not_leftovers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("functions");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "chunk-a1b2.d4f1.js", // dots
            "main.min.js",        // dots
            "_app.js",            // leading underscore
            "Chunk.js",           // uppercase
            "2fast.js",           // leading digit
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }
        // …while a plausible artifact name still reads as ours.
        std::fs::write(dir.join("removed-fn.js"), b"x").unwrap();

        let conflicts = functions_dir_conflicts(&dir, std::iter::empty()).unwrap();
        assert_eq!(conflicts.stale_artifacts, vec!["removed-fn.js".to_string()]);
        assert_eq!(
            conflicts.frontend,
            vec![
                "2fast.js".to_string(),
                "Chunk.js".to_string(),
                "_app.js".to_string(),
                "chunk-a1b2.d4f1.js".to_string(),
                "main.min.js".to_string(),
            ]
        );
    }

    /// Any conflict refuses the publish, whatever the filename looks like.
    /// `removed-fn.js` reads as a leftover and an unhashed `main.js` reads the
    /// same way, so the outcome must not consult the bucket at all — only the
    /// wording does.
    #[test]
    fn reserved_dir_refuses_a_leftover_too_only_the_wording_differs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("functions")).unwrap();
        std::fs::write(tmp.path().join("functions/removed-fn.js"), b"x").unwrap();

        let declared = ["top-stores".to_string()];
        let err = enforce_reserved_functions_dir(tmp.path(), declared.iter(), true)
            .expect_err("a conflict must stop the publish whichever bucket it lands in");
        let msg = err.to_string();
        assert!(msg.contains("functions/removed-fn.js"), "got: {msg}");
        assert!(
            msg.contains("previous publish"),
            "a leftover should still get its own explanation, got: {msg}"
        );

        // The shape the guess can't tell apart: unhashed bundler output that
        // satisfies the function-name grammar. Same outcome, which is the
        // point — the guess only picks prose.
        std::fs::remove_file(tmp.path().join("functions/removed-fn.js")).unwrap();
        std::fs::write(tmp.path().join("functions/main.js"), b"x").unwrap();
        assert!(
            enforce_reserved_functions_dir(tmp.path(), declared.iter(), true).is_err(),
            "an unhashed bundler entry must not be waved through as a leftover"
        );
    }

    /// One publish, every problem. A frontend collision used to return early
    /// and swallow the leftover warning behind it, so an author cleaned the
    /// pages, re-published, and only then heard about the stale artifact —
    /// one class of problem per round-trip.
    #[test]
    fn reserved_dir_reports_both_buckets_in_one_message() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("functions")).unwrap();
        std::fs::write(tmp.path().join("functions/index.html"), b"<html>").unwrap();
        std::fs::write(tmp.path().join("functions/removed-fn.js"), b"x").unwrap();

        let declared = ["top-stores".to_string()];
        let err = enforce_reserved_functions_dir(tmp.path(), declared.iter(), true)
            .expect_err("a frontend collision must refuse the publish");
        let msg = err.to_string();
        assert!(
            msg.contains("functions/index.html") && msg.contains("functions/removed-fn.js"),
            "both conflicts must appear in one message, got: {msg}"
        );
    }

    /// With nothing declared, every entry conflicts — this is the
    /// zero-Oxy-Functions app whose files the serve guard silently drops.
    /// The no-declared-functions warning caps per bucket, so a class can't be
    /// squeezed out of the message by the other class sorting ahead of it.
    /// Merging first and capping once made that alphabetical luck.
    #[test]
    fn reserved_dir_warning_names_both_classes_despite_the_cap() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("functions");
        std::fs::create_dir_all(&dir).unwrap();
        // Six leftover-shaped names all sort before `index.html`; merged and
        // capped at five, the one surprising entry would vanish into a count.
        for i in 0..6 {
            std::fs::write(dir.join(format!("aaa-fn-{i}.js")), b"x").unwrap();
        }
        std::fs::write(dir.join("index.html"), b"<html>").unwrap();

        let conflicts = functions_dir_conflicts(&dir, std::iter::empty()).unwrap();
        assert_eq!(conflicts.frontend, vec!["index.html".to_string()]);
        assert_eq!(conflicts.stale_artifacts.len(), 6);
        // Per-bucket rendering keeps the frontend entry visible.
        assert!(
            describe_entries(&conflicts.frontend).contains("index.html"),
            "the lone frontend file must still be named"
        );
        // …whereas a merged, once-capped list would have dropped it.
        let mut merged = conflicts.frontend.clone();
        merged.extend(conflicts.stale_artifacts.clone());
        merged.sort();
        assert!(
            !describe_entries(&merged).contains("index.html"),
            "guard assumption: merging first is what hid it"
        );
    }

    #[test]
    fn conflicts_flag_everything_when_no_functions_are_declared() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("functions");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), b"<html>").unwrap();
        assert_eq!(
            functions_dir_conflicts(&dir, std::iter::empty())
                .unwrap()
                .frontend,
            vec!["index.html".to_string()]
        );
    }

    #[test]
    fn conflicts_are_empty_when_the_directory_does_not_exist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let declared = ["top-stores".to_string()];
        assert!(
            functions_dir_conflicts(&tmp.path().join("functions"), declared.iter())
                .unwrap()
                .is_empty()
        );
    }

    /// A frontend collision refuses the publish only when we're about to write
    /// into the directory. An app with no functions gets a warning instead —
    /// erroring would refuse a publish that succeeds today.
    #[test]
    fn reserved_dir_errors_only_when_this_publish_writes_there() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("functions")).unwrap();
        std::fs::write(tmp.path().join("functions/index.html"), b"<html>").unwrap();

        let declared = ["top-stores".to_string()];
        let err = enforce_reserved_functions_dir(tmp.path(), declared.iter(), true)
            .expect_err("a merge must be refused");
        assert!(
            err.to_string().contains("functions/index.html"),
            "the error must name the colliding file, got: {err}"
        );

        // The manifest-free / zero-function publish: same directory, warning
        // only. This path was unreachable until the check moved out of
        // `bundle_functions`, which only runs when there IS a manifest.
        enforce_reserved_functions_dir(tmp.path(), std::iter::empty(), false)
            .expect("no functions declared: warn, don't gate");
    }

    #[test]
    fn describe_entries_bounds_a_long_list() {
        let many: Vec<String> = (0..8).map(|i| format!("p{i}.html")).collect();
        let rendered = describe_entries(&many);
        assert!(rendered.starts_with("functions/p0.html, "), "{rendered}");
        assert!(rendered.ends_with("(and 3 more)"), "{rendered}");
        assert_eq!(
            describe_entries(&["only.html".to_string()]),
            "functions/only.html"
        );
    }

    /// A workflow re-run on the same commit must not reuse a build id — the
    /// stored bytes for one are immutable and cached by that id.
    #[test]
    fn ci_build_id_is_unique_per_run_not_per_commit() {
        // SAFETY: nextest runs each test in its own process, so no other test
        // observes these vars.
        unsafe {
            std::env::set_var("GITHUB_SHA", "abc123");
            std::env::remove_var("GITHUB_RUN_ID");
            std::env::remove_var("GITHUB_RUN_ATTEMPT");
        }
        // Non-GitHub CI that sets only the sha keeps the old behavior.
        assert_eq!(ci_build_id().as_deref(), Some("abc123"));

        unsafe { std::env::set_var("GITHUB_RUN_ID", "42") }
        assert_eq!(ci_build_id().as_deref(), Some("abc123-42"));

        // The same run, re-run: a distinct id, which is the whole point.
        unsafe { std::env::set_var("GITHUB_RUN_ATTEMPT", "2") }
        assert_eq!(ci_build_id().as_deref(), Some("abc123-42.2"));

        // No sha at all → the caller falls back to a random uuid.
        unsafe { std::env::remove_var("GITHUB_SHA") }
        assert_eq!(ci_build_id(), None);
    }
}
