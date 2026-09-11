use super::peer::{is_loopback_peer, reachable};
use super::*;

#[test]
fn unset_env_disables_the_endpoint() {
    assert!(parse_dev_login_emails(None, None).is_empty());
    assert!(parse_dev_login_emails(Some(""), None).is_empty());
    assert!(parse_dev_login_emails(Some("  ,  "), None).is_empty());
}

#[test]
fn entries_are_trimmed_lowercased_and_validated() {
    assert_eq!(
        parse_dev_login_emails(
            Some(" Dev@Oxy.local , not-an-email ,member@oxy.local"),
            None
        ),
        vec!["dev@oxy.local".to_string(), "member@oxy.local".to_string()]
    );
}

#[test]
fn no_requested_email_uses_the_first_entry() {
    let allowed = parse_dev_login_emails(Some("dev@oxy.local,member@oxy.local"), None);
    assert_eq!(
        resolve_dev_login_email(&allowed, None),
        Some("dev@oxy.local".to_string())
    );
    assert_eq!(
        resolve_dev_login_email(&allowed, Some("   ")),
        Some("dev@oxy.local".to_string())
    );
}

#[test]
fn requested_email_matches_case_insensitively() {
    let allowed = parse_dev_login_emails(Some("dev@oxy.local,member@oxy.local"), None);
    assert_eq!(
        resolve_dev_login_email(&allowed, Some("MEMBER@oxy.local")),
        Some("member@oxy.local".to_string())
    );
}

#[test]
fn unlisted_email_is_refused_rather_than_falling_back() {
    let allowed = parse_dev_login_emails(Some("dev@oxy.local"), None);
    assert_eq!(
        resolve_dev_login_email(&allowed, Some("owner@oxy.tech")),
        None
    );
}

#[test]
fn empty_allowlist_never_resolves() {
    assert_eq!(resolve_dev_login_email(&[], None), None);
    assert_eq!(resolve_dev_login_email(&[], Some("dev@oxy.local")), None);
}

// The gate itself, decided before any database work — so these cover the
// refusals the endpoint promises without needing Postgres. The DB-side ones
// (401 for a deleted user, 409 for an unseeded persona) sit past
// `establish_connection` and are not reachable from a unit test.

#[test]
fn disabled_is_a_404_not_a_403() {
    // 404 rather than 403 on purpose: a server without the env var must
    // not admit the route exists.
    assert_eq!(
        resolve_or_refuse(&[], None, None, true),
        Err(StatusCode::NOT_FOUND)
    );
    assert_eq!(
        resolve_or_refuse(&[], Some("dev@oxy.local"), None, true),
        Err(StatusCode::NOT_FOUND)
    );
}

// Where the allow-list comes from. The release-build cases are the reason
// `resolve_source` takes `debug_build` instead of reading `cfg!` inline:
// a debug test run could not otherwise exercise them, and they are exactly
// the ones that would be a production auth bypass if they regressed.

fn roster(v: &str) -> Option<String> {
    Some(v.to_string())
}

#[test]
fn explicit_var_wins_in_every_build() {
    for debug_build in [true, false] {
        assert_eq!(
            resolve_source(
                roster("dev@oxy.local"),
                roster("staff@oxy.tech"),
                debug_build
            ),
            (roster("dev@oxy.local"), Some(DevLoginSource::Explicit))
        );
    }
}

#[test]
fn debug_build_falls_back_to_the_staff_roster() {
    assert_eq!(
        resolve_source(None, roster("staff@oxy.tech"), true),
        (roster("staff@oxy.tech"), Some(DevLoginSource::GlobalAdmins))
    );
}

#[test]
fn the_pre_rename_spelling_is_not_a_rung() {
    // OXY_APP_ADMINS was removed from every reader. Nothing here consults
    // it, so a .env that still uses only the old name resolves to nothing
    // — `custom_apps_auth::warn_on_removed_legacy_admins_env` is what tells
    // the operator, rather than a silent grant from a var we no longer read.
    assert_eq!(resolve_source(None, None, true), (None, None));
}

#[test]
fn release_build_never_falls_back_to_the_roster() {
    // The whole point. OXY_GLOBAL_ADMINS is set on every real deployment,
    // so a release binary honoring it would mint Global-Admin sessions to
    // anyone who can reach the server — including, via kubectl
    // port-forward, callers who present as loopback.
    let resolved = resolve_source(None, roster("staff@oxy.tech"), false);
    assert_eq!(resolved, (None, None));
    assert!(parse_dev_login_emails(resolved.0.as_deref(), resolved.1).is_empty());
}

#[test]
fn set_but_empty_disables_even_on_a_debug_build() {
    // OXY_DEV_LOGIN_EMAILS= is the off switch that doesn't require
    // unsetting the staff roster the rest of the app needs.
    let (raw, source) = resolve_source(Some(String::new()), roster("staff@oxy.tech"), true);
    assert!(parse_dev_login_emails(raw.as_deref(), source).is_empty());
}

#[test]
fn nothing_set_is_disabled_in_both_builds() {
    assert_eq!(resolve_source(None, None, true), (None, None));
    assert_eq!(resolve_source(None, None, false), (None, None));
}

