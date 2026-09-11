//! Named sign-in identities — `/api/auth/dev-login?as=<persona>`.
//!
//! A persona is **shorthand for an email, never a grant**. It resolves to an
//! address and then the ordinary allow-list check runs, so it can only reach an
//! identity the allow-list already admits. What makes the seeded ones reachable
//! with zero configuration is [`inferred_allow_list`]: the roster fallback
//! (debug builds, loopback callers — the parent module's two guards) carries
//! them alongside `OXY_GLOBAL_ADMINS`. An explicit `OXY_DEV_LOGIN_EMAILS`
//! replaces that list wholesale, personas included.
//!
//! The emails mirror what `oxy seed` creates (`seed_partners.rs`,
//! `seed_platform_grants.rs`); `seeded_personas_match_the_seed` pins them, so a
//! reshuffled seed fails a test instead of 409-ing a browser run.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};

use super::GLOBAL_ADMINS_ENV;

/// What has to exist before a persona counts as seeded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Provisioning {
    /// Minted on first use, like an email sign-in: the staff roster is the
    /// operator's own list, so there is no seed step to have missed.
    OnFirstUse,
    /// The user row must exist — a platform grant, with no org of its own.
    UserRow,
    /// The user row **and** an org membership must exist.
    OrgMember,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Persona {
    pub(super) name: &'static str,
    /// `None` ⇒ the first `OXY_GLOBAL_ADMINS` entry.
    email: Option<&'static str>,
    pub(super) provisioning: Provisioning,
}

pub(super) const PERSONAS: &[Persona] = &[
    Persona {
        name: "staff",
        email: None,
        provisioning: Provisioning::OnFirstUse,
    },
    // Acme person 0 — the org Owner.
    Persona {
        name: "owner",
        email: Some("maya.nguyen@acme.test"),
        provisioning: Provisioning::OrgMember,
    },
    // Acme person 6 — an org Member with no partner access and no team. Person 1
    // is a Member too, but also a partner operator, so not a *plain* member.
    Persona {
        name: "member",
        email: Some("amara.larsson@acme.test"),
        provisioning: Provisioning::OrgMember,
    },
    // The unscoped App Operator grant: staff standing, no org membership.
    Persona {
        name: "operator",
        email: Some("app-operator@oxy.local"),
        provisioning: Provisioning::UserRow,
    },
    // Acme person 1 — a Member holding one of Acme's partner-operator bindings.
    Persona {
        name: "partner",
        email: Some("oliver.okafor@acme.test"),
        provisioning: Provisioning::OrgMember,
    },
];

impl Persona {
    /// The address this persona signs in as.
    ///
    /// `roster` is empty on a release build, so `as=staff` cannot read
    /// `OXY_GLOBAL_ADMINS` there — the same line the allow-list fallback holds.
    fn email(&self, roster: &[String]) -> Result<String, DevLoginRefusal> {
        match self.email {
            Some(email) => Ok(email.to_string()),
            None => roster.first().cloned().ok_or_else(|| {
                DevLoginRefusal::with_reason(
                    StatusCode::CONFLICT,
                    format!(
                        "persona `{}` is the first {GLOBAL_ADMINS_ENV} entry, and none is \
                         set (read on debug builds only)",
                        self.name
                    ),
                )
            }),
        }
    }

    /// The refusal for a persona whose rows `oxy seed` has not created.
    pub(super) fn not_seeded(&self, email: &str) -> DevLoginRefusal {
        DevLoginRefusal::with_reason(
            StatusCode::CONFLICT,
            format!(
                "persona `{}` ({email}) is not seeded — run `just up` (or `oxy seed`)",
                self.name
            ),
        )
    }
}

/// Whether a sign-in with no user row may mint one. A persona that names a
/// seeded identity must not: a fresh row would land on onboarding and read as
/// "the seed is broken" instead of "the seed never ran".
pub(super) fn may_mint(persona: Option<&Persona>) -> bool {
    persona.is_none_or(|p| p.provisioning == Provisioning::OnFirstUse)
}

