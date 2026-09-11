//! A router where a route's pod is a type, not a note.
//!
//! `role_manifest.rs` held the same fact in a second place — a hand-written path
//! table, matched by string. Being second, it could disagree with the routes, and
//! a route nobody added to it defaulted to `FleetOk`: a handler that reads the
//! working copy shipped fleet-served and 404ed on every replica. That is how
//! `GET /apps/source/{file}` shipped broken.
//!
//! Every route goes through [`RoleRouter::route_ide`] or
//! [`RoleRouter::route_fleet`], and the two take differently-typed handlers.
//! `WorkspaceManagerWorkingCopy` resolves only from `IdeState`, so a handler that
//! asks for a working copy produces a `MethodRouter<IdeState>` and **will not
//! compile** through the fleet door. The role stops being a claim about the
//! handler and becomes something the compiler has checked.
//!
//! Mounting is `route_service`, so each route carries its own state and the
//! tree above it stays one `Router<AppState>` — no splitting the router in two.
//! `route_service` does not merge, so a path's verbs go in one call; a path
//! whose verbs sit on opposite sides uses [`RoleRouter::route_split`].
//!
//! The declarations accumulated here are what `role_manifest::classify` reads,
//! so the routing table is a by-product of building the server rather than a
//! description of it.

use axum::Router;
use axum::routing::MethodRouter;
use oxy_shared::fleet_role::{RouteRole, RouteRoleDecl};

use super::{AppState, FleetState, IdeState};

