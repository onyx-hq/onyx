use super::*;

/// Every guard below asserts policy about routes, and the routes now carry
/// their own roles — so classify against the real router rather than the
/// tables it is replacing. Shadows the module-level `classify`.
fn classify(method: &str, path: &str) -> RouteRole {
    install_route_declarations_for_tests();
    super::classify(method, path)
}

#[test]
fn only_serve_offloads_workers_single_all_in_one_drains_its_own_queue() {
    // The single-instance invariant: a plain `OXY_ROLE=all` node (and ide /
    // worker) runs the durable fleet + global driver in-process, so
    // scheduled + manual jobs execute without a second node. Only the
    // stateless `serve` replica offloads to the worker fleet. Pure over
    // `Role`, so it can assert every role without touching the process-role
    // OnceLock. Guards the DX regression where a lone node silently queues
    // jobs forever with nothing draining them.
    assert!(
        role_runs_inprocess_workers(Role::All),
        "a single OXY_ROLE=all instance must drain its own queue"
    );
    assert!(role_runs_inprocess_workers(Role::Ide));
    assert!(role_runs_inprocess_workers(Role::Worker));
    assert!(
        !role_runs_inprocess_workers(Role::Serve),
        "serve is a pure reader — it offloads to the worker fleet"
    );
}

#[test]
fn every_database_route_that_touches_the_working_copy_is_ide_only() {
    let ws = "d9830be4-c6a4";
    for (method, path) in [
        ("POST", ""),
        ("POST", "/sync"),
        ("POST", "/clean"),
        ("POST", "/test-connection"),
        ("POST", "/build"),
    ] {
        assert_eq!(
            classify(method, &format!("/api/{ws}/databases{path}")),
            RouteRole::IdeOnly,
            "{method} /databases{path} mutates the working copy"
        );
    }

    // The list degrades rather than failing without a working copy, and the
    // launcher readiness check calls it on every page load.
    assert_eq!(
        classify("GET", &format!("/api/{ws}/databases")),
        RouteRole::FleetOk
    );

    for (method, path) in [
        ("POST", "/inspect"),
        ("POST", "/inspect-schemas"),
        ("POST", "/inspect-schema-tables"),
        ("GET", "/warehouse/schema"),
    ] {
        assert_eq!(
            classify(method, &format!("/api/{ws}/databases{path}")),
            RouteRole::FleetOk,
            "{method} /databases{path} introspects the warehouse, not the disk"
        );
    }
}

#[test]
fn draining_the_queue_is_not_owning_a_working_copy() {
    // Worker is the role where these two diverge, and conflating them is what
    // let a diskless worker claim a Compile task and walk a tree that is not
    // there. `role_runs_inprocess_workers` is about who executes queued work;
    // `role_owns_workspace_files` is about who has files to execute it against.
    assert!(role_runs_inprocess_workers(Role::Worker));
    assert!(
        !role_owns_workspace_files(Role::Worker),
        "a worker drains the queue but holds no checkout"
    );

    assert!(role_owns_workspace_files(Role::Ide));
    assert!(role_owns_workspace_files(Role::All));
    assert!(!role_owns_workspace_files(Role::Serve));
}

#[test]
fn pattern_matches_concrete_uri() {
    // Single-segment params
    assert!(pattern_matches(
        "/api/{workspace_id}/compile",
        "/api/abc-123/compile"
    ));
    // Rest wildcards consume one OR more trailing segments
    assert!(pattern_matches(
        "/api/{workspace_id}/git/{*rest}",
        "/api/abc/git/status"
    ));
    assert!(pattern_matches(
        "/api/{workspace_id}/git/{*rest}",
        "/api/abc/git/commit/sha"
    ));
    // Non-match: wrong prefix
    assert!(!pattern_matches(
        "/api/{workspace_id}/compile",
        "/api/abc-123/threads"
    ));
    // Non-match: missing segment
    assert!(!pattern_matches(
        "/api/{workspace_id}/compile",
        "/api/abc-123"
    ));
    // Non-match: rest wildcard requires at least one segment
    assert!(!pattern_matches(
        "/api/{workspace_id}/git/{*rest}",
        "/api/abc/git"
    ));
}

#[test]
fn ide_routes_classify_against_live_uri() {
    // The real shape the middleware sees: `/api` prefix + nested workspace.
    assert_eq!(
        classify("POST", "/api/d9830be4-c6a4/compile"),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("POST", "/api/d9830be4-c6a4/files/cGF0aA"),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("DELETE", "/api/d9830be4-c6a4/files/cGF0aA/delete-file"),
        RouteRole::IdeOnly
    );
    // Real flat git route (NOT under `/git/` — that segment doesn't exist).
    assert_eq!(
        classify("POST", "/api/d9830be4-c6a4/pull-changes"),
        RouteRole::IdeOnly
    );
    // The `/onboarding/*` routes moved to the `oxy-api-onboarding` sibling
    // crate, which declares them and asserts them in its own
    // `tests/route_roles.rs`. They cannot be checked here: `oxy-app` does not
    // depend on that crate, so its declarations are absent from the set this
    // test installs and `classify` would answer with the FleetOk default —
    // a failure that says nothing about the product.
    assert_eq!(
        classify("POST", "/api/d9830be4-c6a4/modeling/run"),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("POST", "/api/d9830be4-c6a4/switch-branch"),
        RouteRole::IdeOnly
    );
    // `/orgs/{id}/onboarding/github` moved with the rest of the onboarding
    // surface — asserted in `crates/api-onboarding/tests/route_roles.rs`.
    // The two halves of /details: git state needs the ide, metadata does not.
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/git-state"),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/meta"),
        RouteRole::FleetOk
    );
    // The Looker query POSTs read node-local synced metadata, like the GET.
    assert_eq!(
        classify("POST", "/api/d9830be4-c6a4/integrations/looker/query"),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("POST", "/api/d9830be4-c6a4/integrations/looker/query/sql"),
        RouteRole::IdeOnly
    );
    // Running a test case is execution against the working copy.
    assert_eq!(
        classify("POST", "/api/d9830be4-c6a4/tests/cGF0aA/cases/0"),
        RouteRole::IdeOnly
    );
    // Its neighbours stay on the fleet — listing runs is a Postgres read,
    // and pinning it would put test history behind the singleton.
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/tests/cGF0aA/runs"),
        RouteRole::FleetOk
    );
    // Deletion is the counterpart of those three creators. `?delete_files=true`
    // removes the working copy, and on a replica the `path.exists()` guard
    // silently skipped it and still returned 200 — the caller was told their
    // files were gone while they sat on the ide's volume.
    assert_eq!(
        classify("DELETE", "/api/orgs/some-org/workspaces/d9830be4-c6a4"),
        RouteRole::IdeOnly
    );
    // The sibling LIST must stay on the fleet — viewing workspaces cannot
    // require the singleton.
    assert_eq!(
        classify("GET", "/api/orgs/some-org/workspaces"),
        RouteRole::FleetOk
    );
    // Git/working-copy STATE reads (regression: "Workspace directory not
    // found" on a serve replica).
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/git-state"),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/status"),
        RouteRole::IdeOnly
    );
    // Process-local BROADCASTER live SSE (regression: silent truncation on
    // a worker-less serve replica).
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/events"),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/events/lookup"),
        RouteRole::IdeOnly
    );
    // `/world-model/events` used to belong in this list and no longer does.
    // Its publishers append to `world_model_events` and every pod tails that
    // table onto its own bus, so the feed is no longer process-local and any
    // replica can serve a subscriber.
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/world-model/events"),
        RouteRole::FleetOk
    );
}

/// The agentic run/exec + generated-chart surface must stay IdeOnly: a
/// subrun's `execute_sql` runs in-process against local DuckDB, and charts
/// are read off local disk. Flipping any of these to FleetOk would serve
/// them on a no-working-copy replica — and for `/analytics` specifically it
/// would bypass the runtime serve-safety gate (the conditional un-pin),
/// breaking every workspace with an FS-bound database. This test is the
/// regression guard for that whole surface.
#[test]
fn run_exec_and_chart_surface_stays_ide_only() {
    let ws = "d9830be4-c6a4";
    // `/charts/{file}` is deliberately absent: the writer mirrors it to S3,
    // so it serves from any replica. `exported-charts` has no mirror.
    let cases: [(&str, String); 7] = [
        ("POST", format!("/api/{ws}/analytics/runs")),
        ("POST", format!("/api/{ws}/analytics/runs/abc/answer")),
        ("POST", format!("/api/{ws}/agentic-workflows/run")),
        ("POST", format!("/api/{ws}/agentic-airway/run")),
        ("GET", format!("/api/{ws}/exported-charts/x.svg")),
        ("GET", format!("/api/{ws}/apps/cGF0aA")), // data-app auto-run
        ("POST", format!("/api/{ws}/apps/cGF0aA/run")), // data-app run
    ];
    for (method, path) in cases {
        assert_eq!(
            classify(method, &path),
            RouteRole::IdeOnly,
            "{method} {path} must stay IdeOnly"
        );
    }
}

