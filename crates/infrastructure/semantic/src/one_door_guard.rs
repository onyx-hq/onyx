//! Keeps [`crate::engine_cache`] the only way to build a `SemanticEngine`.
//!
//! The cache existed before this guard and was used by exactly one call site.
//! Every other surface hand-rolled `spawn_blocking` → `from_semantic_layer` →
//! compile → drop, so the engine build the cache claims is "paid once per
//! workspace per TTL" was in fact paid per request on the surfaces with the
//! biggest layers. Nothing in the type system stops the next handler doing it
//! again. This does.
//!
//! Same shape as `crates/app/tests/platform/compiled_reader_is_not_a_back_door.rs`: a
//! source scan with an allowlist where each entry carries its reason, and where
//! a STALE entry fails too — otherwise the list only grows and stops meaning
//! anything.
//!
//! It lives in this crate rather than `oxy-app/tests/` on purpose: it reads
//! source text and links nothing, so hosting it here costs one more test in a
//! 4-dependency leaf instead of a fresh multi-hundred-MB test binary in
//! `oxy-app` (see the "Build & Test" note in the root `CLAUDE.md`).

#![cfg(test)]

use std::path::{Path, PathBuf};

/// The calls that build an engine from scratch.
///
/// Two spellings, because #3034 made `oxy_airlayer_compat::build_engine` the
/// canonical one but a few sites still reach for the raw airlayer constructor.
/// Both are "an engine built outside the cache", which is what this polices.
///
/// The qualified path matters: `agentic-analytics` has its own unrelated
/// `build_engine` that constructs a VENDOR engine (Cube et al.), and a bare
/// substring would report it as an offender forever.
const RAW_BUILDS: &[&str] = &[
    "oxy_airlayer_compat::build_engine(",
    "SemanticEngine::from_semantic_layer",
];

/// Whether `text` constructs an engine by either spelling.
fn builds_an_engine(text: &str) -> bool {
    RAW_BUILDS.iter().any(|pat| text.contains(pat))
}

/// Files that may build an engine directly, and why.
///
/// Three kinds only. Anything else goes through
/// `SemanticEngineCache::get_or_build` (or `resolve_and_compile_cached`).
///
///   1. **The door itself**, and the one wrapper that feeds it.
///   2. **A layer the cache cannot hold** — mutated per request, per run, or
///      per cycle, so a workspace-keyed entry would be a wrong answer, not a
///      slow one.
///   3. **Validation of a hypothetical layer** — a proposed edit that is not
///      the workspace's layer and must never be cached as one.
const ALLOWED: &[(&str, &str)] = &[
    // ── 1. The door ─────────────────────────────────────────────────────────
    (
        "crates/agentic/semantic/src/compile.rs",
        "`load_and_build`, the builder both `resolve_and_compile` and \
         `resolve_and_compile_cached` hand to the cache. It parses the layer \
         and defers the engine build itself to `oxy_airlayer_compat`.",
    ),
    (
        "crates/app/src/server/api/middlewares/workspace_context.rs",
        "`SemanticEngineCacheCtx::get_or_build` — the request-scoped handle \
         that supplies the key and the builder.",
    ),
    // ── 2. Layers the cache must not hold ───────────────────────────────────
    (
        "crates/app/src/server/preagg_rebuild.rs",
        "Cycle-scoped: a filtered view subset with topics dropped, under a \
         `with_default` dialect map rather than the workspace's. Already built \
         once per cycle, and deliberately so — see `RebuildContext::engine`, \
         where building per rollup turns one malformed view into a failure of \
         every rollup in the workspace.",
    ),
    (
        "crates/app/src/agentic_wiring/metric_tree_runner.rs",
        "The opportunity drill installs synthetic `__drill__` measures into a \
         `SharedLayer` mid-run and the executor must see them, so this engine \
         is rebuilt per query on purpose. A frozen snapshot would reject a \
         measure it never saw.",
    ),
    (
        "crates/app/src/server/api/metric_tree.rs",
        "`post_opportunity` runs `augment_layer_for_opportunity` first, so its \
         layer is unique to the request and shares nothing with the \
         workspace's.",
    ),
    (
        "crates/app/src/server/api/world_model_graph/handlers.rs",
        "`measure_breakdown_core`'s fallback for the customer-app caller, which \
         enters through `enter_semantic_boundary` — headers-driven, with no \
         `AppState` to carry a cache. The workspace caller passes its cached \
         engine.",
    ),
    (
        "crates/app/src/server/api/projects/metric_tree.rs",
        "`run_baseline_query`, on the same customer-app boundary as \
         `world_model_graph/handlers.rs` above: these routes are mounted in \
         `router/public.rs`, which does not carry `workspace_middleware`, so \
         the `SemanticEngineCacheCtx` extension that supplies the cache is \
         never inserted. `SemanticBoundary` holds a `CustomAppContext` and a \
         scan dir and nothing else. The workspace twin \
         (`server/api/metric_tree.rs`) takes the extractor and IS cached.",
    ),
    (
        "crates/app/src/server/api/projects/cohort.rs",
        "`post_cohort` runs `augment_layer_for_cohort` first, which installs a \
         synthetic `__cohort_total__` count-distinct measure the truncation \
         guard reads — so the layer is unique to the request, exactly like \
         `post_opportunity` above. It is also on the customer-app boundary \
         (mounted in `router/public.rs`), so no `SemanticEngineCacheCtx` \
         extension exists to supply a cache in the first place.",
    ),
    (
        "crates/app/src/server/simulation/probe.rs",
        "Run-scoped: `FitProbe` parses its layer out of the materialised world \
         directory a run generates, not the workspace's. The cache is keyed by \
         workspace id, so an entry for it would hand the next run — or a real \
         request — an engine built over a temp dir that no longer exists. \
         Built once per run, not per period; see `FitProbe::new`.",
    ),
    // ── 3. Hypothetical layers ──────────────────────────────────────────────
    (
        "crates/infrastructure/semantic/src/lib.rs",
        "Defines `build_engine` itself, and `gate_semantic_write` checks a \
         PROPOSED edit against a dialect-agnostic engine — not the workspace's \
         layer, and never cacheable as one.",
    ),
    (
        "crates/agentic/analytics/src/semantic/mod.rs",
        "`SemanticCatalog::empty()` builds over zero views (nothing to cache), \
         and `load_files` takes an arbitrary path set with dialects derived \
         from live connectors rather than config.",
    ),
];