// The whole config, personas included — what the LazyLock actually holds.

fn persona_emails() -> Vec<&'static str> {
    vec![
        "maya.nguyen@acme.test",
        "amara.larsson@acme.test",
        "app-operator@oxy.local",
        "oliver.okafor@acme.test",
    ]
}

#[test]
fn the_inferred_list_is_the_roster_plus_the_personas() {
    let config = build_config(None, roster("Staff@oxy.tech"), true);
    assert_eq!(config.source, Some(DevLoginSource::GlobalAdmins));
    // Roster first, so a bare request is still staff.
    assert_eq!(
        config.emails.first().map(String::as_str),
        Some("staff@oxy.tech")
    );
    for email in persona_emails() {
        assert!(config.emails.iter().any(|e| e == email), "{email}");
    }
    assert_eq!(config.roster, vec!["staff@oxy.tech".to_string()]);
}

#[test]
fn an_explicit_list_replaces_the_fallback_personas_included() {
    let config = build_config(roster("dev@oxy.local"), roster("staff@oxy.tech"), true);
    assert_eq!(config.source, Some(DevLoginSource::Explicit));
    assert_eq!(config.emails, vec!["dev@oxy.local".to_string()]);
    // `as=staff` still names the roster on a debug build — and must then be
    // on the explicit list like any other address.
    assert_eq!(config.roster, vec!["staff@oxy.tech".to_string()]);
}

#[test]
fn a_release_build_reads_neither_the_roster_nor_the_personas() {
    let config = build_config(None, roster("staff@oxy.tech"), false);
    assert!(config.emails.is_empty());
    assert_eq!(config.source, None);
    assert!(
        config.roster.is_empty(),
        "as=staff must not read the roster"
    );

    let explicit = build_config(roster("dev@oxy.local"), roster("staff@oxy.tech"), false);
    assert_eq!(explicit.emails, vec!["dev@oxy.local".to_string()]);
    assert!(explicit.roster.is_empty());
}

#[test]
fn the_off_switch_still_turns_personas_off() {
    let config = build_config(Some(String::new()), roster("staff@oxy.tech"), true);
    assert!(config.emails.is_empty());
    assert_eq!(config.source, None);
}

#[test]
fn no_roster_at_all_stays_disabled() {
    // Personas widen an inferred list; they never conjure one.
    let config = build_config(None, None, true);
    assert!(config.emails.is_empty());
    assert_eq!(config.source, None);
}

#[test]
fn a_roster_that_parses_to_nothing_stays_disabled() {
    // Set but unusable (no TLD) is the same as unset — before personas this
    // produced an empty list and a 404, and it still must.
    let config = build_config(None, roster("me@oxy"), true);
    assert!(config.roster.is_empty());
    assert!(config.emails.is_empty());
    assert_eq!(config.source, None);
}

// Persona requests through the whole pre-database pipeline.

fn request(email: Option<&str>, persona: Option<&str>) -> DevLoginRequest {
    DevLoginRequest {
        email: email.map(str::to_string),
        persona: persona.map(str::to_string),
    }
}

fn status_of(
    config: &DevLoginConfig,
    req: &DevLoginRequest,
    loopback: bool,
) -> Result<String, StatusCode> {
    resolve_request(config, req, loopback)
        .map(|(_, email)| email)
        .map_err(|refusal| refusal.status())
}

#[test]
fn a_persona_signs_in_through_the_inferred_list_on_box() {
    let config = build_config(None, roster("staff@oxy.tech"), true);
    assert_eq!(
        status_of(&config, &request(None, Some("member")), true),
        Ok("amara.larsson@acme.test".into())
    );
    assert_eq!(
        status_of(&config, &request(None, Some("staff")), true),
        Ok("staff@oxy.tech".into())
    );
}

#[test]
fn the_404_gate_runs_before_any_persona_error() {
    // Off-box, a bad persona must still look like a missing route — a 400
    // listing the valid names would advertise the bypass.
    let inferred = build_config(None, roster("staff@oxy.tech"), true);
    for req in [
        request(None, Some("bogus")),
        request(Some("a@b.test"), Some("owner")),
    ] {
        assert_eq!(
            status_of(&inferred, &req, false),
            Err(StatusCode::NOT_FOUND)
        );
    }
    let disabled = build_config(None, None, true);
    assert_eq!(
        status_of(&disabled, &request(None, Some("bogus")), true),
        Err(StatusCode::NOT_FOUND)
    );
}

#[test]
fn persona_errors_past_the_gate() {
    let config = build_config(None, roster("staff@oxy.tech"), true);
    assert_eq!(
        status_of(&config, &request(None, Some("bogus")), true),
        Err(StatusCode::BAD_REQUEST)
    );
    assert_eq!(
        status_of(
            &config,
            &request(Some("staff@oxy.tech"), Some("owner")),
            true
        ),
        Err(StatusCode::BAD_REQUEST)
    );
}

