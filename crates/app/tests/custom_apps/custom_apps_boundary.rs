//! Boundary test: the custom-apps surface must stay behind a thin, named set of seams.
//!
//! ## Why a source-scanning test
//!
//! The custom-apps surface is ~17.5k LOC across ~35 tightly-cohesive `custom_apps_*`
//! modules (plus the `custom_app_template` scaffold). An audit found it is *already*
//! well-bounded — it reaches the rest of `oxy-app` through only a handful of seams, and
//! it carries its own request context (`CustomAppContext`) rather than threading
//! `AppState`. That thinness is the whole reason the surface is legible and testable.
//!
//! But nothing *keeps* it thin. The shortest path when a custom-apps handler needs
//! something from billing, or admin, or a new service is to reach straight into that
//! module — and the boundary erodes one reasonable import at a time, exactly the way the
//! ~170-site authz scatter got built (see `authz_boundaries.rs`). A reviewer who doesn't
//! already hold the boundary in their head won't catch it. So the objection has to be
//! mechanical: this test fails the build when a custom-apps file imports an `oxy-app`
//! module outside the sanctioned seam list.
//!
//! ## The allowlist IS the coupling, written down — and shrinking it is the migration
//!
//! Each [`ALLOWED_SEAMS`] entry is a real dependency the surface has *today*, with the
//! reason it's tolerated. The list going UP means new coupling crept in (justify it or
//! don't add it). The list going DOWN — deleting a seam once its last import is gone — is
//! measurable progress toward making the surface extractable. The heaviest remaining seam,
//! `agentic_wiring::OxyProjectContext`, is the next target to invert behind a trait
//! (`server::api::projects` already was); when it's gone, a real Functions crate becomes feasible.
//!
//! To keep the list honest in both directions, [`no_unused_seams`] fails if an allowlisted
//! seam has no importer left — you cannot narrow the coupling and leave the entry behind.
//!
//! ## Precision over reach
//!
//! Only file-level and local `use` of `crate::`/`super::` paths are scanned; `#[cfg(test)]`
//! modules are skipped (their `use super::*` is an intra-file idiom, not a seam). Anything
//! naming a `custom_app*` segment is intra-cluster and ignored. A test that cries wolf gets
//! deleted, and then it protects nothing.
//!
//! ## Known gap — cross-crate paths are invisible here, by construction
//!
//! Only `crate::`/`super::`/`self::` paths are scanned, so an `oxy_*::` import of a
//! workspace sibling is not a seam and never will be flagged. That is deliberate and it is
//! the same thing the two `REMOVED 2026-07-27` notes above celebrate: when a dependency
//! moves DOWN into a crate both sides can depend on (`oxy-server-authz`, `oxy-shared`,
//! `oxy-app-core`), the `oxy-app` back-edge really is gone and deleting the seam really is
//! progress. The gap is that the test cannot tell that move apart from a move made purely
//! to dodge it — extracting a module to a sibling crate deletes a seam entry with no
//! review signal. Closing it means teaching the scanner which sibling crates are
//! sanctioned shared seams (`oxy-app-core`) versus which are feature crates custom-apps
//! must not reach into, which is a different model than the one this file implements.
//! Until then it stays a reviewer's obligation: an extraction that shrinks
//! [`ALLOWED_SEAMS`] should say where the coupling went.

use std::fs;
use std::path::{Path, PathBuf};

/// A sanctioned seam: a module-path prefix custom-apps files may import from, and why.
struct Seam {
    prefix: &'static str,
    why: &'static str,
}