/// Viewing a past conversation / run is a Postgres read and must NOT depend
/// on the ide singleton (HA). The run-history reads across all three agentic
/// surfaces are carved out to FleetOk even though they sit under the IdeOnly
/// `/analytics` `/agentic-workflows` `/agentic-airway` wildcards; the
/// EXECUTION + live-stream + file-read endpoints right next to them stay
/// IdeOnly.
#[test]
fn agentic_run_history_reads_are_fleet_ok() {
    let ws = "d9830be4-c6a4";
    // Pure Postgres reads → FleetOk (serve from any replica).
    for path in [
        format!("/api/{ws}/analytics/threads/t-1/runs"), // list_runs_by_thread
        format!("/api/{ws}/analytics/threads/t-1/run"),  // get_run_by_thread
        format!("/api/{ws}/agentic-workflows/runs"),     // list_runs_for_workflow
        format!("/api/{ws}/agentic-workflows/runs/r-1"), // get_workflow_run
        format!("/api/{ws}/agentic-workflows/threads/t-1/run"), // latest_run_for_thread
        format!("/api/{ws}/agentic-airway/runs"),        // list_runs_for_pipeline
        // These two had a carve-out but no assertion — deleting their
        // declaration changed behaviour and nothing went red, which a
        // mutation test found while moving them into `agentic-http`.
        format!("/api/{ws}/agentic-airway/coverage"), // airway_coverage
        format!("/api/{ws}/agentic-airway/backfill-ranges"), // airway_backfill_ranges
    ] {
        assert_eq!(
            classify("GET", &path),
            RouteRole::FleetOk,
            "{path} is a Postgres run-history read — must serve from any replica"
        );
    }
    // Execution / live-stream / FS-write endpoints under the same surfaces
    // stay IdeOnly — the carve-out must not widen to them. Note the
    // live-SSE `runs/{id}/events` has one MORE segment than the carved-out
    // `runs/{id}`, so it is not shadowed.
    for (method, path) in [
        ("POST", format!("/api/{ws}/analytics/runs")), // start (executes)
        ("GET", format!("/api/{ws}/analytics/runs/r-1/events")), // live SSE
        ("POST", format!("/api/{ws}/analytics/runs/r-1/answer")), // resume
        (
            "POST",
            format!("/api/{ws}/analytics/runs/r-1/revert-file-changes"),
        ), // FS write
        ("POST", format!("/api/{ws}/agentic-workflows/runs")), // start
        (
            "GET",
            format!("/api/{ws}/agentic-workflows/runs/r-1/events"),
        ), // live SSE
        (
            "POST",
            format!("/api/{ws}/agentic-workflows/runs/r-1/cancel"),
        ),
        ("GET", format!("/api/{ws}/agentic-workflows/files")), // workspace FS read
        ("POST", format!("/api/{ws}/agentic-airway/runs")),    // start
        ("GET", format!("/api/{ws}/agentic-airway/runs/r-1/events")), // live SSE
    ] {
        assert_eq!(
            classify(&method, &path),
            RouteRole::IdeOnly,
            "{method} {path} executes/streams/reads-FS — must stay IdeOnly"
        );
    }
}

#[test]
fn workspace_health_is_fleet_ok() {
    assert_eq!(
        classify("GET", "/api/admin/workspace-health"),
        RouteRole::FleetOk
    );
}

#[test]
fn workspace_health_eval_is_fleet_ok() {
    // The on-demand eval handler is a pure Postgres enqueue: it seeds a
    // Global `health_eval_workspace` task and returns 202. The heavy work
    // (workspace-context build + reconcile.yml FS fallthrough) runs in the
    // fleet executor that drains the task, NOT in this handler — route class
    // doesn't govern task execution. So the POST is FS-free and must serve
    // FleetOk: pinning it IdeOnly would block an operator from triggering an
    // eval whenever the ide is down, undercutting the offload's whole point.
    let ws = "d9830be4-c6a4";
    assert_eq!(
        classify("POST", &format!("/api/admin/workspace-health/{ws}/eval")),
        RouteRole::FleetOk
    );
}

#[test]
fn simulation_routes_are_fleet_ok_including_the_profit_race() {
    // Every simulation route reads or writes persisted rows only:
    // `simulation_definitions` through the compile boundary, `simulation_run*`
    // through Postgres, and the POST merely enqueues a TaskSpec. The run itself
    // DOES touch local disk — a per-run TempDir — but that happens on the
    // worker, which is why it is queued work rather than a spawn in a handler.
    //
    // The race in particular is the HA half of the rule: it is a
    // `workspace_id`-scoped join of `simulation_runs` onto
    // `simulation_run_periods`, so pinning it IdeOnly would make *viewing* a
    // finished race need the singleton — the exact failure the split exists to
    // prevent.
    let ws = "d9830be4-c6a4";
    for (method, path) in [
        ("GET", format!("/api/{ws}/simulations")),
        ("POST", format!("/api/{ws}/simulations/validate")),
        ("GET", format!("/api/{ws}/simulations/runs")),
        ("GET", format!("/api/{ws}/simulations/runs/abc")),
        ("POST", format!("/api/{ws}/simulations/demo/runs")),
        ("GET", format!("/api/{ws}/simulations/demo/race")),
    ] {
        assert_eq!(
            classify(method, &path),
            RouteRole::FleetOk,
            "{method} {path} must serve from any replica"
        );
    }
}

#[test]
fn workspace_metric_tree_routes_are_fleet_ok() {
    // Every workspace-surface metric-tree route must serve from ANY replica.
    //
    // They resolve their scan root through `semantic::resolve_query_scan_source`
    // — compile boundary first, working copy second — exactly like the
    // `/semantic` execute route next door, and their warehouse config comes
    // from the compiled workspace config, not the FS fallback. No `.git`, no
    // state dir.
    //
    // This list is a regression guard with a real outage behind it: the
    // handlers USED to call `config_manager.semantics_scan_path()` directly, so
    // on a stateless serve replica — which has no working copy — every call
    // 500'd with a flat "Failed to load semantic model" for every workspace
    // (oxy-hq/oxygen#878). The fix was to move them onto the compile boundary,
    // NOT to pin them to the ide: viewing a metric tree is a read, and a read
    // that needs the singleton is an HA bug. If one of these ever fails here,
    // check that the handler still resolves its scan root through the boundary
    // before reaching for an ide mount.
    let ws = "d9830be4-c6a4";
    for (method, path) in [
        ("GET", "".to_string()),
        ("GET", "/revenue/sensitivity".to_string()),
        ("POST", "/predict".to_string()),
        ("POST", "/explain".to_string()),
        ("POST", "/opportunity".to_string()),
        ("POST", "/drill".to_string()),
        ("GET", "/time-dimensions".to_string()),
        ("POST", "/distribution".to_string()),
        ("POST", "/baseline".to_string()),
        // The scenario projection reads exactly what `baseline` reads — the
        // compiled layer plus one warehouse query — differing only in that it
        // asks for the window broken out by bucket. Classified with its
        // sibling: if one of these two ever has to move, both do, and
        // splitting them would leave the scenario canvas drawing levels from
        // one fleet and curves from another.
        ("POST", "/projection".to_string()),
    ] {
        assert_eq!(
            classify(method, &format!("/api/{ws}/semantic/metric-tree{path}")),
            RouteRole::FleetOk,
            "workspace metric-tree route {method} /semantic/metric-tree{path} must \
             stay FleetOk (compile-boundary scan root — see resolve_query_scan_source)"
        );
    }
}

#[test]
fn runtime_routes_are_ide_only() {
    // The DuckDB / local-execution subset. Each one EXECUTES in-process on the
    // ide (a query, a run, a process-local event stream), so it cannot be
    // answered by a replica that has neither the execution env nor the
    // broadcaster — regardless of what its handler's signature suggests.
    let ws = "d9830be4-c6a4";
    let runtime: [(&str, String); 7] = [
        ("GET", format!("/api/{ws}/apps/cGF0aA")),
        ("POST", format!("/api/{ws}/apps/cGF0aA/run")),
        ("POST", format!("/api/{ws}/apps/cGF0aA/result")),
        ("GET", format!("/api/{ws}/apps/file/cGF0aA")),
        ("POST", format!("/api/{ws}/analytics/runs")),
        ("POST", format!("/api/{ws}/agentic-airway/run")),
        ("GET", format!("/api/{ws}/events")),
    ];
    for (method, path) in &runtime {
        assert_eq!(
            classify(method, path),
            RouteRole::IdeOnly,
            "{method} {path} must stay IdeOnly"
        );
    }

    // The neighbours that must NOT be swept along: same prefixes, but pure
    // reads of persisted data, so pinning them to the singleton would make
    // *viewing* need the ide — the HA bug the split exists to prevent.
    for (method, path) in [
        ("GET", format!("/api/{ws}/threads")),
        ("GET", format!("/api/{ws}/apps/cGF0aA/displays")),
        // Reads `world_model_events` from Postgres — it sits under a prefix
        // full of ide-pinned streams, which is exactly why it is named here.
        ("GET", format!("/api/{ws}/world-model/events")),
    ] {
        assert_eq!(
            classify(method, &path),
            RouteRole::FleetOk,
            "{method} {path} is a persisted-data read and must serve from any replica"
        );
    }
}

#[test]
fn unknown_routes_default_to_fleet_ok() {
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/threads"),
        RouteRole::FleetOk
    );
    assert_eq!(classify("POST", "/api/analytics/runs"), RouteRole::FleetOk);
    assert_eq!(classify("GET", "/health"), RouteRole::FleetOk);
    assert_eq!(classify("GET", "/healthz"), RouteRole::FleetOk);
    // The agentic run/exec surface (/analytics, /agentic-workflows,
    // /agentic-airway) is pinned to the ide for ephemeral-env tier 1 —
    // subruns execute in-process where the run drives and touch the FS — so
    // even the cross-process /events streams under it now classify IdeOnly.
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/analytics/runs/abc/events"),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify(
            "GET",
            "/api/d9830be4-c6a4/agentic-workflows/runs/abc/events"
        ),
        RouteRole::IdeOnly
    );
    // /blocks reads persisted blocks from Postgres (no broadcaster) — FleetOk.
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/blocks"),
        RouteRole::FleetOk
    );
    // Every `/world-model/*` read is FleetOk — the Postgres+S3 ones here, and
    // `/world-model/events` since its feed moved into Postgres.
    assert_eq!(
        classify("GET", "/api/d9830be4-c6a4/world-model/cameras"),
        RouteRole::FleetOk
    );
    // But `/semantic/world-model*` scan the workspace working copy directly
    // (config_manager.semantics_scan_path), so they're IdeOnly — a serve
    // replica has no working copy and 500s ("Failed to load semantic
    // model"). Regression guard for oxygen-internal 2026-07-27.
    for (method, path) in [
        ("GET", "/api/d9830be4-c6a4/semantic/world-model"),
        ("GET", "/api/d9830be4-c6a4/semantic/world-model/instances"),
        (
            "GET",
            "/api/d9830be4-c6a4/semantic/world-model/filter-instances",
        ),
        (
            "POST",
            "/api/d9830be4-c6a4/semantic/world-model/filter-counts",
        ),
        (
            "GET",
            "/api/d9830be4-c6a4/semantic/world-model/instance-detail",
        ),
        (
            "GET",
            "/api/d9830be4-c6a4/semantic/world-model/measure-breakdown",
        ),
    ] {
        assert_eq!(
            classify(method, path),
            RouteRole::IdeOnly,
            "{method} {path} must be IdeOnly (reads the workspace working copy)"
        );
    }
    // Customer-apps batch mutations touch only Postgres + the S3 build
    // store (never the workspace FS), so — like their per-app siblings —
    // they classify FleetOk by default and serve from any replica.
    assert_eq!(
        classify("POST", "/api/customer-apps/batch/publish"),
        RouteRole::FleetOk
    );
    assert_eq!(
        classify("POST", "/api/customer-apps/batch/promote-latest"),
        RouteRole::FleetOk
    );
    assert_eq!(
        classify("POST", "/api/customer-apps/batch/unpublish"),
        RouteRole::FleetOk
    );
    assert_eq!(
        classify("POST", "/api/customer-apps/batch/delete"),
        RouteRole::FleetOk
    );
}

