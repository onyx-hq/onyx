//! `oxy-app.json` parsing for the self-serve publish flow.
//!
//! The manifest is identity-first; `build` and `environments` are optional
//! and fall back to convention-over-configuration defaults (Vercel-style),
//! so the common-case manifest stays identity-only. `oxy publish` and
//! `oxy login` both read target resolution from here.

use std::collections::HashMap;
use std::path::Path;

use oxy_shared::errors::OxyError;
use serde::Deserialize;

use super::env_url;
pub use super::env_url::ResolvedEnv;

const MANIFEST_FILE: &str = "oxy-app.json";

/// Parsed `oxy-app.json`. Unknown fields are ignored (forward-compat).
#[derive(Debug, Default, Deserialize)]
pub struct OxyAppManifest {
    pub slug: Option<String>,
    #[serde(rename = "orgSlug")]
    pub org_slug: Option<String>,
    pub name: Option<String>,
    pub build: Option<BuildSpec>,
    pub environments: Option<HashMap<String, EnvSpec>>,
    /// Optional Oxy Functions shipped in the bundle's `functions/` dir,
    /// keyed by function name. See
    /// `internal-docs/customer-apps-functions.md`.
    pub functions: Option<HashMap<String, FunctionSpec>>,
}

/// Per-function manifest entry. Mirrors `OxyAppFunctionManifest` in the
/// TypeScript SDK; unknown fields ignored for forward-compat.
#[derive(Debug, Default, Deserialize, Clone)]
pub struct FunctionSpec {
    pub entry: Option<String>,
    pub schedule: Option<String>,
    pub timezone: Option<String>,
    pub route: Option<bool>,
    #[serde(rename = "airwayStep")]
    pub airway_step: Option<AirwayStepSpec>,
    #[serde(rename = "timeoutSeconds")]
    pub timeout_seconds: Option<u32>,
}

#[derive(Debug, Default, Deserialize, Clone)]
pub struct AirwayStepSpec {
    pub pipeline: String,
    pub resource: String,
}

