//! Who is emitting: the OpenTelemetry [`Resource`] every exported span and log
//! record carries.
//!
//! HyperDX groups by `service.name`, so the split fleet gets one service per
//! role — `oxy-serve`, `oxy-ide`, `oxy-worker` — and a single-process box is
//! plain `oxy`. The role is also stamped as its own attribute (`oxy.role`) so a
//! query can still say "every role" without string-prefix matching. Anything
//! the operator sets through the standard `OTEL_SERVICE_NAME` /
//! `OTEL_RESOURCE_ATTRIBUTES` variables wins over what is derived here: the
//! SDK's own env detector reads those, and [`build`] only fills the gaps.

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;

/// Resource attribute carrying the fleet role verbatim (`ide` / `serve` /
/// `worker` / `all`).
pub const ROLE_ATTR: &str = "oxy.role";

/// Resolve the fleet role this process runs as, from the two things known
/// before the CLI has even parsed: an explicit `OXY_ROLE` and the subcommand.
///
/// Mirrors `role_manifest::init_process_role_from_env_with_default` without
/// reaching into `oxy-app` — exact matches, no trimming, so the two never
/// disagree about what `OXY_ROLE=" ide "` means: an explicit `OXY_ROLE` wins; otherwise `oxy worker`
/// is a worker and `oxy serve` / `oxy start` are the all-in-one default. Any
/// other command (`oxy run`, `oxy validate`) has no role.
pub fn role_hint(subcommand: Option<&str>, oxy_role: Option<&str>) -> Option<&'static str> {
    match oxy_role {
        Some("ide") => return Some("ide"),
        Some("serve") => return Some("serve"),
        Some("worker") => return Some("worker"),
        Some("all") => return Some("all"),
        _ => {}
    }
    match subcommand {
        Some("worker") => Some("worker"),
        Some("serve") | Some("start") => Some("all"),
        _ => None,
    }
}

/// The `service.name` a role maps to. `all` and "no role" are both `oxy`: a
/// one-process deployment is the product, not a tier of it.
pub fn service_name_for_role(role: Option<&str>) -> String {
    match role {
        Some("ide") => "oxy-ide".to_string(),
        Some("serve") => "oxy-serve".to_string(),
        Some("worker") => "oxy-worker".to_string(),
        _ => "oxy".to_string(),
    }
}

/// `true` when the operator has already named the service through the
/// standard variables, in which case the derived name must not override it.
pub fn operator_named_service(
    otel_service_name: Option<&str>,
    otel_resource_attributes: Option<&str>,
) -> bool {
    otel_service_name.is_some_and(|s| !s.trim().is_empty())
        || operator_set_attribute(otel_resource_attributes, "service.name")
}

/// `true` when `OTEL_RESOURCE_ATTRIBUTES` already carries `key`. Anything
/// derived here is appended *after* the SDK's env detector, and later wins —
/// so a derived value must yield to an explicit one, not override it.
pub fn operator_set_attribute(otel_resource_attributes: Option<&str>, key: &str) -> bool {
    otel_resource_attributes
        .map(|attrs| {
            attrs.split(',').any(|kv| {
                kv.trim()
                    .strip_prefix(key)
                    .is_some_and(|rest| rest.starts_with('='))
            })
        })
        .unwrap_or(false)
}

/// The deployment environment, read the same way `sentry_config` reads it so
/// the two systems agree on what "prod" is called. `None` when nothing is set —
/// the attribute is then simply absent rather than guessed.
fn deployment_environment() -> Option<String> {
    [
        "OXY_ENVIRONMENT",
        "SENTRY_ENVIRONMENT",
        "ENVIRONMENT",
        "ENV",
    ]
    .iter()
    .find_map(|var| std::env::var(var).ok())
    .map(|v| v.trim().to_string())
    .filter(|v| !v.is_empty())
}

