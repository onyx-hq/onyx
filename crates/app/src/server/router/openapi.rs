//! The OpenAPI router used by Swagger UI. Kept separate from the runtime
//! router because its route set is curated (not every handler is exposed).

use axum::body::Body;
use axum::http::Request;
use oxy::config::constants::DEFAULT_API_KEY_HEADER;
use sentry::integrations::tower::NewSentryLayer;
use tower::ServiceBuilder;
use utoipa::openapi::ExternalDocs;
use utoipa::openapi::security::{
    ApiKey as OApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityRequirement, SecurityScheme,
};
use utoipa::openapi::server::Server;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::api::{
    agent, api_keys, app, data, database, healthcheck, organizations, projects, run, thread, user,
    workspaces,
};

use super::{IdeState, build_cors_layer};

pub async fn openapi_router() -> OpenApiRouter<IdeState> {
    OpenApiRouter::new()
        .routes(routes!(healthcheck::health_check))
        .routes(routes!(healthcheck::readiness_check))
        .routes(routes!(healthcheck::liveness_check))
        .routes(routes!(healthcheck::version_info))
        .routes(routes!(agent::get_agents))
        .routes(routes!(api_keys::create_api_key))
        .routes(routes!(api_keys::list_api_keys))
        .routes(routes!(api_keys::get_api_key))
        .routes(routes!(api_keys::delete_api_key))
        .routes(routes!(app::list_apps))
        .routes(routes!(app::get_app_result))
        .routes(routes!(app::get_chart_image))
        .routes(routes!(workspaces::get_workspace))
        .routes(routes!(workspaces::get_workspace_branches))
        .routes(routes!(run::get_automation_runs))
        .routes(routes!(run::create_automation_run))
        .routes(routes!(run::cancel_automation_run))
        .routes(routes!(run::delete_automation_run))
        .routes(routes!(run::bulk_delete_automation_runs))
        .routes(routes!(run::automation_events))
        .routes(routes!(run::automation_events_sync))
        .routes(routes!(run::get_blocks))
        .routes(routes!(thread::get_threads))
        .routes(routes!(thread::get_thread))
        .routes(routes!(thread::create_thread))
        .routes(routes!(thread::delete_thread))
        .routes(routes!(thread::delete_all_threads))
        .routes(routes!(thread::stop_thread))
        .routes(routes!(thread::bulk_delete_threads))
        .routes(routes!(thread::get_logs))
        // Workflow + automation routes have been retired; the new workflow
        // surface lives under `/agentic-workflows` (see agentic-http).
        // ── the data plane ────────────────────────────────────────────────
        //
        // Added for `oxyc schema`: these are the endpoints an agent needs to
        // construct a correct request for, which is a different set from the
        // ones the web app happens to call. Deliberately NOT web-UI surfaces —
        // an agent never uploads a logo or reorders a list.
        .routes(routes!(data::execute_sql_query))
        .routes(routes!(projects::query::run_query))
        .routes(routes!(projects::semantic_query::run_semantic_query))
        // Turning "the customer" into the ids every other endpoint wants.
        .routes(routes!(user::get_current_user_public))
        .routes(routes!(organizations::list_orgs))
        .routes(routes!(workspaces::list_workspaces))
        // Database routes
        .routes(routes!(database::create_database_config))
        .routes(routes!(database::test_database_connection))
        .layer(build_cors_layer())
        .layer(ServiceBuilder::new().layer(NewSentryLayer::<Request<Body>>::new_from_top()))
}

/// Markdown rendered in the Swagger UI header, and carried as the spec's
/// `info.description`. Documents both the HTTP API and the CLI tools
/// (`oxyc login`, `oxyc api`) that consume it; the body lives in `apidoc.md` so
/// the long markdown stays out of the code.
const APIDOC_DESCRIPTION: &str = include_str!("../../cli/commands/apidoc.md");

