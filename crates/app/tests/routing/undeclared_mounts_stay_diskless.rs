//! The routers `oxy-app` mounts without declaring a role must not reach disk.
//!
//! A merge at a tree's ROOT has no prefix to hang a declaration on, so
//! `RoleRouter::merge_undeclared` mounts those routers and their routes fall to
//! `classify`'s FleetOk default — the one hole the type-level gate cannot close.
//!
//! That is safe today for a reason nothing was checking: neither crate reaches
//! the workspace working copy, `.git`, or the local state dir at all. Measured
//! when this file was written — `oxy-cameras` mounts 75 routes and `airhouse` 6,
//! and the count of workspace-FS accesses across both crates is zero.
//!
//! So the guard is not "classify these routes correctly", it is "keep them
//! diskless". A camera handler that started reading the working copy would be
//! served on a replica that has none, with nothing to catch it — the exact
//! failure this branch exists to make unrepresentable.

use std::path::Path;

/// Every way a handler reaches node-local state, as the route-classification
/// skill defines it.
const WORKSPACE_FS: &[&str] = &[
    "workspace_path",
    "effective_workspace_path",
    "resolve_state_dir",
    "GitClient",
    "WorkspaceManagerWorkingCopy",
    "WorkspaceRootWorkingCopy",
    "ConfigManager",
    // Creating a workspace scaffolds its working copy.
    "workspace_provisioning",
    "create_blank_workspace",
    "create_default_workspace",
];

/// Accesses a scanned crate makes behind a route it DOES declare, so the
/// FleetOk default never applies to them. `(file suffix, needle)` — per needle,
/// so the exemption cannot quietly cover a second kind of access in that file.
const DECLARED_ACCESSES: &[(&str, &str)] = &[
    // `POST /partners/{id}/orgs` creates the client org's Default workspace;
    // `oxy_api_partner_console::route_roles()` declares it IdeOnly, and that
    // crate's own test asserts the declaration classifies.
    ("api-partner-console/src/orgs.rs", "workspace_provisioning"),
    (
        "api-partner-console/src/orgs.rs",
        "create_default_workspace",
    ),
];

fn rust_sources(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_sources(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
            && let Ok(body) = std::fs::read_to_string(&path)
        {
            out.push((path.display().to_string(), body));
        }
    }
    out
}

#[test]
fn the_crates_mounted_without_a_declaration_never_touch_the_working_copy() {
    let mut offenders = Vec::new();
    let mut files_scanned = 0;

    for crate_dir in [
        "../cameras/src",
        "../airhouse/src",
        // Declared FleetOk wholesale by the `/admin` wildcard in global.rs, so a
        // route added under one inherits FleetOk without anyone deciding. Measured
        // diskless when those declarations were written; the reconcile config
        // read under `/admin/workspace-health` runs on the WORKER, not on the
        // request path, which is why it is not an exception here.
        // Extracted from `src/server/api/partner_console` into a sibling crate.
        // The guard asserts its own sources are non-empty precisely so a move
        // like that fails loudly instead of silently covering nothing. Merged
        // at the root with no prefix; the one route that needs the ide is in
        // its `route_roles()`, and exempted by `DECLARED_ACCESSES`.
        "../api-partner-console/src",
        "src/server/api/billing",
    ] {
        let sources = rust_sources(Path::new(crate_dir));
        assert!(
            !sources.is_empty(),
            "no sources under {crate_dir} — the crate moved and this guard \
             stopped covering it",
        );
        files_scanned += sources.len();

        for (path, body) in sources {
            for needle in WORKSPACE_FS {
                if DECLARED_ACCESSES
                    .iter()
                    .any(|(file, declared)| path.ends_with(file) && declared == needle)
                {
                    continue;
                }
                for (index, line) in body.lines().enumerate() {
                    // A mention in prose is not an access.
                    let code = line.split("//").next().unwrap_or(line);
                    if code.contains(needle) {
                        offenders.push(format!("{path}:{} — {needle}", index + 1));
                    }
                }
            }
        }
    }

    assert!(
        files_scanned > 20,
        "scanned only {files_scanned} files — the walk broke, and a guard that \
         reads nothing passes for the wrong reason",
    );

    assert!(
        offenders.is_empty(),
        "these crates are mounted with `merge_undeclared`, so their routes are \
         served on any pod — including replicas with no working copy. One of \
         them now reaches for one:\n  {}\n\nEither drop the access, or mount \
         the router through a door that declares a role.",
        offenders.join("\n  "),
    );
}