#[test]
fn org_subdomain_routes_are_fleet_ok() {
    // Both org-subdomain surfaces are Postgres-only (read workspace→org,
    // upsert the `org_subdomains` row) — no workspace FS — so they serve
    // from any replica, like `oxy-access`. See
    // `internal-docs/org-subdomain-infra.md`.
    let id = "d9830be4-c6a4";
    // Customer read-only status (workspace-scoped).
    assert_eq!(
        classify("GET", &format!("/api/{id}/org-subdomain")),
        RouteRole::FleetOk,
    );
    // Oxy-staff control (admin surface).
    for method in ["GET", "PUT"] {
        assert_eq!(
            classify(method, &format!("/api/admin/orgs/{id}/subdomain")),
            RouteRole::FleetOk,
            "{method} admin org-subdomain must be FleetOk (Postgres-only)"
        );
    }
}

#[test]
fn admin_create_org_reaches_the_ide_and_the_rest_of_the_console_does_not() {
    // Creating an org scaffolds its Default workspace — a working copy on
    // node-local disk — so a stateless replica must not answer it.
    assert_eq!(
        classify("POST", "/api/admin/orgs"),
        RouteRole::IdeOnly,
        "POST admin create-org writes the new org's Default workspace working copy"
    );
    // The carve-out names its verb and its path: the billing list on the same
    // path, and the rest of the orgs console, stay on the fleet.
    let id = "d9830be4-c6a4";
    for (method, path) in [
        ("GET", "/api/admin/orgs".to_string()),
        ("GET", "/api/admin/orgs-meta".to_string()),
        ("GET", format!("/api/admin/orgs/{id}/detail")),
        ("PATCH", format!("/api/admin/orgs/{id}")),
        ("DELETE", format!("/api/admin/orgs/{id}")),
        ("GET", "/api/admin/feature-flags".to_string()),
    ] {
        assert_eq!(
            classify(method, &path),
            RouteRole::FleetOk,
            "{method} {path} is Postgres-only and must stay FleetOk"
        );
    }
}

#[test]
fn ide_only_accepted_by_ide_and_all_only() {
    let r = RouteRole::IdeOnly;
    assert!(r.accepted_by(Role::Ide));
    assert!(r.accepted_by(Role::All));
    assert!(!r.accepted_by(Role::Serve));
    assert!(!r.accepted_by(Role::Worker));
}

#[test]
fn fleet_ok_accepted_by_every_role_including_worker() {
    let r = RouteRole::FleetOk;
    assert!(r.accepted_by(Role::Ide));
    assert!(r.accepted_by(Role::Serve));
    assert!(r.accepted_by(Role::Worker));
    assert!(r.accepted_by(Role::All));
}

// The next two tests set OXY_ROLE + init the process-role OnceLock; they
// rely on nextest's per-test process isolation (CLAUDE.md mandates nextest),
// the same pattern the role_middleware/types tests use.

#[test]
fn fs_write_guard_refuses_on_serve_role() {
    // SAFETY: nextest isolates this test in its own single-threaded process.
    unsafe { std::env::set_var("OXY_ROLE", "serve") };
    init_process_role_from_env();
    assert!(
        !process_is_fs_writable(),
        "serve replica owns no filesystem"
    );
    assert!(
        ensure_fs_writable("test write").is_err(),
        "serve replica must refuse a workspace FS write (super_read_only)"
    );
}

#[test]
fn fs_write_guard_allows_fs_owning_roles() {
    // SAFETY: nextest isolates this test in its own single-threaded process.
    // `all` is the default; assert the guard is a no-op for an FS-owning role.
    unsafe { std::env::set_var("OXY_ROLE", "ide") };
    init_process_role_from_env();
    assert!(process_is_fs_writable(), "ide owns the working copy");
    assert!(
        ensure_fs_writable("test write").is_ok(),
        "an FS-owning role must permit workspace FS writes"
    );
}

#[test]
fn method_wildcard_matches_any_verb() {
    // `/branches` is a `method: "*"` IdeOnly entry — both verbs match.
    assert_eq!(classify("GET", "/api/abc/branches"), RouteRole::IdeOnly);
    assert_eq!(
        classify("DELETE", "/api/abc/branches/feature-x"),
        RouteRole::IdeOnly
    );
}

#[test]
fn app_data_and_source_file_reads_are_ide_only() {
    // Every handler that calls `AppService::run()` (executes the inline
    // automation's file-path SQL) is ide-pinned: get_app_data (GET
    // /apps/{pathb64}, the auto-run on load), run_app (POST .../run) and
    // get_app_result (POST .../result). The file/source reads
    // (get_source_file → workspace_path; get_data → local state dir) are
    // ide-pinned too. The non-executing surface stays fleet-served:
    // get_displays returns SQL templates for the FE to run.
    let ws = "/api/d9830be4-c6a4";
    assert_eq!(
        classify("GET", &format!("{ws}/apps/source/b3h5bWFydA")),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("GET", &format!("{ws}/apps/file/b3h5bWFydA")),
        RouteRole::IdeOnly
    );
    // get_app_data runs the inline automation on a cold cache → ide.
    assert_eq!(
        classify("GET", &format!("{ws}/apps/b3h5bWFydA")),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("POST", &format!("{ws}/apps/b3h5bWFydA/run")),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("POST", &format!("{ws}/apps/b3h5bWFydA/result")),
        RouteRole::IdeOnly
    );
    // get_displays only emits SQL templates — no server-side run — so the
    // 4-segment GET pin above must NOT shadow it; it stays fleet-served.
    assert_eq!(
        classify("GET", &format!("{ws}/apps/b3h5bWFydA/displays")),
        RouteRole::FleetOk
    );
    // get_app_data_cached serves a dashboard's last cached data (boundary
    // def + disk/S3 cache, no execution) — the ide-down fallback. It MUST
    // stay FleetOk, or a serve replica would proxy it to a dead ide and
    // defeat the whole graceful-degradation feature.
    assert_eq!(
        classify("GET", &format!("{ws}/apps/b3h5bWFydA/data-cached")),
        RouteRole::FleetOk
    );
    // App WRITE surface mutates the working copy → IdeOnly (proxied to the
    // ide). FleetOk here would silently drop the publish toggle / generated
    // app on a working-copy-less replica.
    assert_eq!(
        classify("POST", &format!("{ws}/apps/b3h5bWFydA/publish")),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("POST", &format!("{ws}/apps/b3h5bWFydA/unpublish")),
        RouteRole::IdeOnly
    );
    assert_eq!(
        classify("POST", &format!("{ws}/apps/save-from-run/run-123")),
        RouteRole::IdeOnly
    );
}