impl FunctionSpec {
    /// Source entry path relative to the app dir. Default
    /// `functions/<name>.ts`.
    pub fn entry_for(&self, name: &str) -> String {
        self.entry
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("functions/{name}.ts"))
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct BuildSpec {
    pub install: Option<String>,
    pub command: Option<String>,
    #[serde(rename = "outDir")]
    pub out_dir: Option<String>,
}

/// One `environments.<name>` entry.
///
/// `target` is optional: an entry without it (or with a blank one) resolves as
/// if the entry were absent. It used to be required, and because a parse
/// failure made the whole manifest read as missing, an entry carrying only
/// other keys made `oxy publish` bundle no functions and fall back to flag/cwd
/// identity — silently.
#[derive(Debug, Deserialize)]
pub struct EnvSpec {
    pub target: Option<String>,
}

impl OxyAppManifest {
    /// Load `<dir>/oxy-app.json`. `Ok(None)` when there is no such file —
    /// callers treat absence as "fall back to flags + defaults".
    ///
    /// A file that exists but can't be read or parsed is an **error** naming
    /// the file, never `None`: every caller would otherwise carry on with the
    /// wrong identity, target and function set.
    pub fn load_from_dir(dir: &Path) -> Result<Option<Self>, OxyError> {
        let path = dir.join(MANIFEST_FILE);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(OxyError::ConfigurationError(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        serde_json::from_str(&raw).map(Some).map_err(|e| {
            OxyError::ConfigurationError(format!(
                "{} is not a valid oxy-app.json: {e}. Fix the file and re-run.",
                path.display()
            ))
        })
    }

    /// Install command, default `pnpm install`.
    pub fn build_install(&self) -> String {
        self.build
            .as_ref()
            .and_then(|b| b.install.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "pnpm install".to_string())
    }

    /// Build command, default `pnpm build`.
    pub fn build_command(&self) -> String {
        self.build
            .as_ref()
            .and_then(|b| b.command.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "pnpm build".to_string())
    }

    /// Output directory, default `out` (matches the vite-plugin's forced
    /// `outDir`).
    pub fn build_out_dir(&self) -> String {
        self.build
            .as_ref()
            .and_then(|b| b.out_dir.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "out".to_string())
    }
}

/// Built-in target for a well-known environment name. Used when the manifest
/// has no `environments.<env>` entry — keeps the common case zero-config.
/// Overridden by `environments` in the manifest and by `--target`.
pub fn default_target(env: &str) -> Option<&'static str> {
    match env {
        // The web-app Vite dev server (port 5173), NOT oxy's own port (3000):
        // `oxy login` opens `<target>/cli-auth`, a route that only exists in
        // the live web-app — and oxy serves a *pre-built embedded* bundle that
        // may predate it. Vite serves the current route and proxies `/api/*`
        // to oxy on :3000, so build-config / publish / whoami flow through too.
        "local" => Some("http://localhost:5173"),
        "dev" | "development" => Some("https://aip.dev.oxy.tech"),
        "staging" => Some("https://aip.staging.oxy.tech"),
        "production" | "prod" => Some("https://app.oxygen-hq.com"),
        _ => None,
    }
}

/// Resolve the oxy URL to publish/authenticate against. Precedence:
/// `--target` flag → manifest `environments.<env>.target` → built-in
/// default for `<env>`. Returns `None` if nothing resolves.
pub fn resolve_target(
    manifest: Option<&OxyAppManifest>,
    env: Option<&str>,
    target_flag: Option<&str>,
) -> Option<String> {
    resolve_env(manifest, env, target_flag).map(|r| r.target)
}

/// [`resolve_target`] plus the org slug the value carried, if any.
///
/// Precedence is unchanged — `--target` → manifest `environments.<env>` →
/// built-in default for `<env>` — with one purely **additive** step: an `--env`
/// that no name resolves is tried as a URL, so you can paste the address bar
/// (`--env https://poke-house.oxygen-hq.com`) instead of memorising env names.
/// Every named value keeps working exactly as before, and a name always wins
/// over the URL reading.
///
/// `--target` stays verbatim (it is the explicit escape hatch, including for
/// deployments served under a path); its org slug is still mined so
/// `--target https://<org>.oxygen-hq.com` knows which org it is pointing at.
pub fn resolve_env(
    manifest: Option<&OxyAppManifest>,
    env: Option<&str>,
    target_flag: Option<&str>,
) -> Option<ResolvedEnv> {
    if let Some(t) = target_flag.filter(|s| !s.trim().is_empty()) {
        let org_slug = env_url::parse_env_url(t).and_then(|r| r.org_slug);
        return Some(ResolvedEnv::new(t.trim(), org_slug));
    }
    let env = env?;
    // An entry without a usable `target` falls through, exactly as if absent.
    if let Some(target) = manifest
        .and_then(|m| m.environments.as_ref())
        .and_then(|envs| envs.get(env))
        .and_then(|spec| spec.target.as_deref())
        .filter(|t| !t.trim().is_empty())
    {
        return Some(ResolvedEnv::new(target, None));
    }
    if let Some(t) = default_target(env) {
        return Some(ResolvedEnv::new(t, None));
    }
    // Not a known name: read it as a URL. This is the only new branch, and it
    // runs only where the old code returned `None`.
    env_url::looks_like_url(env)
        .then(|| env_url::parse_env_url(env))
        .flatten()
}

/// Load `<dir>/oxy-app.json` for a command that reads it only to resolve the
/// target (`oxy login`, `logout`, `proxy`, `assume`).
///
/// A non-blank `--target` wins outright in [`resolve_env`], so the manifest
/// cannot change the outcome: this returns `Ok(None)` without reading the file,
/// and a broken `oxy-app.json` no longer fails a command that was told where to
/// go. Otherwise it is [`OxyAppManifest::load_from_dir`], strict as ever.
///
/// `oxy publish` calls `load_from_dir` directly: it needs the manifest's
/// identity and functions whatever `--target` says.
pub fn load_for_target_resolution(
    dir: &Path,
    target_flag: Option<&str>,
) -> Result<Option<OxyAppManifest>, OxyError> {
    if target_flag.is_some_and(|t| !t.trim().is_empty()) {
        return Ok(None);
    }
    OxyAppManifest::load_from_dir(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_defaults_apply_when_absent() {
        let m = OxyAppManifest::default();
        assert_eq!(m.build_install(), "pnpm install");
        assert_eq!(m.build_command(), "pnpm build");
        assert_eq!(m.build_out_dir(), "out");
    }

    #[test]
    fn build_overrides_win() {
        let m: OxyAppManifest = serde_json::from_value(serde_json::json!({
            "slug": "x",
            "build": { "install": "bun install", "command": "bun run build", "outDir": "dist" }
        }))
        .unwrap();
        assert_eq!(m.build_install(), "bun install");
        assert_eq!(m.build_command(), "bun run build");
        assert_eq!(m.build_out_dir(), "dist");
    }

    #[test]
    fn target_precedence_flag_over_manifest_over_default() {
        let m: OxyAppManifest = serde_json::from_value(serde_json::json!({
            "slug": "x",
            "environments": { "dev": { "target": "https://custom.example.com/" } }
        }))
        .unwrap();
        // flag wins
        assert_eq!(
            resolve_target(Some(&m), Some("dev"), Some("https://flag.example.com")).as_deref(),
            Some("https://flag.example.com")
        );
        // manifest wins over built-in default (and trailing slash trimmed)
        assert_eq!(
            resolve_target(Some(&m), Some("dev"), None).as_deref(),
            Some("https://custom.example.com")
        );
        // built-in default when manifest has no entry
        assert_eq!(
            resolve_target(Some(&m), Some("local"), None).as_deref(),
            Some("http://localhost:5173")
        );
        // unknown env, no manifest, no flag → None
        assert_eq!(resolve_target(None, Some("bogus"), None), None);
    }

    #[test]
    fn env_accepts_a_url_and_keeps_every_name_working() {
        // Additive: a URL resolves where a name used to return None…
        let r = resolve_env(None, Some("https://app.oxygen-hq.com/threads/x"), None).unwrap();
        assert_eq!(r.target, "https://app.oxygen-hq.com");
        assert_eq!(r.org_slug, None);
        // …an org URL yields both the product target and the org slug…
        let r = resolve_env(None, Some("https://poke-house.oxygen-hq.com"), None).unwrap();
        assert_eq!(r.target, "https://app.oxygen-hq.com");
        assert_eq!(r.org_slug.as_deref(), Some("poke-house"));
        // …and the named envs are untouched.
        assert_eq!(
            resolve_target(None, Some("production"), None).as_deref(),
            Some("https://app.oxygen-hq.com")
        );
    }

    #[test]
    fn a_manifest_env_name_still_wins_over_url_parsing() {
        // A manifest key that happens to look like a URL must resolve from the
        // manifest, not by parsing — names always win.
        let m: OxyAppManifest = serde_json::from_value(serde_json::json!({
            "slug": "x",
            "environments": { "app.oxygen-hq.com": { "target": "https://pinned.example.com" } }
        }))
        .unwrap();
        assert_eq!(
            resolve_target(Some(&m), Some("app.oxygen-hq.com"), None).as_deref(),
            Some("https://pinned.example.com")
        );
    }

    #[test]
    fn target_flag_stays_verbatim_but_reports_its_org() {
        // Verbatim: the path is preserved (a deployment served under a path is
        // exactly why `--target` exists).
        let r = resolve_env(None, Some("production"), Some("https://host.example/oxy")).unwrap();
        assert_eq!(r.target, "https://host.example/oxy");
        let r = resolve_env(None, None, Some("https://poke-house.oxygen-hq.com")).unwrap();
        assert_eq!(r.target, "https://poke-house.oxygen-hq.com");
        assert_eq!(r.org_slug.as_deref(), Some("poke-house"));
    }

    #[test]
    fn an_environment_entry_without_target_falls_back_as_if_absent() {
        // Newer manifests put other per-environment keys under `environments`
        // (the environments design adds staging/dev slots). An entry with no
        // `target` must not fail the whole parse: that made the CLI treat the
        // manifest as missing and publish with no functions bundled.
        let m: OxyAppManifest = serde_json::from_value(serde_json::json!({
            "slug": "x",
            "functions": { "top-stores": {} },
            "environments": {
                "dev": {},
                "staging": { "someFutureKey": true },
                "production": { "target": "   " },
                "https://poke-house.oxygen-hq.com": {}
            }
        }))
        .expect("an environments entry without `target` still parses");
        assert!(
            m.functions
                .as_ref()
                .is_some_and(|f| f.contains_key("top-stores"))
        );
        for env in ["dev", "staging", "production"] {
            assert_eq!(
                resolve_target(Some(&m), Some(env), None).as_deref(),
                default_target(env),
                "{env} falls back to the built-in default"
            );
        }
        // …and a URL-shaped name falls through to URL parsing, exactly as if
        // the entry were not there.
        let r = resolve_env(Some(&m), Some("https://poke-house.oxygen-hq.com"), None).unwrap();
        assert_eq!(r.target, "https://app.oxygen-hq.com");
        assert_eq!(r.org_slug.as_deref(), Some("poke-house"));
    }

    #[test]
    fn a_missing_manifest_is_none_but_an_unparseable_one_is_an_error() {
        let dir = tempfile::tempdir().expect("tmpdir");
        assert!(
            OxyAppManifest::load_from_dir(dir.path())
                .expect("absence is not an error")
                .is_none()
        );

        for bad in [r#"{ "slug": "x", "#, r#"{ "slug": 5 }"#] {
            std::fs::write(dir.path().join(MANIFEST_FILE), bad).unwrap();
            let err = OxyAppManifest::load_from_dir(dir.path())
                .expect_err("an unparseable oxy-app.json must be an error, not None")
                .to_string();
            assert!(err.contains("oxy-app.json"), "names the file: {err}");
            assert!(
                err.contains(&dir.path().display().to_string()),
                "names the path: {err}"
            );
        }
    }

    #[test]
    fn a_target_flag_skips_a_broken_manifest_but_a_blank_or_absent_one_does_not() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join(MANIFEST_FILE), r#"{ "slug": "x", "#).unwrap();

        // `--target` wins in `resolve_env`, so the broken file cannot change
        // the outcome and must not fail the command.
        assert!(
            load_for_target_resolution(dir.path(), Some("https://flag.example.com"))
                .expect("a target flag does not read the manifest")
                .is_none()
        );

        // Without a usable flag the manifest decides the target: still strict.
        for flag in [None, Some(""), Some("   ")] {
            let err = load_for_target_resolution(dir.path(), flag)
                .expect_err("a broken manifest is an error when it picks the target")
                .to_string();
            assert!(
                err.contains("oxy-app.json"),
                "{flag:?} names the file: {err}"
            );
        }

        // And a valid manifest still loads when no flag is passed.
        std::fs::write(dir.path().join(MANIFEST_FILE), r#"{ "slug": "x" }"#).unwrap();
        let m = load_for_target_resolution(dir.path(), None)
            .expect("a valid manifest loads")
            .expect("and is present");
        assert_eq!(m.slug.as_deref(), Some("x"));
    }

    #[test]
    fn a_bare_unknown_env_name_still_resolves_to_nothing() {
        assert_eq!(resolve_env(None, Some("bogus"), None), None);
    }

    #[test]
    fn builtin_targets_for_known_environments() {
        assert_eq!(default_target("local"), Some("http://localhost:5173"));
        assert_eq!(default_target("dev"), Some("https://aip.dev.oxy.tech"));
        assert_eq!(
            default_target("staging"),
            Some("https://aip.staging.oxy.tech")
        );
        assert_eq!(
            default_target("production"),
            Some("https://app.oxygen-hq.com")
        );
        assert_eq!(default_target("prod"), Some("https://app.oxygen-hq.com"));
        assert_eq!(default_target("bogus"), None);
    }
}