/// The inferred allow-list: the roster first — so a bare request still signs in
/// as staff, exactly as before personas existed — then every seeded persona the
/// roster does not already name.
///
/// An empty roster yields an empty list, whatever the reason it is empty: unset,
/// or set to entries that all failed to parse (`OXY_GLOBAL_ADMINS=me@oxy`).
/// Personas widen a fallback the operator produced; a typo must not enable one.
pub(super) fn inferred_allow_list(roster: &[String]) -> Vec<String> {
    if roster.is_empty() {
        return Vec::new();
    }
    let mut list = roster.to_vec();
    for email in PERSONAS.iter().filter_map(|p| p.email) {
        if !list.iter().any(|listed| listed == email) {
            list.push(email.to_string());
        }
    }
    list
}

/// Who the caller asked to become, before the allow-list has a say.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Target<'a> {
    /// Neither `email` nor `as` — the allow-list's first entry.
    Default,
    Email(&'a str),
    Persona(&'static Persona),
}

impl Target<'_> {
    /// The email to check against the allow-list; `None` means its first entry.
    pub(super) fn requested_email(
        &self,
        roster: &[String],
    ) -> Result<Option<String>, DevLoginRefusal> {
        match self {
            Self::Default => Ok(None),
            Self::Email(email) => Ok(Some((*email).to_string())),
            Self::Persona(persona) => persona.email(roster).map(Some),
        }
    }

    pub(super) fn persona(&self) -> Option<&'static Persona> {
        match self {
            Self::Persona(persona) => Some(*persona),
            _ => None,
        }
    }
}

/// Read `email` / `as` off a request. Blank counts as absent, matching how a
/// blank `email` has always behaved.
pub(super) fn parse_target<'a>(
    email: Option<&'a str>,
    persona: Option<&str>,
) -> Result<Target<'a>, DevLoginRefusal> {
    let email = email.map(str::trim).filter(|e| !e.is_empty());
    let persona = persona.map(str::trim).filter(|p| !p.is_empty());
    match (email, persona) {
        (Some(_), Some(_)) => Err(DevLoginRefusal::with_reason(
            StatusCode::BAD_REQUEST,
            "pass `email` or `as`, not both",
        )),
        (Some(email), None) => Ok(Target::Email(email)),
        (None, Some(name)) => find(name).map(Target::Persona).ok_or_else(|| {
            DevLoginRefusal::with_reason(
                StatusCode::BAD_REQUEST,
                format!("unknown persona `{name}` — valid: {}", persona_names()),
            )
        }),
        (None, None) => Ok(Target::Default),
    }
}

fn find(name: &str) -> Option<&'static Persona> {
    PERSONAS.iter().find(|p| p.name.eq_ignore_ascii_case(name))
}

fn persona_names() -> String {
    PERSONAS
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// A refusal, carrying a reason only where saying one is safe.
///
/// 404 / 403 / 401 stay bare — the endpoint's existing vocabulary, and a 404 in
/// particular must be indistinguishable from a route that isn't mounted. 400 and
/// 409 are reachable only past that gate, by a caller the bypass already serves,
/// and each has exactly one fix worth naming.
#[derive(Debug, PartialEq, Eq)]
pub struct DevLoginRefusal {
    status: StatusCode,
    reason: Option<String>,
}

impl DevLoginRefusal {
    fn with_reason(status: StatusCode, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: Some(reason.into()),
        }
    }

    #[cfg(test)]
    pub(super) fn status(&self) -> StatusCode {
        self.status
    }
}

impl From<StatusCode> for DevLoginRefusal {
    fn from(status: StatusCode) -> Self {
        Self {
            status,
            reason: None,
        }
    }
}