#[test]
fn a_persona_outside_an_explicit_list_is_a_403() {
    // A persona is shorthand, not a grant: the typed list decides.
    let config = build_config(roster("dev@oxy.local"), roster("staff@oxy.tech"), true);
    assert_eq!(
        status_of(&config, &request(None, Some("owner")), false),
        Err(StatusCode::FORBIDDEN)
    );
    let listed = build_config(roster("maya.nguyen@acme.test"), None, false);
    assert_eq!(
        status_of(&listed, &request(None, Some("owner")), false),
        Ok("maya.nguyen@acme.test".into())
    );
}

#[test]
fn staff_on_a_release_build_is_a_409() {
    let config = build_config(roster("staff@oxy.tech"), roster("staff@oxy.tech"), false);
    assert_eq!(
        status_of(&config, &request(None, Some("staff")), true),
        Err(StatusCode::CONFLICT)
    );
}

// Who may USE the list, as opposed to where it came from. `serve` binds
// 0.0.0.0 by default, so an inferred roster that answered a LAN peer would
// vend staff sessions to a coffee-shop network with nothing configured.

#[test]
fn an_inferred_roster_is_refused_off_box() {
    let allowed = parse_dev_login_emails(Some("staff@oxy.tech"), None);
    let source = DevLoginSource::GlobalAdmins;
    // 404, not 403 — from off-box the endpoint must not admit it exists.
    assert_eq!(
        resolve_or_refuse(&allowed, None, Some(source), false),
        Err(StatusCode::NOT_FOUND),
        "{source:?} must not be served to a remote peer"
    );
    assert_eq!(
        resolve_or_refuse(&allowed, None, Some(source), true),
        Ok("staff@oxy.tech".into()),
        "{source:?} must still work on the box itself"
    );
}

#[test]
fn an_explicit_list_is_served_to_any_peer() {
    // The escape hatch: containers, remote dev boxes and CI need this, and
    // somebody deliberately typed the list.
    let allowed = parse_dev_login_emails(Some("dev@oxy.local"), None);
    assert_eq!(
        resolve_or_refuse(&allowed, None, Some(DevLoginSource::Explicit), false),
        Ok("dev@oxy.local".into())
    );
}

/// `/auth/config` must answer the same question the endpoint answers, or
/// the 404 leaks through the neighbouring public route. Drives the real
/// `reachable`, which `dev_login_reachable_by` is a thin wrapper over —
/// only the address parsing is the test's own.
fn reachable_by(enabled: bool, loopback_only: bool, peer: Option<&str>) -> bool {
    reachable(
        enabled,
        loopback_only,
        peer.map(|addr| addr.parse().unwrap()),
    )
}

#[test]
fn config_hides_a_loopback_only_bypass_from_off_box_callers() {
    // The off-box caller gets a 404 from the endpoint; the config it hits
    // one request earlier must not contradict that.
    assert!(!reachable_by(true, true, Some("192.168.1.20:5000")));
    assert!(reachable_by(true, true, Some("127.0.0.1:5000")));
    // An explicit allow-list is a deliberate act, so it is advertised to
    // whoever can reach it — that's the container/CI escape hatch.
    assert!(reachable_by(true, false, Some("192.168.1.20:5000")));
    // Off entirely ⇒ never advertised, from anywhere.
    assert!(!reachable_by(false, false, Some("127.0.0.1:5000")));
}

#[test]
fn unknown_peer_fails_closed() {
    // No connect-info (a listener without `into_make_service_with_connect_info`,
    // or a router-level test) must read as "not loopback", never as "trusted".
    assert!(!reachable_by(true, true, None));
    assert!(reachable_by(true, false, None));
    assert_eq!(
        resolve_or_refuse(
            &parse_dev_login_emails(Some("dev@oxy.local"), None),
            None,
            Some(DevLoginSource::GlobalAdmins),
            PeerAddr(None).is_loopback(),
        ),
        Err(StatusCode::NOT_FOUND)
    );
}

#[test]
fn loopback_covers_ipv4_mapped_ipv6() {
    // ::ffff:127.0.0.1 is the same machine; Ipv6Addr::is_loopback alone
    // says otherwise, which would break the fallback on a dual-stack bind.
    for addr in ["127.0.0.1:1", "[::1]:1", "[::ffff:127.0.0.1]:1"] {
        assert!(
            is_loopback_peer(addr.parse().unwrap()),
            "{addr} should count as loopback"
        );
    }
    for addr in ["192.168.1.20:1", "10.0.0.5:1", "[2001:db8::1]:1"] {
        assert!(
            !is_loopback_peer(addr.parse().unwrap()),
            "{addr} must not count as loopback"
        );
    }
}

#[test]
fn unlisted_email_is_a_403_never_a_silent_fallback() {
    let allowed = parse_dev_login_emails(Some("dev@oxy.local"), None);
    assert_eq!(
        resolve_or_refuse(
            &allowed,
            Some("owner@oxy.tech"),
            Some(DevLoginSource::Explicit),
            true
        ),
        Err(StatusCode::FORBIDDEN)
    );
    assert_eq!(
        resolve_or_refuse(&allowed, None, Some(DevLoginSource::Explicit), true),
        Ok("dev@oxy.local".into())
    );
}