/// The seams the custom-apps surface is allowed to reach into `oxy-app` through.
/// Ordered heaviest-coupling first — the top two are the decoupling targets.
const ALLOWED_SEAMS: &[Seam] = &[
    Seam {
        prefix: "crate::agentic_wiring",
        why: "OxyProjectContext — the pipeline adapter. The function runtime's *consumption* is \
               now inverted behind `custom_apps_functions::seam::FunctionProjectContext` \
               (host.rs no longer imports this); the only remaining importer is \
               `custom_apps_gates`, which CONSTRUCTS the per-request context — a builder shared \
               with the non-boundary /query + /semantic-query handlers. Fully deleting this seam \
               means relocating that construction to a composition root, deferred to a real \
               Functions-crate cut.",
    },
    // REMOVED 2026-07-25: `crate::server::api::projects` (the data-plane SQL path). The
    // function runtime's `ctx.query`/`ctx.queryStream` now runs through the runtime-owned
    // `FunctionQueryExecutor` trait, with the production impl
    // (`projects::query::DataPlaneQueryExecutor`) injected at the composition root (the serve
    // router + the scheduled-function worker). The runtime no longer imports `projects`.
    // This deletion IS the decoupling migration — do not re-add without a real new dependency.
    // REMOVED 2026-07-27: `crate::server::api::middlewares` (partner_authz) and
    // `crate::server::authz`. Both were oxy-app *shims* that re-export the `oxy-server-authz`
    // crate; custom-apps files now import `oxy_server_authz::{globals, loader, partner_authz}`
    // directly (an external crate, not a `crate::` back-edge). Deleting these seams is the
    // first "resolve the resolvable" step toward extracting the custom-apps surface: two of
    // the six cross-crate cycles are already broken because the authz decision layer is its
    // own crate. Do not re-add — reach oxy-server-authz directly.
    // REMOVED 2026-07-27: `crate::server::api::admin::apps::handlers` (build_pretty_url). The
    // `/customer-apps/<org>/<app>/` URL builder is shared by BOTH this surface and the admin
    // apps browser, so it moved DOWN to `oxy_shared::utils::custom_app_url` — a lower crate both
    // depend on. Moving it *into* custom-apps would have made admin depend on custom-apps (a
    // future cross-feature-crate edge the decomposition plan bans), so a shared home is correct.
    Seam {
        prefix: "crate::server::router",
        why: "the router/state seam — is_allowed_origin (custom_apps_gates) and AppState \
               (custom_apps_threads, the one handler that still takes app state).",
    },
    Seam {
        prefix: "crate::server::service::secret_manager",
        why: "the workspace secret manager (function ctx.secrets, gate authentication).",
    },
    Seam {
        prefix: "crate::server::api::operating_graph",
        why: "the operating graph — the org's places, positions and assignments, and the \
               reach rule over them (internal-docs/operating-graph.md). The function runtime \
               decides `ctx.user.reach` from it before the isolate starts (mod.rs), and \
               `ctx.org.places()` / `ctx.org.assignments()` read it (host.rs). It is org \
               data behind the app, like the secret manager: a platform fact every tenant's \
               app consumes, never one an app writes.",
    },
    Seam {
        prefix: "crate::server::api::workspace_org",
        why: "the \"is this workspace a live workspace of this org?\" predicate. \
               custom_apps_publish asks it twice — the cross-org guard (`validate_project`) \
               and the re-home rule (`ensure_same_workspace`) — and the admin apps \
               create/move endpoints ask the same question; one shared answer is what keeps \
               publish and the admin API from disagreeing about the same row (#3175). A pure \
               leaf — `entity::workspaces` and one `find_by_id`, no `AppState` — so at a \
               Functions-crate cut it moves DOWN into `oxy-app-core` (already on entity + \
               sea-orm), as `build_pretty_url` moved to `oxy_shared`, rather than into \
               custom-apps, which would make admin depend on it.",
    },
];

/// Compute a file's module path (`crate::a::b`) from its path relative to `src/`.
fn module_path(rel: &Path) -> Vec<String> {
    let mut parts: Vec<String> = rel
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.last().map(String::as_str) == Some("mod") {
        parts.pop();
    }
    let mut out = vec!["crate".to_string()];
    out.extend(parts);
    out
}