/// Router-DERIVED drift guard — the automated router ⇄ manifest cross-check
/// the hand-maintained `manifest_covers_*` tests lack. Parses the route
/// paths straight out of `router/workspace.rs` for the builders that are
/// ENTIRELY filesystem/git (every route in them touches the working copy on
/// the singleton) and asserts each classifies IdeOnly. A new route added to
/// one of these builders is checked automatically — there is no separate
/// list to forget.
///
/// This canNOT cover MIXED builders (only some routes touch disk), e.g.
/// `build_app_routes`'s `/source` + `/file` reads vs its fleet-served
/// `/run` / `/result`. Those need a per-route test + the
/// `oxy-route-classification` skill at authoring time — that gap is exactly
/// how `/apps/source` shipped FleetOk and 404'd on the serve fleet.
#[test]
fn fully_fs_builder_routes_classify_ide_only() {
    let src = include_str!("router/workspace.rs");
    // (builder fn, mount prefix beneath /api/{workspace_id})
    let builders = [
        ("build_git_routes", ""),
        ("build_file_routes", "/files"),
        ("build_data_repo_routes", "/repositories"),
    ];
    let ws = "/api/d9830be4-c6a4";
    let mut checked = 0;
    for (builder, prefix) in builders {
        for route in route_paths_in_fn(src, builder) {
            let tail = if route == "/" { "" } else { route.as_str() };
            let concrete = concretize(&format!("{ws}{prefix}{tail}"));
            assert!(
                ["GET", "POST", "PUT", "DELETE", "PATCH"]
                    .iter()
                    .any(|m| classify(m, &concrete) == RouteRole::IdeOnly),
                "{prefix}{route} (mounted by {builder}) classifies FleetOk — every \
                 route in a fully-filesystem builder must be mounted with `route_ide`, \
                 or a serve replica with no working copy 404s/500s it.",
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 25,
        "parsed only {checked} routes from the FS builders — the source parser likely \
         broke; fix the parser rather than weakening the guard",
    );
}

/// Per-sub-route drift guard for `build_test_file_routes()`, MIXED since
/// `run_test_case` was pinned. The mount check above stops at `/tests`; this
/// is what examines what is under it.
#[test]
fn every_test_sub_route_is_classified() {
    let src = include_str!("router/workspace.rs");
    // Test sub-routes intentionally FleetOk. The two file reads are a KNOWN
    // GAP, recorded rather than hidden: `list_test_files` and `get_test_file`
    // read `.test.yml` off the working copy, and the compile boundary has no
    // artifact for them — `oxy_compile::walker` deliberately skips every path
    // containing `.test.`. Closing it is a new entity type (the five-step
    // contract), not a reclassification: pinning them IdeOnly would put the
    // IDE test list behind the singleton for a read.
    const TEST_FLEET_OK: &[&str] = &[
        "/",                                          // list_test_files — GAP, see above
        "/project-runs",                              // Postgres
        "/project-runs/{project_run_id}",             // Postgres
        "/{pathb64}",                                 // get_test_file — GAP, see above
        "/{pathb64}/runs",                            // Postgres run history
        "/{pathb64}/runs/{run_index}",                // Postgres
        "/{pathb64}/runs/{run_index}/human-verdicts", // Postgres
        "/{pathb64}/runs/{run_index}/cases/{case_index}/human-verdict", // Postgres
    ];
    assert_sub_routes_classified(src, "build_test_file_routes", "/tests", TEST_FLEET_OK, 1);
}

/// Per-sub-route drift guard for `build_integration_routes()`, MIXED since
/// the two Looker query POSTs were pinned. They read node-local synced
/// metadata with no S3 read-through, and were FleetOk only because they carry
/// one more path segment than the already-IdeOnly `GET /looker`.
#[test]
fn every_integration_sub_route_is_classified() {
    let src = include_str!("router/workspace.rs");
    const INTEGRATION_FLEET_OK: &[&str] = &[
        "/quickbooks/authorize", // OAuth redirect — no workspace files
        // Same shape, any provider: writes a Postgres state row and upserts the
        // client secret through the workspace secret manager. No working copy,
        // no .git, no local state dir — so any replica can serve it.
        "/oauth/{provider}/authorize",
    ];
    assert_sub_routes_classified(
        src,
        "build_integration_routes",
        "/integrations",
        INTEGRATION_FLEET_OK,
        3,
    );
}

/// Shared body of the per-sub-route guards: every route in `builder` must be
/// IdeOnly for some method, or be on the reviewed ack-list. Also rejects a
/// stale ack, so removing a route forces the list to shrink with it.
fn assert_sub_routes_classified(
    src: &str,
    builder: &str,
    nest: &str,
    ack: &[&str],
    min_ide_only: usize,
) {
    let ws = "/api/d9830be4-c6a4";
    let routes = route_paths_in_fn(src, builder);
    assert!(
        !routes.is_empty(),
        "parsed no routes from {builder} — fix the parser rather than \
         weakening the guard",
    );
    let mut ide_only_count = 0;
    for route in &routes {
        if ack.contains(&route.as_str()) {
            continue;
        }
        let tail = if route == "/" { "" } else { route.as_str() };
        let concrete = concretize(&format!("{ws}{nest}{tail}"));
        assert!(
            ["GET", "POST", "PUT", "DELETE", "PATCH"]
                .iter()
                .any(|m| classify(m, &concrete) == RouteRole::IdeOnly),
            "sub-route {route:?} (under {nest}) classifies FleetOk but is not on \
             the reviewed ack-list for {builder} — if it reads or writes the \
             working copy, mount it with `route_ide`; if it is genuinely \
             stateless, add it to the list with a reason.",
        );
        ide_only_count += 1;
    }
    assert!(
        ide_only_count >= min_ide_only,
        "{builder} has {ide_only_count} IdeOnly sub-routes, expected at least \
         {min_ide_only} — a pin was lost",
    );
    let route_set: std::collections::HashSet<&str> = routes.iter().map(|r| r.as_str()).collect();
    for entry in ack {
        assert!(
            route_set.contains(entry),
            "the ack-list for {builder} lists {entry:?} but no such route exists \
             — remove the stale entry."
        );
    }
}

/// Per-sub-route drift guard for the MIXED `build_app_routes()` builder —
/// the structural backstop the file previously lacked. `fully_fs_builder_*`
/// deliberately can't cover MIXED builders, and `every_workspace_mount_*`
/// only sees the `/apps` NEST (its one-segment probe matches the IdeOnly
/// `/apps/{pathb64}` pattern), so every app SUB-route was otherwise
/// classified by reviewer attention alone — exactly how publish/unpublish/
/// save-from-run wrote the working copy while classified FleetOk. This
/// parses `build_app_routes()` and asserts each sub-route is EITHER IdeOnly
/// OR on the explicit `APP_FLEET_OK` ack-list below, so a new app sub-route
/// fails CI unless someone classifies it on purpose.
#[test]
fn every_app_sub_route_is_classified() {
    let src = include_str!("router/workspace.rs");
    // App sub-routes intentionally FleetOk: served from the compile boundary
    // / S3 / Postgres, never the working copy. REVIEW before adding one — if
    // a handler reads OR writes the working copy / local state dir, it
    // belongs on `route_ide`, not here.
    const APP_FLEET_OK: &[&str] = &[
        "/",                              // list_apps (Postgres definitions)
        "/{pathb64}/displays",            // get_displays (SQL templates, no run)
        "/{pathb64}/data-cached",         // get_app_data_cached (boundary + S3)
        "/{pathb64}/charts/{chart_path}", // get_chart_image (local + S3 fallback)
    ];
    let ws = "/api/d9830be4-c6a4";
    let routes = route_paths_in_fn(src, "build_app_routes");
    let mut checked = 0;
    for route in &routes {
        if APP_FLEET_OK.contains(&route.as_str()) {
            continue; // intentional, reviewed FleetOk
        }
        let tail = if route == "/" { "" } else { route.as_str() };
        let concrete = concretize(&format!("{ws}/apps{tail}"));
        assert!(
            ["GET", "POST", "PUT", "DELETE", "PATCH"]
                .iter()
                .any(|m| classify(m, &concrete) == RouteRole::IdeOnly),
            "app sub-route {route:?} (under /apps) classifies FleetOk but is not in \
             APP_FLEET_OK — if it reads/writes the working copy, mount it with \
             `route_ide`; if it is genuinely stateless add it to APP_FLEET_OK. \
             (This is the publish/unpublish/save-from-run gap.)",
        );
        checked += 1;
    }
    assert!(
        checked >= 5,
        "parsed only {checked} app sub-routes — the parser likely broke; fix it \
         rather than weakening the guard",
    );
    // No stale acks: every APP_FLEET_OK entry must be a current sub-route.
    let route_set: std::collections::HashSet<&str> = routes.iter().map(|r| r.as_str()).collect();
    for ack in APP_FLEET_OK {
        assert!(
            route_set.contains(ack),
            "APP_FLEET_OK lists {ack:?} but build_app_routes has no such route — \
             remove the stale entry."
        );
    }
}

/// `router/public.rs` mounts through the declaring door, not the silent one.
///
/// This file used to carry 51 bare `.route(` calls. Nothing was wrong with the
/// roles — every one of them is genuinely FleetOk — but nothing SAID so, and an
/// undeclared route is indistinguishable from a route nobody thought about. It
/// also meant the type gate did not apply: `.route()` accepts a
/// `MethodRouter<S>` for any state, so a handler asking for
/// `WorkspaceManagerWorkingCopy` would have compiled and shipped as a route a
/// stateless replica cannot serve. `route_fleet` takes `MethodRouter<FleetState>`
/// and rejects it — measured, by adding such a handler and watching
/// `cargo check` fail with E0308.
///
/// What this actually guards is narrower than it first looks, and the narrower
/// claim is the true one. `RoleRouter` has no `.route()` method at all, so a
/// single bare mount does not fail this test — it fails to COMPILE, which is
/// better. Measured: putting `.route("/ready", ..)` back gives E0308-adjacent
/// breakage at `cargo check`, not a red test.
///
/// So the scan exists for the reversion the compiler would accept: someone
/// changing `build_public_routes` back to a plain `Router<AppState>`, at which
/// point every `.route(` compiles again and all 51 declarations vanish at once.
/// The count assertion is the half that earns its place — it is what notices
/// the file going quiet.
#[test]
fn the_public_router_declares_every_route_it_mounts() {
    let src = include_str!("router/public.rs");
    // `.expect`, not `unwrap_or(0)`: falling back to 0 widens the scan to the
    // whole file, and since the count is unchanged both assertions below stay
    // green — so the ONE failure this test's message claims to report ("the
    // parser broke, or this file moved") was the one it could not.
    let start = src
        .find("pub(super) fn build_public_routes")
        .expect("build_public_routes not found in router/public.rs — it moved or was renamed");
    let body = &src[start..];

    let bare: Vec<&str> = body
        .lines()
        .filter(|l| l.trim_start().starts_with(".route("))
        .collect();
    assert!(
        bare.is_empty(),
        "these mounts in router/public.rs use the undeclaring door:\n  {}\n\n\
         Use `.route_fleet(..)` — or `.route_ide(..)` if the handler genuinely \
         needs the working copy, in which case the compiler will tell you: a \
         working-copy extractor types the handler as `IdeState` and will not \
         build through the fleet door.",
        bare.join("\n  ")
    );

    let declared = body.matches(".route_fleet(").count() + body.matches(".route_ide(").count();
    // The file declares 51. A floor of 40 let eleven disappear unnoticed, which
    // is most of a surface — near enough to the real count to notice a subtree
    // going quiet, loose enough that adding or removing a handful does not make
    // this the test everyone edits without reading.
    assert!(
        declared >= 45,
        "only {declared} declared mounts found — the parser broke, or this file \
         moved. A guard that silently stops covering anything is the shape this \
         whole area exists to remove.",
    );
}

/// The Postgres-backed run-state siblings of the customer-app data plane.
///
/// This test used to also pin the four EXECUTION routes IdeOnly, on the
/// grounds that they "build a WorkspaceManager from the working copy and run
/// inline". They no longer do — see
/// `the_customer_app_data_plane_is_fleet_ok`, which now covers them. What
/// remains here is the half that was always Postgres-only, kept because
/// these routes live in `router/public.rs`, outside `build_workspace_routes`,
/// so the workspace-mount drift test cannot see them.
#[test]
fn custom_app_run_state_routes_are_fleet_ok() {
    let pid = "d9830be4-c6a4";
    // Postgres-backed run-state siblings are cross-process safe → FleetOk.
    // The shell-context bootstrap (SDK shell chrome) is likewise a pure
    // persisted-data read and must never get pinned to the singleton.
    let fleet_ok = [
        ("GET", format!("/api/projects/{pid}/procedures/runs/run-1")),
        (
            "POST",
            format!("/api/projects/{pid}/procedures/runs/run-1/cancel"),
        ),
        (
            "POST",
            format!("/api/projects/{pid}/agents/asks/run-1/cancel"),
        ),
        (
            "GET",
            format!("/api/projects/{pid}/agents/runs/run-1/events"),
        ),
        ("GET", format!("/api/projects/{pid}/shell-context")),
        ("GET", format!("/api/projects/{pid}/threads")),
        ("GET", format!("/api/projects/{pid}/threads/t-1")),
    ];
    for (method, path) in &fleet_ok {
        assert_eq!(
            classify(method, path),
            RouteRole::FleetOk,
            "custom-app run-state route {method} {path} must stay FleetOk \
             (reads/writes Postgres run state, no working copy)"
        );
    }
}

/// The whole customer-app data plane is FleetOk.
///
/// The SQL-executing half used to be IdeOnly, and the reason was recorded
/// here: they "build a connector from the FS-fallback config, empty on a
/// serve replica". That was true — `build_project_context` resolved a path
/// and built a fallback-config manager, never asking Postgres — and it meant
/// every request from every deployed custom app went through the one pod
/// holding a checkout, so an ide restart took the apps down with it.
///
/// It reads the compile boundary now, so `databases` comes from the promoted
/// revision. The remaining filesystem arms are gated on
/// `can_read_disk()` and return a retryable 503 with a lazy compile
/// enqueued, rather than compiling against an empty directory.
#[test]
fn the_customer_app_data_plane_is_fleet_ok() {
    let pid = "d9830be4-c6a4";
    let sem = format!("/api/projects/{pid}/semantic");
    let fleet_ok = [
        ("POST", format!("/api/projects/{pid}/query")),
        ("POST", format!("/api/projects/{pid}/semantic-query")),
        ("POST", format!("/api/projects/{pid}/agents/a-1/asks")),
        ("POST", format!("/api/projects/{pid}/procedures/p-1/runs")),
        ("POST", format!("{sem}/metric-tree/explain")),
        ("POST", format!("{sem}/metric-tree/opportunity")),
        ("POST", format!("{sem}/metric-tree/distribution")),
        ("POST", format!("{sem}/metric-tree/baseline")),
        ("POST", format!("{sem}/metric-tree/projection")),
        ("GET", format!("{sem}/world-model/measure-breakdown")),
        ("GET", format!("{sem}/world-model/instances")),
        // Pure ops, FleetOk all along.
        ("GET", format!("{sem}/metric-tree")),
        ("GET", format!("{sem}/metric-tree/revenue/sensitivity")),
        ("POST", format!("{sem}/metric-tree/predict")),
        ("GET", format!("{sem}/metric-tree/time-dimensions")),
        ("GET", format!("{sem}/world-model")),
    ];
    for (method, path) in &fleet_ok {
        assert_eq!(
            classify(method, path),
            RouteRole::FleetOk,
            "customer-app data-plane route {method} {path} must be FleetOk — \
             a custom app must not go down with the ide"
        );
    }
}

/// Oxy Functions execute in-process from the working copy
/// (`build_project_context` + `ctx.semantic` FS reads), so their invocation
/// route must be IdeOnly — a serve replica forwards it to the ide. Static
/// bundle assets are S3-backed and stay FleetOk.
#[test]
fn custom_app_function_route_is_ide_only() {
    assert_eq!(
        classify("POST", "/customer-apps/acme/hello-oxy/fn/post-je"),
        RouteRole::IdeOnly,
        "custom-app fn invocation must be IdeOnly (runs in-process from the working copy)"
    );
    // Static assets + index are served from S3 → any replica → FleetOk. The
    // 5-segment `.../fn/{name}` pattern must not capture these.
    assert_eq!(
        classify("GET", "/customer-apps/acme/hello-oxy/assets/main.js"),
        RouteRole::FleetOk,
        "custom-app static assets stay FleetOk"
    );
    assert_eq!(
        classify("GET", "/customer-apps/acme/hello-oxy"),
        RouteRole::FleetOk
    );
}

/// Path string of every `.route("PATH", ...)` mounted directly in
/// `fn {fn_name}` of `src`. Deliberately simple text parsing (no syntax
/// crate): the FS builders are flat lists of `.route(...)` calls.
fn route_paths_in_fn(src: &str, fn_name: &str) -> Vec<String> {
    let start = src
        .find(&format!("fn {fn_name}"))
        .unwrap_or_else(|| panic!("{fn_name} not found in workspace.rs"));
    let rest = &src[start..];
    // Body ends at the next top-level `fn ` (column 0).
    let end = rest[1..].find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    let mut out = Vec::new();
    for marker in MOUNT_MARKERS {
        for seg in rest[..end].split(marker).skip(1) {
            let Some(after_quote) = seg.trim_start().strip_prefix('"') else {
                continue;
            };
            let Some(close) = after_quote.find('"') else {
                continue;
            };
            out.push(after_quote[..close].to_string());
        }
    }
    out
}

/// Every way a route reaches the router. All of them take the path first,
/// so a parser needs the names and nothing else.
const MOUNT_MARKERS: &[&str] = &[
    ".route_ide(",
    ".route_fleet(",
    ".route_fleet_optional_working_copy(",
    ".route_split(",
    ".route_split_optional_working_copy(",
];

/// Replace `{name}` / `{*name}` pattern segments with a literal so a route
/// pattern becomes a concrete request path `classify` can match.
fn concretize(pattern: &str) -> String {
    let mut out = String::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if c == '{' {
            for d in chars.by_ref() {
                if d == '}' {
                    break;
                }
            }
            out.push('x');
        } else {
            out.push(c);
        }
    }
    out
}

/// Drift guard (hand-maintained — NOT router-introspecting). Every flat
/// route mounted by `build_git_routes()` + `build_data_repo_routes()`
/// (router/workspace.rs) touches the working copy / shells out to git, so
/// each MUST classify IdeOnly; a route that's FleetOk lands on a serve
/// replica with no working copy → 500. This test asserts the list below
/// classifies IdeOnly, so it catches a route that's in the list but mounted
/// on the wrong door. It does NOT see new routes added to the router — so a
/// new git route must be named in TWO places: its `route_ide` mount, and
/// this list. (Since the path table went away, `classify` reads what the
/// router declared, so router and manifest can no longer disagree — only
/// this hand-written list can fall behind the router.)
#[test]
fn manifest_covers_every_git_route() {
    let ws = "/api/d9830be4-c6a4";
    // (method, flat path) pairs straight from build_git_routes().
    let git_routes = [
        ("GET", format!("{ws}/branches")),
        ("DELETE", format!("{ws}/branches/main")),
        ("POST", format!("{ws}/switch-branch")),
        ("POST", format!("{ws}/pull-changes")),
        ("POST", format!("{ws}/fetch")),
        ("POST", format!("{ws}/push-changes")),
        ("POST", format!("{ws}/abort-rebase")),
        ("POST", format!("{ws}/continue-rebase")),
        ("POST", format!("{ws}/resolve-conflict-file")),
        ("POST", format!("{ws}/unresolve-conflict-file")),
        ("POST", format!("{ws}/resolve-conflict-with-content")),
        ("POST", format!("{ws}/force-push")),
        ("POST", format!("{ws}/discard-all")),
        ("GET", format!("{ws}/recent-commits")),
        ("GET", format!("{ws}/revision-info")),
        ("POST", format!("{ws}/reset-to-commit")),
    ];
    for (method, path) in &git_routes {
        assert_eq!(
            classify(method, path),
            RouteRole::IdeOnly,
            "git route {method} {path} must be IdeOnly (touches working copy)"
        );
    }
    // build_data_repo_routes(), nested under /repositories.
    let repo_routes = [
        ("GET", format!("{ws}/repositories")),
        ("POST", format!("{ws}/repositories")),
        ("DELETE", format!("{ws}/repositories/my-repo")),
        ("POST", format!("{ws}/repositories/my-repo/checkout")),
        ("GET", format!("{ws}/repositories/my-repo/diff")),
        ("POST", format!("{ws}/repositories/my-repo/commit")),
        ("GET", format!("{ws}/repositories/my-repo/files")),
        ("POST", format!("{ws}/repositories/github")),
    ];
    for (method, path) in &repo_routes {
        assert_eq!(
            classify(method, path),
            RouteRole::IdeOnly,
            "data-repo route {method} {path} must be IdeOnly (git working copy)"
        );
    }
    // `onboarding-readiness` moved out with the rest of the onboarding surface
    // — see `crates/api-onboarding/tests/route_roles.rs`.
}

/// Coverage guard for routes that read NODE-LOCAL state the compile
/// boundary does not materialise (git state, process-local BROADCASTER
/// SSE). A serve replica has no working copy and no in-process run owner,
/// so these degrade silently there ("Workspace directory not found", a
/// truncated stream) unless classified IdeOnly. Hand-maintained, same
/// limitation as `manifest_covers_every_git_route`: it catches a route
/// moved off `route_ide`, not a brand-new router route nobody listed —
/// that's the behavioral canary's job (see internal-docs/compile-boundary.md
/// "the role-classification canary"). Name a new state-touching route in TWO
/// places: its `route_ide` mount, and here.
#[test]
fn manifest_covers_state_touching_routes() {
    let ws = "/api/d9830be4-c6a4";
    let ide_only = [
        // git/working-copy state reads (workspace_root + detect_git_mode)
        ("GET", format!("{ws}/git-state")),
        ("GET", format!("{ws}/status")),
        // process-local BROADCASTER live SSE (legacy workflow/task streams)
        ("GET", format!("{ws}/events")),
        ("GET", format!("{ws}/events/lookup")),
        ("GET", format!("{ws}/events/sync")),
        ("GET", format!("{ws}/exported-charts/abc.png")),
        // modeling/airform — dbt projects on disk; ALL methods + the bare
        // list root are IdeOnly (regression for the POST-only-manifest gap).
        ("GET", format!("{ws}/modeling")),
        ("GET", format!("{ws}/modeling/myproj/lineage")),
        ("POST", format!("{ws}/modeling/myproj/run")),
        // Agentic run/exec surface — ide-pinned for tier 1 (subruns run
        // in-process where the analytics run drives and touch the FS).
        ("POST", format!("{ws}/analytics/runs")),
        ("GET", format!("{ws}/analytics/runs/r1/events")),
        ("GET", format!("{ws}/agentic-workflows/files")),
        ("GET", format!("{ws}/agentic-workflows/runs/r1/events")),
        ("GET", format!("{ws}/agentic-airway/runs/r1/events")),
    ];
    for (method, path) in &ide_only {
        assert_eq!(
            classify(method, path),
            RouteRole::IdeOnly,
            "state-touching route {method} {path} must be IdeOnly \
             (no working copy / process-local broadcaster on the serve fleet)"
        );
    }
    // Counter-guard: routes that LOOK similar but are cross-process safe
    // must stay FleetOk, or we needlessly pin the chat data plane to the
    // ide singleton. (The agentic run/exec surface — /analytics,
    // /agentic-workflows, /agentic-airway — is now ide-pinned for tier 1;
    // see the `ide_only` set above.)
    let fleet_ok = [
        ("GET", format!("{ws}/blocks")), // persisted Postgres read
        ("GET", format!("{ws}/world-model/cameras")),
        // parquet result cache — fleet-safe via the S3 read-through in
        // result_files::{store,get} (mirror on write, fetch on local miss).
        ("GET", format!("{ws}/results/files/file-123")),
        ("DELETE", format!("{ws}/results/files/file-123")),
    ];
    for (method, path) in &fleet_ok {
        assert_eq!(
            classify(method, path),
            RouteRole::FleetOk,
            "cross-process route {method} {path} must stay FleetOk"
        );
    }
}

// ── Stage 0b: router-derived completeness gate ─────────────────────────
//
// The FIRST test that derives its route set FROM the router source rather
// than a hand-maintained list. It parses every `.route()` / `.nest()` mount
// in `build_workspace_routes` (router/workspace.rs) and asserts each is
// EXPLICITLY classified — IdeOnly by its `route_ide` mount, or acknowledged
// FleetOk below. A new mount nobody classified fails CI here instead of
// silently defaulting to FleetOk and 404ing on a serve replica that has no
// working copy (the `/apps/source` outage class, oxygen-internal#2531).
//
// Scope/limits (honest): it guards the top-level mount surface. It does not
// introspect nested builders' sub-routes (axum exposes no route table) nor
// per-method gaps inside a builder — those stay guarded by the per-builder
// tests above + the behavioral canary (internal-docs/compile-boundary.md).
// `.merge(...)` mounts carry no path literal (git: see
// `manifest_covers_every_git_route`).

/// Workspace mounts whose DEFAULT is FleetOk: served statelessly (compile
/// boundary + Postgres + S3). Kept in sync with `build_workspace_routes` —
/// the test rejects stale entries.
///
/// A mount here may still be **MIXED** — `/apps`, `/tests` and
/// `/integrations` each have IdeOnly sub-routes. This list cannot express
/// that: `every_workspace_mount_is_classified` short-circuits on a match, so
/// an entry here means "do not ask about the mount", not "nothing under it
/// touches disk". The earlier wording claimed the latter and was false the
/// moment a sub-route was pinned.
///
/// **A mixed mount needs a per-sub-route guard.** See
/// `every_app_sub_route_is_classified` and the two beside it — without one,
/// every sub-route under the mount is classified by reviewer attention
/// alone, which is exactly how publish/unpublish came to write the working
/// copy while classified FleetOk.
///
/// REVIEW before adding one: if a handler under the mount reads local disk
/// and has no sub-route guard, it belongs on `route_ide`.

/// Extracts `(is_nest, path)` for every `.route("…")` / `.nest("…")` mount
/// in `body` (handles the multi-line `.route(\n  "…"` form).
fn parse_mounts(body: &str) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let nests: &[&str] = &[".nest(", ".nest_all(", ".nest_declared(", ".nest_typed("];
    for marker in MOUNT_MARKERS.iter().chain(nests) {
        let is_nest = nests.contains(marker);
        let mut hay = body;
        while let Some(i) = hay.find(marker) {
            let after = skip_ws_and_comments(&hay[i + marker.len()..]);
            if let Some(rest) = after.strip_prefix('"')
                && let Some(end) = rest.find('"')
            {
                out.push((is_nest, rest[..end].to_string()));
            }
            hay = &hay[i + marker.len()..];
        }
    }
    out
}