/// The finished OpenAPI document: [`openapi_router`]'s operations plus the
/// title, description, security schemes and server prefix that make it a
/// usable spec rather than a bare path list.
///
/// Two consumers, deliberately one builder: `oxy serve` hands it to Swagger UI
/// at `/apidoc`, and `oxyc openapi` prints it offline. A caller who can
/// only reach the binary gets exactly the document the running server serves.
///
/// Scope: `openapi_router`'s route set is **curated**, not the whole surface —
/// it is where the request/response *schemas* live. `oxyc routes` is the
/// complete endpoint list.
pub async fn build_openapi_doc() -> utoipa::openapi::OpenApi {
    let mut doc = openapi_router().await.into_openapi();

    doc.info.title = "Oxy API".to_string();
    doc.info.description = Some(APIDOC_DESCRIPTION.to_string());
    doc.info.contact = None;
    doc.info.license = None;

    let mut external_docs = ExternalDocs::new("https://oxygen-hq.com/docs");
    external_docs.description = Some("Oxy documentation".to_string());
    doc.external_docs = Some(external_docs);

    let mut components = doc.components.take().unwrap_or_default();
    // API key scheme — for service-to-service calls (X-API-Key header).
    components.security_schemes.insert(
        "ApiKey".to_string(),
        SecurityScheme::ApiKey(OApiKey::Header(ApiKeyValue::new(
            DEFAULT_API_KEY_HEADER.to_string(),
        ))),
    );
    // Bearer scheme — the JWT issued by `oxyc login` (and returned by the
    // magic-link flow). Pass via `Authorization: Bearer <token>`.
    components.security_schemes.insert(
        "BearerAuth".to_string(),
        SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
    );
    doc.components = Some(components);

    // Endpoints accept either scheme; clients only need to supply one.
    doc.security = Some(vec![
        SecurityRequirement::new("ApiKey", Vec::<String>::new()),
        SecurityRequirement::new("BearerAuth", Vec::<String>::new()),
    ]);
    doc.servers = Some(vec![Server::new("/api")]);
    doc
}

#[cfg(test)]
mod openapi_coverage_tests {
    use super::build_openapi_doc;

    /// The data-plane operations `oxyc schema` exists to serve.
    ///
    /// A green build proves nothing here: `routes!(handler)` compiles fine
    /// whether or not the handler carries a `#[utoipa::path]`, and a handler
    /// whose annotation was dropped simply vanishes from the document. This is
    /// the only thing that would notice.
    ///
    /// These are the endpoints an AGENT needs a schema for — SQL, semantic
    /// query, and the two lookups that turn a customer into the ids everything
    /// else takes. Web-UI-only surfaces are deliberately not in this list and
    /// should not be added to it.
    const AGENT_PATHS: &[(&str, &str)] = &[
        ("/{workspace_id}/sql/query", "post"),
        ("/projects/{project_id}/query", "post"),
        ("/projects/{project_id}/semantic-query", "post"),
        ("/user", "get"),
        ("/orgs", "get"),
        ("/orgs/{org_id}/workspaces", "get"),
    ];