fn repo_root() -> PathBuf {
    // crates/infrastructure/semantic → repo root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
}

/// Every `.rs` under `crates/`, as (repo-relative path, code-only contents).
///
/// This guard's own file is skipped — it necessarily names the call it forbids.
fn source_files() -> Vec<(String, String)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                // `tests/` is integration tests; a test may build an engine freely.
                if matches!(name.as_str(), "target" | "node_modules" | ".git" | "tests") {
                    continue;
                }
                walk(&path, root, out);
            } else if name.ends_with(".rs")
                && name != "one_door_guard.rs"
                && !name.ends_with("_tests.rs")
                && name != "tests.rs"
            {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    out.push((rel, code_only(&text)));
                }
            }
        }
    }
    let root = repo_root();
    let mut out = Vec::new();
    walk(&root.join("crates"), &root, &mut out);
    out
}

/// Strip what is not a call: line comments (a doc comment naming the call is
/// not a call — `preagg_executor.rs` explains the cost in prose and would
/// otherwise read as an offender) and every `#[cfg(test)]` item.
///
/// Test modules are matched by attribute, not by the name `tests`: the
/// analytics validation fixtures live in a differently-named `#[cfg(test)]`
/// module, and a name-based rule silently missed them.
fn code_only(text: &str) -> String {
    let no_comments: String = text
        .lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n");
    strip_cfg_test(&no_comments)
}

/// Remove every `#[cfg(test)]`-annotated item, brace-matched.
fn strip_cfg_test(text: &str) -> String {
    const ATTR: &str = "#[cfg(test)]";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(ATTR) {
        out.push_str(&rest[..at]);
        let after = &rest[at + ATTR.len()..];
        // An attribute on an item with no block (`#[cfg(test)] use ...;`)
        // terminates at the `;`. Without this check the brace matcher would
        // run on to some unrelated later block and delete real code.
        let open = match (after.find('{'), after.find(';')) {
            (Some(o), Some(semi)) if semi < o => {
                rest = &after[semi + 1..];
                continue;
            }
            (Some(o), _) => o,
            (None, Some(semi)) => {
                rest = &after[semi + 1..];
                continue;
            }
            (None, None) => return out,
        };
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in after[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => rest = &after[e..],
            // Unbalanced; give up rather than mis-slice.
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// The scan is text-based, so a parsing slip could silently drop files and turn
/// this guard into a pass-everything no-op. A floor on the file count catches
/// that: the workspace has hundreds of `.rs` files, so anything near zero means
/// the walk or the comment/`cfg(test)` stripping broke, not that the repo shrank.
#[test]
fn the_scan_actually_sees_the_repo() {
    let files = source_files();
    assert!(
        files.len() > 300,
        "only {} source files scanned — the walk is broken, so every other \
         assertion in this module is vacuous",
        files.len(),
    );
    assert!(
        files
            .iter()
            .any(|(p, _)| p == "crates/agentic/semantic/src/compile.rs"),
        "a known engine-building file is missing from the scan",
    );
    // Stripping must not eat the file: `compile.rs` builds an engine outside
    // any test module, so its code must survive `code_only`.
    let (_, compile_rs) = files
        .iter()
        .find(|(p, _)| p == "crates/agentic/semantic/src/compile.rs")
        .expect("checked above");
    assert!(
        builds_an_engine(compile_rs),
        "`code_only` stripped a real call out of compile.rs — brace matching \
         or comment stripping desynchronised",
    );
}

#[test]
fn engine_construction_goes_through_the_cache() {
    let offenders: Vec<String> = source_files()
        .into_iter()
        .filter(|(path, text)| {
            builds_an_engine(text) && !ALLOWED.iter().any(|(allowed, _)| allowed == path)
        })
        .map(|(path, _)| path)
        .collect();

    assert!(
        offenders.is_empty(),
        "these files build a `SemanticEngine` directly instead of going through \
         `SemanticEngineCache` (`resolve_and_compile_cached`, or \
         `SemanticEngineCacheCtx::get_or_build`):\n  {}\n\n\
         Rebuilding the engine per request revalidates the whole semantic model \
         and rebuilds the join graph. If this site genuinely cannot share a \
         workspace-keyed engine — a mutated, per-run, or hypothetical layer — \
         add it to ALLOWED with the reason.",
        offenders.join("\n  "),
    );
}

#[test]
fn allowlist_has_no_stale_entries() {
    let files = source_files();
    let stale: Vec<&str> = ALLOWED
        .iter()
        .filter(|(allowed, _)| {
            match files.iter().find(|(path, _)| path == allowed) {
                // Still there, still builds an engine: a live exemption.
                Some((_, text)) => !builds_an_engine(text),
                // File is gone.
                None => true,
            }
        })
        .map(|(allowed, _)| *allowed)
        .collect();

    assert!(
        stale.is_empty(),
        "these ALLOWED entries no longer build an engine (or the file is gone) \
         — drop them, or the list stops meaning anything:\n  {}",
        stale.join("\n  "),
    );
}
