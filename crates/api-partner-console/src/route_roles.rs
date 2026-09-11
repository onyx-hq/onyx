//! Which pod may serve this crate's routes, where one may not take the default.

use oxy_shared::fleet_role::{RouteRole, RouteRoleDecl};

/// The routes in [`crate::routes`] that may not take the FleetOk default.
///
/// Stated here because `oxy-app` owns `RoleRouter` and cannot see these
/// handlers — the same reason `oxy-api-onboarding` carries `route_roles()`.
/// Paths are absolute: `oxy-server` merges this crate at the protected-tree
/// root, so there is no prefix to join.
///
/// Creating a client org creates its `Default` workspace, a working copy
/// scaffolded onto node-local disk, so a stateless replica must not answer it.
/// The org list on the same path's GET is a Postgres read and stays FleetOk.
pub fn route_roles() -> &'static [RouteRoleDecl] {
    &[RouteRoleDecl {
        method: "POST",
        path: "/partners/{partner_org_id}/orgs",
        role: RouteRole::IdeOnly,
    }]
}

#[cfg(test)]
mod tests {
    use super::route_roles;
    use oxy_app::server::role_manifest::{
        RouteRole, classify, install_route_declarations_for_tests_with,
    };

    const PARTNER: &str = "33333333-3333-3333-3333-333333333333";

    /// Installed the way `oxy-server` does, then asked through the same
    /// `classify` the request path uses — so a wrong prefix fails here rather
    /// than silently falling to the FleetOk default at runtime. The one
    /// installing test in this binary: the registry is a process-wide once-cell.
    #[test]
    fn creating_a_client_org_reaches_the_ide_and_nothing_else_does() {
        let extra = route_roles()
            .iter()
            .map(|d| (d.method, d.path.to_string(), d.role))
            .collect();
        install_route_declarations_for_tests_with(extra);

        let orgs = format!("/api/partners/{PARTNER}/orgs");
        assert_eq!(
            classify("POST", &orgs),
            RouteRole::IdeOnly,
            "creating a client org scaffolds its Default workspace onto disk"
        );
        // The declaration names its verb, so it must not swallow the path's
        // other verbs — or its neighbours.
        for (method, path) in [
            ("GET", orgs.clone()),
            ("PATCH", format!("{orgs}/{PARTNER}")),
        ] {
            assert_eq!(
                classify(method, &path),
                RouteRole::FleetOk,
                "{method} {path} reads or writes only Postgres"
            );
        }
    }
}