/// The source text of `fn <name>`, up to its closing brace.
fn fn_body<'a>(src: &'a str, name: &str) -> &'a str {
    let start = src
        .find(&format!("fn {name}"))
        .unwrap_or_else(|| panic!("{name} fn present"));
    let body = &src[start..];
    &body[..body.find("\n}\n").unwrap_or(body.len())]
}

/// Names of the builders `body` `.merge`s in, in source order.
///
/// Discovered rather than listed so a second merged builder is picked up
/// without anyone remembering to add it here. `.nest`ed builders are
/// deliberately not matched — see the note at the call site.
///
/// Takes the text up to the next `(`, which for a `.merge(some_router)` that is
/// not a call would run on to an unrelated paren — the `build_` prefix filter
/// is what contains that, so a non-call argument is skipped rather than
/// mis-parsed.
fn merged_builders(body: &str) -> Vec<&str> {
    let mut names = Vec::new();
    for (idx, _) in body.match_indices(".merge(") {
        let rest = &body[idx + ".merge(".len()..];
        let Some(end) = rest.find('(') else { continue };
        let name = rest[..end].trim();
        if name.starts_with("build_") && !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

#[test]
fn discovers_every_merged_builder() {
    let src = include_str!("router/workspace.rs");
    let merged = merged_builders(fn_body(src, "build_workspace_routes"));
    assert!(
        merged.contains(&"build_metric_tree_routes"),
        "the known merged builder must be discovered: {merged:?}"
    );
    // A `.nest`ed builder must NOT be picked up — its paths are relative to the
    // nest prefix, so classifying them here would use the wrong URI.
    assert!(
        !merged.iter().any(|n| *n == "build_thread_routes"),
        "nested builders must stay out: {merged:?}"
    );
}

/// Skip whitespace AND `//` comments before a mount's path literal.
///
/// Without this, `rustfmt` moving a comment *inside* the `.route(` call —
/// which is exactly what happened to `/oxy-access` — makes the mount
/// invisible to this test. That fails silently in the dangerous direction: an
/// unclassified FS-touching route would slip through the very check that
/// exists to catch it. (It surfaced as a bogus "stale acknowledgement",
/// which is the lucky case.)
fn skip_ws_and_comments(mut s: &str) -> &str {
    loop {
        s = s.trim_start();
        let Some(rest) = s.strip_prefix("//") else {
            return s;
        };
        s = rest.find('\n').map_or("", |i| &rest[i..]);
    }
}

/// The door that let a FleetOk route hold a `WorkspaceManager<WorkingCopy>` is
/// gone, and this is what stops it coming back.
///
/// It existed because a handler reading the compile boundary FIRST and the
/// working copy only on a miss still had to name `WorkingCopy` in its
/// signature, which `route_fleet` refuses. The count went 24 -> 17 -> 8 -> 6
/// -> 5 -> 4 -> 0 as those handlers moved to `ConfigManager` reads that own
/// both arms, and the last four turned out not to belong on the fleet at all:
/// the metric-tree runner parses the semantic model off the workspace root, so
/// `/scan` and `/{id}/explain` failed on a replica rather than degrading.
///
/// A new one would mean a route claiming FleetOk while its handler requires a
/// disk. The two honest answers are `WorkspaceManagerReadOnly` — whose slot
/// carries `Option<WorkingCopy>`, so the boundary-miss fallback still works on
/// a node that owns files — or `route_ide`.
#[test]
fn the_optional_working_copy_door_stays_shut() {
    let router = include_str!("router/role_router.rs");
    for gone in [
        "fn route_fleet_optional_working_copy",
        "fn route_split_optional_working_copy",
    ] {
        assert!(
            !router.contains(gone),
            "`{gone}` is back. It mounts a handler that requires a working copy \
             on a route declared FleetOk, which is a promise a stateless replica \
             cannot keep — the four routes it last held failed there with an IO \
             error naming a path the caller never wrote.\n\n\
             Use `WorkspaceManagerReadOnly` when the disk is a FALLBACK (its \
             slot is an `Option`, so a node with files still serves it), or \
             `route_ide` when the handler genuinely cannot run without one."
        );
    }

    let workspace = include_str!("router/workspace.rs");
    let workspace = &workspace[..workspace.find("#[cfg(test)]").unwrap_or(workspace.len())];
    for marker in [
        ".route_fleet_optional_working_copy(",
        ".route_split_optional_working_copy(",
    ] {
        assert!(
            !workspace.contains(marker),
            "`{marker}` is mounted again — see the message above."
        );
    }
}

/// Every mount in `build_workspace_routes` must be DECLARED. Before
/// `RoleRouter` this checked something weaker — that a mount was matched by
/// some pattern in a hand-written table, or listed in a 49-entry
/// acknowledgement of deliberate FleetOk mounts. Both are gone: a mount
/// carries its role now, so the only failure left is a mount the router
/// somehow serves without declaring one.
#[test]
fn every_workspace_mount_is_declared() {
    let src = include_str!("router/workspace.rs");
    // `build_workspace_routes` plus every builder it `.merge`s. The `.nest`ed
    // builders are deliberately excluded — their sub-routes carry no prefix at
    // this layer, so a path parsed out of them would be checked against the
    // wrong URI. A MERGED builder is the opposite case: its paths are absolute
    // and land in the tree verbatim, so leaving one out would hide those routes
    // from the very check that exists to catch an undeclared mount.
    //
    // So the merged builders are DISCOVERED, not listed: a hand-maintained list
    // has to be remembered when a second one is added, and nothing fails if it
    // isn't — the new routes just quietly stop being checked.
    let root = fn_body(src, "build_workspace_routes");
    let merged = merged_builders(root);
    assert!(
        !merged.is_empty(),
        "found no .merge(build_*) in build_workspace_routes — the router shape \
         changed; fix merged_builders"
    );
    let mut bodies = root.to_string();
    for name in &merged {
        bodies.push_str(fn_body(src, name));
    }
    let mounts = parse_mounts(&bodies);
    assert!(
        mounts.len() > 30,
        "parser found only {} mounts — the router shape changed; fix parse_mounts",
        mounts.len()
    );

    let declared = crate::server::router::route_declarations();
    for (_, path) in &mounts {
        let full = format!("/api/{{workspace_id}}{path}");
        assert!(
            declared.iter().any(|(_, p, _)| p.starts_with(&full)),
            "workspace mount {path:?} is served but declares no role. Mount it \
             through `RoleRouter` so it states one, or — if another crate owns \
             the routes and mounts them at this tree's root — go through \
             `merge_undeclared` with the reason.",
        );
    }
}

/// Every process that serves requests or drains work must declare its role.
///
/// The failure mode is the reason this is a source scan rather than a call
/// test: `current_process_role()` falls back to `Role::All` when nothing set
/// the `OnceLock`, and `All` is the value that means "this node owns the
/// workspace files". So omitting the call is silent — the process comes up and
/// claims a filesystem it does not have.
///
/// `oxy worker` did exactly that. Three separate reads take the wrong branch on
/// a standalone worker as a result: `OxyProjectContext::context_root` globs an
/// absent working copy instead of materialising the boundary (the "no databases
/// configured" failure its own comment names), the workspace middleware
/// publishes the `WorkingCopy` extension, and a missing root reads as
/// "materializing" rather than as a worker's normal state.
///
/// A new long-running entry point belongs in this list.
#[test]
fn every_long_running_entry_point_declares_its_role() {
    // A prefix, so `_with_default` counts: `oxy worker` declares `Worker` when
    // `OXY_ROLE` is unset, and matching the bare `()` form would have made this
    // guard fail for a call that is MORE correct than the one it was written
    // against.
    const INIT: &str = "init_process_role_from_env";
    for (command, src) in [
        ("oxy serve", include_str!("../cli/commands/serve.rs")),
        ("oxy worker", include_str!("../cli/commands/worker.rs")),
    ] {
        // Comments stripped first. `worker.rs` carries a NOTE block explaining
        // why a SECOND bare init must never come back, and that prose contains
        // this very literal — so without stripping, deleting the real call
        // would leave this guard green. A test satisfied by a comment about
        // the thing is not a test of the thing.
        let src = &strip_line_comments(src);
        assert!(
            src.contains(INIT),
            "`{command}` never calls `{INIT}`, so `current_process_role()` \
             answers `All` for it — the role that owns the workspace files. \
             Nothing fails loudly; the pod just starts describing itself wrong.",
        );
    }
}

/// Strip `//` comments so a source-grep guard cannot be satisfied by prose.
///
/// Deliberately crude — it does not understand strings or block comments —
/// which is fine for the two entry points it reads, and the failure direction
/// is safe: at worst it strips something it should not and a guard gets
/// stricter.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `oxy worker` must seed the role EXACTLY once, and with an explicit default.
///
/// The bug this pins actually shipped: main's #2822 added
/// `_with_default(Worker)` while this branch had already added a bare
/// `init_process_role_from_env()` below it. They do not conflict textually, so
/// the merge kept both — and the second is not idempotent. `PROCESS_ROLE` is a
/// `OnceLock` so the role stays `Worker`, but `set_process_owns_workspace_files`
/// is an `AtomicBool::store`, and the bare form resolves `Role::All` when
/// `OXY_ROLE` is unset, storing `true` over the `false` written moments before.
///
/// The pod then answers `process_can_compile() == false` (reads the `OnceLock`)
/// and `process_owns_workspace_files() == true` (reads the atomic) — the exact
/// split-brain the role manifest exists to prevent, and worse than either end
/// alone because the two disagree.
#[test]
fn oxy_worker_seeds_the_role_once_and_never_with_the_bare_default() {
    let src = strip_line_comments(include_str!("../cli/commands/worker.rs"));
    assert!(
        src.contains("init_process_role_from_env_with_default("),
        "`oxy worker` must declare its role explicitly; running the command IS \
         the declaration."
    );
    // The bare form, as a call. `_with_default(` does not match this because
    // the next char after the fn name is `_`, not `(`.
    assert!(
        !src.contains("init_process_role_from_env()"),
        "`oxy worker` calls the bare `init_process_role_from_env()` as well as \
         `_with_default`. That is not a harmless duplicate: the bare form \
         resolves `Role::All` when `OXY_ROLE` is unset, and its \
         `set_process_owns_workspace_files(true)` overwrites the `false` the \
         explicit call just stored — leaving the process claiming a workspace \
         working copy it does not have."
    );
}

/// The flag the omission above actually moves.
///
/// `role_owns_workspace_files` is what the extension publish gate and the
/// state-dir fallback read, so a worker declaring `All` is a worker that gets
/// handed a working copy it has no files for.
#[test]
fn a_worker_that_declares_itself_owns_no_workspace_files() {
    // SAFETY: nextest runs each test in its own process, so neither the env var
    // nor the `PROCESS_ROLE` OnceLock leaks into another test.
    unsafe { std::env::set_var("OXY_ROLE", "worker") };
    assert_eq!(init_process_role_from_env(), Role::Worker);
    assert!(
        !oxy::workspace_fs_probe::process_owns_workspace_files(),
        "a declared worker must not claim the workspace filesystem"
    );
}

/// And with `OXY_ROLE` unset, which is the deployment that was actually broken.
///
/// Adding the init call to `run_worker` only helped charts that already set the
/// variable — everywhere else the unset default is `Role::All`, the value that
/// claims the filesystem. Running `oxy worker` is the stronger statement, so
/// the command supplies its own default and an explicit value still wins.
#[test]
fn an_undeclared_worker_still_owns_no_workspace_files() {
    // SAFETY: nextest runs each test in its own process.
    unsafe { std::env::remove_var("OXY_ROLE") };
    assert_eq!(
        init_process_role_from_env_with_default(Role::Worker),
        Role::Worker,
        "the command it was launched as is the declaration"
    );
    assert!(!oxy::workspace_fs_probe::process_owns_workspace_files());
}

#[test]
fn airhouse_admin_routes_are_fleet_ok() {
    let ws = "3c6e0b8a-9c15-224a-8236-000000000001";
    for (method, path) in [
        ("GET", "/api/admin/airhouse".to_string()),
        (
            "POST",
            format!("/api/admin/workspaces/{ws}/airhouse/provision"),
        ),
    ] {
        assert_eq!(
            classify(method, &path),
            RouteRole::FleetOk,
            "{method} {path} is a Postgres-plus-HTTP read/write — pinning it to \
             the ide makes a tenant's warehouse surface die with the singleton"
        );
    }
}

#[test]
fn custom_app_health_is_fleet_ok() {
    assert_eq!(
        classify("GET", "/customer-apps/acme/command-center/health"),
        RouteRole::FleetOk,
        "the external liveness endpoint must stay FleetOk — a health check that \
         needs the singleton fails whenever the singleton restarts"
    );
    // Its neighbour stays pinned: a function invocation executes in-process
    // and reads the workspace. The `fn` segment is what splits them.
    assert_eq!(
        classify("POST", "/customer-apps/acme/command-center/fn/health"),
        RouteRole::IdeOnly
    );
}

#[test]
fn source_upload_is_fleet_ok() {
    let ws = "d9830be4-c6a4";
    assert_eq!(
        classify("POST", &format!("/api/{ws}/source-uploads/reports")),
        RouteRole::FleetOk,
        "an S3 write with no working-copy access must not need the ide"
    );

    // The neighbouring surface it deliberately does NOT live under.
    assert_eq!(
        classify("POST", &format!("/api/{ws}/agentic-airway/runs")),
        RouteRole::IdeOnly,
        "a live pipeline run still belongs on the ide — the carve-out is \
         the upload, not the surface"
    );
}

// ── Ported from origin/main's inline `mod tests` at the #2851 merge ──────
// This branch moved that module to this file; these are the cases it had that
// this file did not.
//
// Three of main's did NOT come across, and their absence is deliberate:
// `ide_down_degradable_routes`, `every_workspace_mount_is_classified` and its
// sibling pin `degrades_when_ide_unreachable` / `FLEET_OK_ACKNOWLEDGED`, both
// removed by 5f5c3bd15 — the degrade hook answered `false` for every route, so
// the arm it guarded was unreachable, and the mount census moved to
// `tests/routing/route_role_derivation.rs`, which derives its route set from the router
// instead of a hand-kept list. Re-adding them would pin behaviour that no
// longer exists.
//
// Two more did not come across, and this pair is a real disagreement rather
// than dead code. main asserts `POST /api/projects/{id}/query` and the
// metric-tree query routes are IdeOnly because they "build a connector from the
// FS-fallback config, empty on serve". That WAS true; this branch changed it —
// the data plane reads the compile boundary, so `databases` comes from the
// promoted revision and the surviving filesystem arms are gated on
// `can_read_disk()`. Pinning them IdeOnly again would put every deployed custom
// app back behind the single pod holding a checkout, which is exactly what
// `the_customer_app_data_plane_is_fleet_ok` above exists to prevent.
//
// The move is not finished, and the gap is measured rather than assumed:
// against the docker fleet, `POST /api/projects/<ws>/query {"database":"local"}`
// answers `DuckDB 'local': cannot resolve path '.db/'` on a replica and returns
// rows on the ide. A DuckDB `local` database still resolves to a node-local
// file that `can_read_disk()` does not cover, so it fails with an error instead
// of the retryable 503 the design intends. Recorded as finding #2 in
// `customer-apps-oxy-starter-fleet.flow.test.yml`.
//
// Two more did not come across, and this one is a real disagreement rather than
// dead code: `custom_app_execution_routes_are_ide_only` and
// `customer_app_metric_tree_query_routes_are_ide_only` assert that
// `POST /api/projects/{id}/query` and the metric-tree query routes are IdeOnly,
// on the grounds that they "build a connector from the FS-fallback config,
// empty on serve". That WAS true. This branch changed it — the data plane reads
// the compile boundary, so `databases` comes from the promoted revision, and
// the surviving filesystem arms are gated on `can_read_disk()`. Pinning them
// IdeOnly again would put every deployed custom app back behind the single pod
// holding a checkout, which is what `the_customer_app_data_plane_is_fleet_ok`
// (above) exists to prevent.
//
// The move is not finished, and the gap is measured, not assumed: against the
// docker fleet, `POST /api/projects/<ws>/query {"database":"local"}` answers
// `DuckDB 'local': cannot resolve path '.db/'` on a replica and returns rows on
// the ide. A DuckDB `local` database still resolves to a node-local file that
// `can_read_disk()` does not cover, so the failure is an error rather than the
// retryable 503 the design intends. Recorded in
// `customer-apps-oxy-starter-fleet.flow.test.yml`'s header as finding #2.
//
// The nine that did come across are verbatim, so a failure means a real
// disagreement about a route's pod rather than a transcription slip.

/// The per-org OLTP surface reads Postgres and nothing else — no workspace
/// working copy, no `.git`, no state dir — so every route must serve from
/// any replica.
///
/// There is no manifest entry for these (FleetOk is the default); this test
/// exists because the *default* is what protects them. A future broad
/// `IdeOnly` pattern — an `/api/{workspace_id}/{*rest}` say, where
/// `workspace_id` would happily match the literal `oltp` — would silently
/// pin a tenant's live business data behind the singleton, and the symptom
/// is an HA outage rather than a compile error.
#[test]
fn tenant_data_plane_routes_are_fleet_ok() {
    let org = "8f14e45f-ceea-167a-5a36";
    // A workspace id, not the org one — the airhouse route below takes a
    // workspace, and reusing `org` for it read as a copy-paste slip even
    // though classification is by path shape.
    let ws = "3c6e0b8a-9c15-224a-8236";
    for (method, path) in [
        // Data plane: status + read-only ERD for the caller's own org.
        ("GET", "/api/oltp/me/connection".to_string()),
        ("GET", "/api/oltp/me/erd".to_string()),
        // Staff plane: provisioning and credential issue. Authorization is
        // a `route_layer` (Action::PlatformOltp); placement is orthogonal
        // to it, and neither touches node-local disk.
        ("GET", format!("/api/admin/orgs/{org}/oltp")),
        ("POST", format!("/api/admin/orgs/{org}/oltp/provision")),
        ("POST", format!("/api/admin/orgs/{org}/oltp/credentials")),
        // Added with the admin UI. Both are org-keyed writes against
        // Postgres and a provider API — no node-local state — so the
        // default is right; this list is what pins it.
        ("POST", format!("/api/admin/orgs/{org}/oltp/visibility")),
        ("DELETE", format!("/api/admin/orgs/{org}/oltp")),
        // Releasing one app's store. Same shape as its two neighbours —
        // Postgres plus the provider API — and pinned for the same reason:
        // the default is what protects it, so the list is what keeps a
        // future broad `IdeOnly` pattern from quietly capturing it.
        (
            "POST",
            format!("/api/admin/orgs/{org}/oltp/deprovision-writer"),
        ),
        // Airhouse's admin surface, same shape: Postgres plus an HTTP
        // client, no node-local state. Pinning, not a fix — the default is
        // already right, and this is what keeps a future broad `IdeOnly`
        // pattern from quietly capturing it.
        ("GET", "/api/admin/airhouse".to_string()),
        (
            "POST",
            format!("/api/admin/workspaces/{ws}/airhouse/provision"),
        ),
    ] {
        assert_eq!(
            classify(method, &path),
            RouteRole::FleetOk,
            "{method} {path} is a Postgres-only read/write — pinning it to \
                 the ide makes a tenant's OLTP surface die with the singleton"
        );
    }
}

/// Both routers must mount the OLTP surface.
///
/// This guards a failure that already happened once and is recorded in the
/// design doc: `/oltp/me/connection` was mounted only in
/// `build_local_protected_routes`, but cloud reaches these through
/// `build_global_routes`. A missing route is not a 404 here — the SPA
/// catch-all answers unknown paths with **HTTP 200 and `index.html`**, so
/// the settings panel read it as "not provisioned" and the bug looked like
/// a data problem. Source-level, like
/// `fully_fs_builder_routes_classify_ide_only`, because the failure is a
/// missing line rather than a wrong value.
#[test]
fn the_oltp_router_is_mounted_in_both_global_and_protected_routers() {
    for (file, src) in [
        ("router/global.rs", include_str!("router/global.rs")),
        ("router/protected.rs", include_str!("router/protected.rs")),
    ] {
        assert!(
            // The `.merge` prefix is part of the needle on purpose: matching
            // the bare path would let a doc comment — or a commented-out
            // mount, which is exactly how this broke the first time — satisfy
            // the test. Both doors count: this branch mounts another crate's
            // router through `merge_undeclared`, which carries the reason the
            // merge has no declaration to hang a role on.
            src.contains(".merge(oxy_oltp::api::router")
                || src.contains(".merge_undeclared(\n            oxy_oltp::api::router")
                || src.contains(".merge_undeclared(oxy_oltp::api::router"),
            "{file} must merge oxy_oltp::api::router — a route missing from \
                 one of the two routers answers 200 + index.html, not 404"
        );
    }
}

#[test]
fn airway_config_is_fleet_ok() {
    assert_eq!(
        classify("GET", "/api/admin/airway/config"),
        RouteRole::FleetOk,
        "airway config CRUD is Postgres-only and must stay HA"
    );
}

#[test]
fn airway_config_writes_are_fleet_ok() {
    let ws = "d9830be4-c6a4-4c1c-9c1e-000000000001";
    for (method, path) in [
        ("PUT", "/api/admin/airway/config/toast".to_string()),
        ("DELETE", "/api/admin/airway/config/toast".to_string()),
        (
            "PUT",
            format!("/api/admin/airway/config/toast/workspaces/{ws}"),
        ),
        (
            "DELETE",
            format!("/api/admin/airway/config/toast/workspaces/{ws}"),
        ),
    ] {
        assert_eq!(
            classify(method, &path),
            RouteRole::FleetOk,
            "{method} {path} is Postgres-only CRUD (stage 3 Task 2) and must stay HA"
        );
    }
}

/// The preview reads workspace *content*, which is usually the tell for an
/// IdeOnly route — but it reads that content from the compile boundary, so
/// it is FleetOk like the rest of the airway-config surface. Pinned
/// because the tempting misreading (`airway_run` resolves specs from the
/// working copy, so this must too) would pin a Postgres-only admin surface
/// to the singleton for no reason — and that reading is now doubly wrong,
/// since `airway_run` reads the compiled row as well.
#[test]
fn airway_policy_preview_is_fleet_ok() {
    assert_eq!(
        classify("GET", "/api/admin/airway/config/toast/preview"),
        RouteRole::FleetOk,
        "the preview reads compiled `airway_pipelines` rows scoped to each workspace's \
             promoted revision — never the workspace filesystem — so it serves from any replica"
    );
}

/// The operational tier's three routes are Postgres-only and must stay
/// HA. The read one is the interesting case: it also reads a process-local
/// `OnceLock` to report what the answering replica installed. That is
/// deliberately NOT a reason to pin it to the ide — a singleton would
/// report exactly one process's state too, just less available. The
/// honesty comes from `installed_scope` in the payload, not from the route
/// class.
#[test]
fn airway_deployment_config_is_fleet_ok() {
    for method in ["GET", "PUT", "DELETE"] {
        assert_eq!(
            classify(method, "/api/admin/airway/deployment-config"),
            RouteRole::FleetOk,
            "{method} /api/admin/airway/deployment-config is Postgres-only and must stay HA"
        );
    }
}

/// `deployment-config` must not be swallowed by the per-source-kind
/// pattern next door. Both live under `/api/admin/airway/`, and
/// `/config/{source_kind}` is one segment away from matching it — a
/// regression here would classify the deployment routes by whatever rule
/// the policy routes carry, silently.
#[test]
fn deployment_config_does_not_collide_with_a_source_kind_path() {
    assert_eq!(
        classify("PUT", "/api/admin/airway/config/deployment-config"),
        RouteRole::FleetOk,
        "control: this is the source-kind route, matched by its own pattern"
    );
    assert_eq!(
        classify("GET", "/api/admin/airway/deployment-config"),
        RouteRole::FleetOk
    );
}