/// `(method, path, role)`. `method` is `"*"` unless a path serves two verbs at
/// different roles — `/app-integrations` lists on GET from Postgres but upserts
/// on POST into the working copy.
pub type Decl = (&'static str, String, RouteRole);

/// A `Router<AppState>` that types each route by the pod that may serve it.
pub struct RoleRouter {
    app_state: AppState,
    router: Router<AppState>,
    decls: Vec<Decl>,
    undeclared: Vec<&'static str>,
}

impl RoleRouter {
    pub fn new(app_state: AppState) -> Self {
        Self {
            app_state,
            router: Router::new(),
            decls: Vec::new(),
            undeclared: Vec::new(),
        }
    }

    fn ide_state(&self) -> IdeState {
        IdeState(self.app_state.clone())
    }

    fn fleet_state(&self) -> FleetState {
        FleetState(self.app_state.clone())
    }

    /// A route that needs the workspace working copy, `.git`, or the local state
    /// dir. Reverse-proxied to the ide singleton when it lands on a replica.
    pub fn route_ide(self, path: &str, method_router: MethodRouter<IdeState>) -> Self {
        self.mount(RouteRole::IdeOnly, "*", path, method_router)
    }

    /// A route that reads only persisted data — Postgres, S3, the compile
    /// boundary. Runs on any pod, and cannot reach for a working copy: the
    /// handler would not compile here.
    pub fn route_fleet(self, path: &str, method_router: MethodRouter<FleetState>) -> Self {
        self.mount(RouteRole::FleetOk, "*", path, method_router)
    }

    fn mount<S>(
        mut self,
        role: RouteRole,
        method: &'static str,
        path: &str,
        method_router: MethodRouter<S>,
    ) -> Self
    where
        S: Clone + Send + Sync + 'static,
        Self: StateFor<S>,
    {
        self.decls.push((method, path.to_string(), role));
        let state = <Self as StateFor<S>>::state(&self);
        self.router = self
            .router
            .route_service(path, method_router.with_state(state));
        self
    }

    /// One path whose verbs sit on opposite sides. `/databases` is the shape:
    /// the list degrades without a working copy and the launcher calls it on
    /// every page load, while creating one writes `config.yml`.
    pub fn route_split(
        mut self,
        path: &str,
        ide_method: &'static str,
        ide: MethodRouter<IdeState>,
        fleet_method: &'static str,
        fleet: MethodRouter<FleetState>,
    ) -> Self {
        self.decls
            .push((ide_method, path.to_string(), RouteRole::IdeOnly));
        self.decls
            .push((fleet_method, path.to_string(), RouteRole::FleetOk));
        let (ide_state, fleet_state) = (self.ide_state(), self.fleet_state());
        let merged = ide
            .with_state(ide_state)
            .merge(fleet.with_state(fleet_state));
        self.router = self.router.route_service(path, merged);
        self
    }

    pub fn nest(mut self, prefix: &str, other: RoleRouter) -> Self {
        let (router, decls, undeclared) = other.into_parts();
        self.decls
            .extend(decls.into_iter().map(|(m, p, r)| (m, join(prefix, &p), r)));
        self.undeclared.extend(undeclared);
        self.router = self.router.nest(prefix, router);
        self
    }

    /// Nest a router whose roles are declared beside its routes rather than at
    /// this mount. `agentic-http` is the reason this exists: it sits below
    /// `oxy-app` in the layering, so `oxy-app` cannot see its handlers, and used
    /// to cover all of them with one wildcard plus per-route carve-outs. The
    /// admin console uses it the same way — a FleetOk wildcard plus the one
    /// route that writes a working copy (`admin::router_roles`).
    pub fn nest_declared(
        mut self,
        prefix: &str,
        router: Router<AppState>,
        decls: &[RouteRoleDecl],
    ) -> Self {
        self.decls.extend(
            decls
                .iter()
                .map(|d| (d.method, join(prefix, d.path), d.role)),
        );
        self.router = self.router.nest(prefix, router);
        self
    }

    /// Nest a subtree that types its own routes, under one role. The role is
    /// declared here because only the mounting site knows the prefix; the
    /// subtree's own state is what stops a handler in it reaching for a working
    /// copy it should not have.
    pub fn nest_typed<S>(
        mut self,
        prefix: &str,
        role: RouteRole,
        router: Router<S>,
        state: S,
    ) -> Self
    where
        S: Clone + Send + Sync + 'static,
    {
        self.decls.push(("*", format!("{prefix}/{{*rest}}"), role));
        self.decls.push(("*", prefix.to_string(), role));
        self.router = self
            .router
            .nest_service(prefix, router.with_state(state).into_service());
        self
    }

    /// Nest a subtree under one role, without naming its routes. For routers
    /// whose whole surface shares a role and whose handlers need no working
    /// copy; a route added there inherits the role, which is what the wildcard
    /// means.
    pub fn nest_all(
        mut self,
        prefix: &str,
        role: RouteRole,
        router: Router<AppState>,
        _why: &'static str,
    ) -> Self {
        self.decls.push(("*", format!("{prefix}/{{*rest}}"), role));
        self.decls.push(("*", prefix.to_string(), role));
        self.router = self.router.nest(prefix, router);
        self
    }

    pub fn merge(mut self, other: RoleRouter) -> Self {
        let (router, decls, undeclared) = other.into_parts();
        self.decls.extend(decls);
        self.undeclared.extend(undeclared);
        self.router = self.router.merge(router);
        self
    }

    /// Merge routes mounted at this tree's root by another crate, which brings
    /// its own declarations.
    ///
    /// [`merge_undeclared`](Self::merge_undeclared) is the right call when a
    /// crate's whole surface is Postgres-only and the FleetOk default is the
    /// truth. This is for the one that is not: `oxy-api-onboarding` clones a
    /// repository and scaffolds `config.yml` onto node-local disk, and it sits
    /// outside this crate, so neither the type gate nor `route_ide` can reach
    /// it. Undeclared, it would take the default and let a stateless replica
    /// clone into a checkout it does not own.
    ///
    /// The paths are absolute, not prefix-joined: a merge has no prefix, so the
    /// crate states the shape it actually mounts at.
    pub fn merge_declared(mut self, router: Router<AppState>, decls: &[RouteRoleDecl]) -> Self {
        self.decls
            .extend(decls.iter().map(|d| (d.method, d.path.to_string(), d.role)));
        self.router = self.router.merge(router);
        self
    }

    /// Merge routes mounted at this tree's root by another crate. A merge has
    /// no prefix to hang a declaration on, so these carry no declaration and
    /// fall to `classify`'s default — the one hole this type cannot close. Each
    /// says why, and `undeclared_merges` counts them.
    pub fn merge_undeclared(mut self, router: Router<AppState>, why: &'static str) -> Self {
        self.undeclared.push(why);
        self.router = self.router.merge(router);
        self
    }

    /// Apply any `Router` operation that carries no routes — `layer`,
    /// `route_layer`, `fallback`. Declarations pass through untouched.
    pub fn map_router(mut self, f: impl FnOnce(Router<AppState>) -> Router<AppState>) -> Self {
        self.router = f(self.router);
        self
    }

    pub fn into_parts(self) -> (Router<AppState>, Vec<Decl>, Vec<&'static str>) {
        (self.router, self.decls, self.undeclared)
    }

    pub fn into_router(self) -> Router<AppState> {
        self.router
    }
}

/// Which state value a mount of type `S` finishes its handlers with.
pub trait StateFor<S> {
    fn state(&self) -> S;
}

impl StateFor<IdeState> for RoleRouter {
    fn state(&self) -> IdeState {
        self.ide_state()
    }
}

impl StateFor<FleetState> for RoleRouter {
    fn state(&self) -> FleetState {
        self.fleet_state()
    }
}

fn join(prefix: &str, path: &str) -> String {
    if path.is_empty() || path == "/" {
        prefix.to_string()
    } else {
        format!("{prefix}{path}")
    }
}