impl IntoResponse for DevLoginRefusal {
    fn into_response(self) -> Response {
        match self.reason {
            None => self.status.into_response(),
            Some(reason) => {
                (self.status, Json(serde_json::json!({ "error": reason }))).into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roster() -> Vec<String> {
        vec!["staff@oxy.tech".to_string(), "second@oxy.tech".to_string()]
    }

    #[test]
    fn each_persona_resolves_to_its_email() {
        let expected = [
            ("staff", "staff@oxy.tech"),
            ("owner", "maya.nguyen@acme.test"),
            ("member", "amara.larsson@acme.test"),
            ("operator", "app-operator@oxy.local"),
            ("partner", "oliver.okafor@acme.test"),
        ];
        assert_eq!(expected.len(), PERSONAS.len(), "a persona is untested");
        for (name, email) in expected {
            let target = parse_target(None, Some(name)).unwrap();
            assert_eq!(
                target.requested_email(&roster()),
                Ok(Some(email.to_string())),
                "{name}"
            );
        }
    }

    #[test]
    fn persona_names_match_case_insensitively_and_trimmed() {
        let target = parse_target(None, Some("  Member ")).unwrap();
        assert_eq!(target.persona().map(|p| p.name), Some("member"));
    }

    #[test]
    fn unknown_persona_is_a_400_that_lists_the_valid_names() {
        let refusal = parse_target(None, Some("admin")).unwrap_err();
        assert_eq!(refusal.status(), StatusCode::BAD_REQUEST);
        let reason = refusal.reason.unwrap();
        for persona in PERSONAS {
            assert!(reason.contains(persona.name), "{reason}");
        }
    }

    #[test]
    fn email_and_persona_together_is_a_400() {
        let refusal = parse_target(Some("dev@oxy.local"), Some("owner")).unwrap_err();
        assert_eq!(refusal.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn blank_values_count_as_absent() {
        assert_eq!(parse_target(Some("  "), Some("")), Ok(Target::Default));
        assert_eq!(
            parse_target(Some("dev@oxy.local"), Some("   ")),
            Ok(Target::Email("dev@oxy.local"))
        );
    }

    #[test]
    fn staff_with_no_roster_is_a_409_not_a_silent_default() {
        // A release build, or a debug build with only an explicit list: nothing to
        // name. Falling through to "the first allow-list entry" would sign the
        // caller in as whoever that is — not who they asked for.
        let target = parse_target(None, Some("staff")).unwrap();
        let refusal = target.requested_email(&[]).unwrap_err();
        assert_eq!(refusal.status(), StatusCode::CONFLICT);
    }

    #[test]
    fn inferred_list_keeps_the_roster_first_and_adds_each_persona_once() {
        let roster = vec![
            "maya.nguyen@acme.test".to_string(),
            "staff@oxy.tech".to_string(),
        ];
        let list = inferred_allow_list(&roster);
        // Roster order preserved — its first entry is still the bare-request default.
        assert_eq!(&list[..2], &["maya.nguyen@acme.test", "staff@oxy.tech"]);
        for email in PERSONAS.iter().filter_map(|p| p.email) {
            assert_eq!(list.iter().filter(|e| *e == email).count(), 1, "{email}");
        }
        // No roster: nothing to widen, so no list at all.
        assert!(inferred_allow_list(&[]).is_empty());
    }

    #[test]
    fn only_staff_and_plain_emails_may_mint_a_user() {
        assert!(may_mint(None));
        for persona in PERSONAS {
            assert_eq!(
                may_mint(Some(persona)),
                persona.name == "staff",
                "{}",
                persona.name
            );
        }
    }

    #[test]
    fn a_reasonless_refusal_has_an_empty_body() {
        // 404 must look exactly like an unmounted route.
        let response = DevLoginRefusal::from(StatusCode::NOT_FOUND).into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            axum::body::HttpBody::size_hint(response.body()).exact(),
            Some(0)
        );
    }

    #[test]
    fn seeded_personas_match_the_seed() {
        // The table is compiled in; the seed is generated from name pools and
        // partner operator counts. Derive the same answers from the seed itself.
        for (name, email) in crate::cli::commands::seed::seeded_persona_emails() {
            let persona = find(name).unwrap_or_else(|| panic!("seed names unknown persona {name}"));
            assert_eq!(persona.email, Some(email.as_str()), "{name}");
        }
    }
}