    /// The operation for `method`, or `None` — read off the typed `PathItem`
    /// rather than by grepping serialized JSON, which is how the first version
    /// of this test managed to pass on `rendered.contains("get")` matching the
    /// word "get" anywhere in a description.
    fn operation<'a>(
        item: &'a utoipa::openapi::path::PathItem,
        method: &str,
    ) -> Option<&'a utoipa::openapi::path::Operation> {
        match method {
            "get" => item.get.as_ref(),
            "post" => item.post.as_ref(),
            "put" => item.put.as_ref(),
            "patch" => item.patch.as_ref(),
            "delete" => item.delete.as_ref(),
            other => panic!("unhandled method in AGENT_PATHS: {other}"),
        }
    }

    /// Every operation on a path item, paired with its method name.
    fn operations(
        item: &utoipa::openapi::path::PathItem,
    ) -> Vec<(&'static str, &utoipa::openapi::path::Operation)> {
        [
            ("get", item.get.as_ref()),
            ("put", item.put.as_ref()),
            ("post", item.post.as_ref()),
            ("delete", item.delete.as_ref()),
            ("options", item.options.as_ref()),
            ("head", item.head.as_ref()),
            ("patch", item.patch.as_ref()),
            ("trace", item.trace.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, op)| op.map(|o| (name, o)))
        .collect()
    }

    #[tokio::test]
    async fn the_agent_data_plane_is_documented() {
        let doc = build_openapi_doc().await;
        for (path, method) in AGENT_PATHS {
            let item = doc.paths.paths.get(*path).unwrap_or_else(|| {
                panic!(
                    "{path} is missing from the OpenAPI document — `oxyc schema {path}` \
                     would answer 'not documented'. Either the #[utoipa::path] annotation \
                     was dropped from its handler, or the routes!(..) line was."
                )
            });
            assert!(
                operation(item, method).is_some(),
                "{path} is documented but carries no {method} operation"
            );
        }
    }

    /// A schema with no properties is worse than none: it reads as
    /// "documented, and the body is empty". `SQLParams` is the one an agent
    /// copies first.
    ///
    /// Resolved through the operation's own `$ref` rather than by grepping the
    /// whole components blob — the earlier version asserted only that the
    /// strings "sql" and "database" appeared *somewhere* in the document, which
    /// they do in half a dozen unrelated descriptions.
    #[tokio::test]
    async fn the_sql_request_body_carries_its_fields() {
        use utoipa::openapi::{RefOr, Schema};

        let doc = build_openapi_doc().await;
        let item = doc
            .paths
            .paths
            .get("/{workspace_id}/sql/query")
            .expect("the SQL endpoint is documented");
        let body = operation(item, "post")
            .and_then(|op| op.request_body.as_ref())
            .expect("the SQL endpoint declares a request body");

        let reference = body
            .content
            .get("application/json")
            .and_then(|c| match &c.schema {
                Some(RefOr::Ref(r)) => Some(r.ref_location.clone()),
                _ => None,
            })
            .expect("the request body is a $ref into components");
        let name = reference
            .rsplit('/')
            .next()
            .expect("a $ref ends in a component name")
            .to_string();

        let schema = doc
            .components
            .as_ref()
            .and_then(|c| c.schemas.get(&name))
            .unwrap_or_else(|| panic!("{name} is referenced but not in components/schemas"));

        let RefOr::T(Schema::Object(object)) = schema else {
            panic!("{name} did not resolve to an object schema");
        };
        for field in ["sql", "database"] {
            assert!(
                object.properties.contains_key(field),
                "{name} lost its `{field}` property — an agent copying this body would omit it"
            );
        }
        assert!(
            object.required.iter().any(|r| r == "sql"),
            "{name} no longer marks `sql` required"
        );
    }

    /// Every `security(..)` name an operation cites must be a scheme this
    /// document actually registers.
    ///
    /// Six annotations shipped citing `"Bearer"` while the registered scheme is
    /// `"BearerAuth"` — a dangling reference that generates a valid-looking
    /// document in which no client can work out how to authenticate. Nothing
    /// fails on it: utoipa does not resolve the name, and the JSON is still
    /// well-formed.
    #[tokio::test]
    async fn every_security_requirement_names_a_registered_scheme() {
        let doc = build_openapi_doc().await;
        let registered: Vec<String> = doc
            .components
            .as_ref()
            .map(|c| c.security_schemes.keys().cloned().collect())
            .unwrap_or_default();
        assert!(
            !registered.is_empty(),
            "the document registers no security schemes at all"
        );

        let mut checked = 0usize;
        for (path, item) in &doc.paths.paths {
            for (method, op) in operations(item) {
                for requirement in op.security.iter().flatten() {
                    // `SecurityRequirement`'s map is private, so the scheme
                    // names are read back off its serialized form — which is
                    // also exactly what a client sees.
                    let as_json: serde_json::Value =
                        serde_json::to_value(requirement).expect("serialize security requirement");
                    let Some(map) = as_json.as_object() else {
                        continue;
                    };
                    for name in map.keys() {
                        checked += 1;
                        assert!(
                            registered.contains(name),
                            "{method} {path} cites security scheme {name:?}, which is not \
                             registered. Registered: {registered:?}"
                        );
                    }
                }
            }
        }
        // A loop that inspected nothing would pass silently, which is exactly
        // how the dangling `"Bearer"` survived six annotations in the first
        // place.
        assert!(
            checked > 5,
            "only {checked} security requirements inspected — the walk found almost nothing"
        );
    }

    /// Two Rust types in this crate are both called `SemanticQueryResponse`,
    /// and utoipa names a component after the ident alone — so without an
    /// explicit `#[schema(as = ...)]` on each they collapse into one schema and
    /// whichever registers last wins, handing `oxyc schema` the wrong body
    /// shape for the other endpoint.
    ///
    /// Checked by SHAPE rather than by name, so renaming either component
    /// keeps the test meaningful: the SQL response is an untagged enum (no
    /// `properties`), the semantic one is an object with `columns` and `rows`.
    #[tokio::test]
    async fn the_two_query_response_schemas_stayed_distinct() {
        use utoipa::openapi::{RefOr, Schema};

        let doc = build_openapi_doc().await;
        let components = doc.components.as_ref().expect("components");

        let object_with = |name: &str| -> bool {
            matches!(
                components.schemas.get(name),
                Some(RefOr::T(Schema::Object(o))) if o.properties.contains_key("columns")
            )
        };

        assert!(
            object_with("SemanticQueryResult"),
            "SemanticQueryResult is missing or is not the {{columns, rows}} object — the two \
             SemanticQueryResponse types have probably collided again"
        );
        assert!(
            components.schemas.contains_key("SqlQueryResponse"),
            "SqlQueryResponse is missing — the SQL endpoint's response schema was lost"
        );
        assert!(
            !object_with("SqlQueryResponse"),
            "SqlQueryResponse now looks like the semantic {{columns, rows}} object, so the two \
             collided"
        );
    }
}