/// Resolve a `use` path's leading segments to an absolute module path, expanding
/// `super::`/`self::` against the importing file's module path.
fn resolve(segs: &[String], module: &[String]) -> Vec<String> {
    match segs.first().map(String::as_str) {
        Some("crate") => segs.to_vec(),
        Some("super") | Some("self") => {
            let mut base = module.to_vec();
            let mut i = 0;
            while segs.get(i).map(String::as_str) == Some("super") {
                if base.len() > 1 {
                    base.pop();
                }
                i += 1;
            }
            if segs.get(i).map(String::as_str) == Some("self") {
                i += 1;
            }
            base.extend(segs[i..].iter().cloned());
            base
        }
        _ => segs.to_vec(),
    }
}

/// Expand one level of `prefix::{a, b::c, self}` into leaf paths. Non-grouped paths
/// pass through unchanged.
fn expand_group(path: &str) -> Vec<String> {
    let Some(open) = path.find("::{") else {
        return vec![path.to_string()];
    };
    let prefix = &path[..open];
    let inner = &path[open + 3..path.rfind('}').unwrap_or(path.len())];
    let mut items = Vec::new();
    let (mut depth, mut cur) = (0i32, String::new());
    for ch in inner.chars() {
        match ch {
            '{' => {
                depth += 1;
                cur.push(ch);
            }
            '}' => {
                depth -= 1;
                cur.push(ch);
            }
            ',' if depth == 0 => {
                items.push(std::mem::take(&mut cur));
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        items.push(cur);
    }
    items
        .iter()
        .map(|it| {
            let it = it.trim();
            if it.is_empty() || it == "self" {
                prefix.to_string()
            } else {
                format!("{prefix}::{it}")
            }
        })
        .collect()
}

/// Blank out `#[cfg(test)]`-gated regions so their intra-file `use super::*` idiom
/// isn't mistaken for a seam.
fn strip_cfg_test(text: &str) -> String {
    // Operate on bytes throughout — indexing `&str` at an arbitrary byte offset would
    // panic at a non-char-boundary (comments contain multi-byte UTF-8 like em-dashes).
    let b = text.as_bytes();
    let marker = b"#[cfg(test)]";
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(marker) {
            // Blank exactly the gated ITEM, not "everything up to the next `{`
            // anywhere in the file". A braced item (`mod tests { … }`, an `fn`)
            // ends at the `}` matching its first `{`; a statement item
            // (`use super::x;`, a `const`) ends at its `;`. Whichever terminator
            // comes first after the marker decides — so a braceless
            // `#[cfg(test)] use …;` can't make us brace-match a *later* item and
            // silently blank a real seam (a false negative, the worst failure
            // mode for a boundary test).
            let rest = &b[i + marker.len()..];
            let brace = rest.iter().position(|&c| c == b'{');
            let semi = rest.iter().position(|&c| c == b';');
            i = match (brace, semi) {
                // `;` reached before any `{`: a statement item — blank through it.
                (None, Some(s)) => i + marker.len() + s + 1,
                (Some(bpos), Some(s)) if s < bpos => i + marker.len() + s + 1,
                // A `{` comes first: a braced item — blank through its match.
                (Some(bpos), _) => {
                    let mut j = i + marker.len() + bpos + 1;
                    let mut depth = 1i32;
                    while j < b.len() && depth > 0 {
                        match b[j] {
                            b'{' => depth += 1,
                            b'}' => depth -= 1,
                            _ => {}
                        }
                        j += 1;
                    }
                    j
                }
                // Neither terminator (malformed / EOF): drop just the marker.
                (None, None) => i + marker.len(),
            };
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A `custom_app*` segment anywhere means the target is inside the cluster.
fn is_intra_cluster(resolved: &[String]) -> bool {
    resolved.iter().any(|s| s.contains("custom_app"))
}

/// Collect file-level + local `use crate::`/`use super::`/`use self::` statements
/// (joining line continuations), skipping `#[cfg(test)]` regions.
fn use_statements(text: &str) -> Vec<String> {
    let cleaned = strip_cfg_test(text);
    let lines: Vec<&str> = cleaned.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let t = lines[i].trim_start();
        if t.starts_with("use crate::")
            || t.starts_with("use super::")
            || t.starts_with("use self::")
            || t.starts_with("pub use crate::")
            || t.starts_with("pub use super::")
            || t.starts_with("pub use self::")
        {
            let mut stmt = t.to_string();
            while !stmt.contains(';') && i + 1 < lines.len() {
                i += 1;
                stmt.push_str(lines[i].trim());
            }
            let body = stmt
                .trim_start_matches("pub ")
                .trim_start_matches("use ")
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            out.push(body);
        }
        i += 1;
    }
    out
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The inside-set: every `*.rs` under `src/` whose path names `custom_app`
/// (covers `custom_apps_*` + `custom_app_template`), excluding pure test files.
fn inside_files(src: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    rust_sources(src, &mut all);
    all.into_iter()
        .filter(|p| {
            let s = p.to_string_lossy();
            s.contains("custom_app") && !s.ends_with("tests.rs")
        })
        .collect()
}

#[test]
fn custom_apps_stays_behind_its_seams() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = inside_files(&src);
    assert!(
        files.len() > 20,
        "expected to scan the custom-apps surface (~35 files), found only {} — the walk is \
         broken, and a boundary test that silently scans nothing is worse than no test",
        files.len()
    );

    let mut violations = Vec::new();
    let mut seam_hits = vec![false; ALLOWED_SEAMS.len()];

    for path in &files {
        let rel = path.strip_prefix(&src).unwrap();
        let module = module_path(rel);
        let text = fs::read_to_string(path).unwrap_or_default();
        for stmt in use_statements(&text) {
            for leaf in expand_group(&stmt) {
                let leaf = leaf.split(" as ").next().unwrap_or(&leaf).trim();
                let segs: Vec<String> = leaf
                    .split("::")
                    .filter(|s| !s.is_empty() && *s != "*")
                    .map(str::to_string)
                    .collect();
                if segs
                    .first()
                    .map(String::as_str)
                    .filter(|s| matches!(*s, "crate" | "super" | "self"))
                    .is_none()
                {
                    continue;
                }
                let resolved = resolve(&segs, &module);
                if is_intra_cluster(&resolved) {
                    continue;
                }
                let joined = resolved.join("::");
                match ALLOWED_SEAMS.iter().position(|s| {
                    joined == s.prefix || joined.starts_with(&format!("{}::", s.prefix))
                }) {
                    Some(idx) => seam_hits[idx] = true,
                    None => violations.push(format!(
                        "  {}\n    imports: {joined}",
                        rel.to_string_lossy()
                    )),
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "\n{} custom-apps import(s) reach outside the sanctioned seams:\n\n{}\n\n\
         The custom-apps surface is kept thin on purpose. A new cross-boundary import is either:\n\
         (a) legitimate new coupling — add its module prefix to ALLOWED_SEAMS in \
         tests/custom_apps_boundary.rs with the reason (writing the reason down is the cost), or\n\
         (b) an erosion of the boundary — reach the thing through an existing seam, or don't.\n\n\
         Note: only `use` statements are scanned, so an inline fully-qualified path \
         (`crate::billing::charge(..)` with no `use`) is NOT caught — this gate raises the cost \
         of crossing the boundary, it doesn't make it impossible.\n",
        violations.len(),
        violations.join("\n\n")
    );

    // Keep the allowlist honest: a seam with no importer left must be deleted, so that
    // narrowing the coupling actually shrinks the list.
    let unused: Vec<&str> = ALLOWED_SEAMS
        .iter()
        .zip(&seam_hits)
        .filter(|(_, hit)| !**hit)
        .map(|(s, _)| s.prefix)
        .collect();
    assert!(
        unused.is_empty(),
        "\nALLOWED_SEAMS lists seam(s) no custom-apps file imports anymore:\n  {}\n\n\
         Shrinking this list is the decoupling migration — delete the stale entry.\n",
        unused.join("\n  ")
    );
}