/// Build the process resource: SDK + env detectors, then the derived
/// `service.name` (only when the operator did not set one), the crate version,
/// the fleet role, the environment, and the pod / host name.
pub fn build(role: Option<&str>) -> Resource {
    let mut builder = Resource::builder();

    let operator_named = operator_named_service(
        std::env::var("OTEL_SERVICE_NAME").ok().as_deref(),
        std::env::var("OTEL_RESOURCE_ATTRIBUTES").ok().as_deref(),
    );
    if !operator_named {
        builder = builder.with_service_name(service_name_for_role(role));
    }

    let mut attrs = vec![KeyValue::new("service.version", env!("CARGO_PKG_VERSION"))];
    if let Some(role) = role {
        attrs.push(KeyValue::new(ROLE_ATTR, role.to_string()));
    }
    let resource_attributes = std::env::var("OTEL_RESOURCE_ATTRIBUTES").ok();
    let operator_set_environment =
        operator_set_attribute(resource_attributes.as_deref(), "deployment.environment")
            || operator_set_attribute(
                resource_attributes.as_deref(),
                "deployment.environment.name",
            );
    if let Some(env_name) = deployment_environment().filter(|_| !operator_set_environment) {
        // Semconv renamed `deployment.environment` → `deployment.environment.name`
        // in 2024; HyperDX and most dashboards still read the old key. Both,
        // until the UIs catch up — it is one short string per resource.
        attrs.push(KeyValue::new(
            "deployment.environment.name",
            env_name.clone(),
        ));
        attrs.push(KeyValue::new("deployment.environment", env_name));
    }
    // Kubernetes sets HOSTNAME to the pod name. The collector's k8sattributes
    // processor adds the namespace / deployment on top; this is the join key
    // that survives even when that processor is not configured.
    if let Some(host) = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()) {
        attrs.push(KeyValue::new("service.instance.id", host.clone()));
        attrs.push(KeyValue::new("host.name", host));
    }

    builder.with_attributes(attrs).build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_role_wins_over_subcommand() {
        assert_eq!(role_hint(Some("serve"), Some("ide")), Some("ide"));
        assert_eq!(role_hint(Some("worker"), Some("serve")), Some("serve"));
        // Exact match, like role_manifest: a padded value is not a role.
        assert_eq!(role_hint(None, Some(" worker ")), None);
    }

    #[test]
    fn subcommand_supplies_the_default_role() {
        assert_eq!(role_hint(Some("worker"), None), Some("worker"));
        assert_eq!(role_hint(Some("serve"), None), Some("all"));
        assert_eq!(role_hint(Some("start"), None), Some("all"));
        assert_eq!(role_hint(Some("publish"), None), None);
        assert_eq!(role_hint(None, None), None);
        // An unrecognised OXY_ROLE falls through to the command's default,
        // exactly as role_manifest does.
        assert_eq!(role_hint(Some("worker"), Some("banana")), Some("worker"));
    }

    #[test]
    fn service_names_follow_the_fleet_roles() {
        assert_eq!(service_name_for_role(Some("ide")), "oxy-ide");
        assert_eq!(service_name_for_role(Some("serve")), "oxy-serve");
        assert_eq!(service_name_for_role(Some("worker")), "oxy-worker");
        assert_eq!(service_name_for_role(Some("all")), "oxy");
        assert_eq!(service_name_for_role(None), "oxy");
    }

    #[test]
    fn operator_naming_is_detected_in_either_variable() {
        assert!(operator_named_service(Some("my-oxy"), None));
        assert!(!operator_named_service(Some("  "), None));
        assert!(operator_named_service(
            None,
            Some("deployment.environment=prod, service.name=oxy-eu")
        ));
        assert!(!operator_named_service(
            None,
            Some("deployment.environment=prod")
        ));
        assert!(!operator_named_service(None, None));
    }

    #[test]
    fn an_explicit_resource_attribute_is_detected_by_key() {
        let attrs = Some("deployment.environment=prod, service.name=oxy-eu");
        assert!(operator_set_attribute(attrs, "deployment.environment"));
        assert!(!operator_set_attribute(
            attrs,
            "deployment.environment.name"
        ));
        assert!(operator_set_attribute(attrs, "service.name"));
        assert!(!operator_set_attribute(attrs, "service"));
        assert!(!operator_set_attribute(None, "service.name"));
    }

    #[test]
    fn resource_carries_version_and_role() {
        let resource = build(Some("serve"));
        let get = |k: &'static str| resource.get(&opentelemetry::Key::from_static_str(k));
        assert_eq!(
            get("service.version").map(|v| v.to_string()),
            Some(env!("CARGO_PKG_VERSION").to_string())
        );
        assert_eq!(get(ROLE_ATTR).map(|v| v.to_string()), Some("serve".into()));
        // Without an operator override the derived name lands (the test
        // process may inherit OTEL_SERVICE_NAME from a shell; only assert the
        // derived value when it does not).
        if std::env::var("OTEL_SERVICE_NAME").is_err()
            && std::env::var("OTEL_RESOURCE_ATTRIBUTES").is_err()
        {
            assert_eq!(
                get("service.name").map(|v| v.to_string()),
                Some("oxy-serve".into())
            );
        }
    }
}
