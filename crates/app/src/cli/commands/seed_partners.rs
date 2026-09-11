//! Partner + multi-tenant seed data — realistic orgs, generated people,
//! partnerships and workspaces so a developer can immediately exercise the admin
//! surface and the partner console. **Folded into `oxy seed`** (no `--partners`
//! flag); silently skipped on a non-local DB.
//!
//! The seed IS the design's demo (see
//! `internal-docs/partner-platform.md`). It proves two things
//! the old seed structurally could not:
//!
//! 1. **A partner is a real org.** Acme Consulting has its own owner, its own
//!    workspace, and — with 6 members but only 5 operator slots — an employee who
//!    holds **no partner role at all**, just using Acme's own Oxy. The partner is
//!    not a management shell.
//!
//! 2. **Access is one thing, bounded by the ceiling.** Acme's operators each reach
//!    every client Acme manages and can do everything Acme's ceiling allows — no
//!    per-person roles, no per-client scopes. Initech shows the ceiling rule: its
//!    operators get only `manage_apps` + `view_audit`, so that is all any of them
//!    can do — not manage members, query data, or onboard clients.
//!
//! People (name + email) are GENERATED from curated pools, deterministically, so
//! the data is convincing AND reproducible. Deterministic UUID v5 ids + check-
//! first inserts make the whole seed idempotent and test-referenceable.
//!
//! Dev/test only — refuses a non-local `OXY_DATABASE_URL` for the destructive
//! `--clear`, and skips the partner seed for a non-local `oxy seed`.

use chrono::Utc;
use entity::org_members::{self, OrgRole};
use entity::prelude::{
    OrgMembers, OrgTeamMembers, OrgTeams, Organizations, PartnerCapabilities, PartnerGrants,
    PartnerOrgs, PartnerRoleBindings, Workspaces,
};
use entity::workspaces::{self, WorkspaceStatus};
use entity::{
    org_team_members, org_teams, organizations, partner_capabilities, partner_grants, partner_orgs,
    partner_role_bindings,
};
use oxy::database::client::establish_connection;
use oxy::theme::StyledText;
use oxy_auth::types::Identity;
use oxy_auth::user::UserService;
use oxy_shared::errors::OxyError;
use sea_orm::{
    ActiveModelTrait, ActiveValue, ColumnTrait, EntityTrait, IntoActiveModel, QueryFilter,
};
use uuid::Uuid;

type Conn = sea_orm::DatabaseConnection;

/// One seeded org — its identity, how many people to generate, and its
/// workspaces. People (realistic name + matching email) are GENERATED, so this
/// list stays about topology, not hand-typed `owner`/`analyst` strings.
struct OrgSeed {
    slug: &'static str,
    name: &'static str,
    /// Members to generate besides the owner (person 0).
    members: usize,
    /// Workspace display names; each is pointed at the demo project.
    workspaces: &'static [&'static str],
    /// Teams inside the org — the audiences app access is granted to.
    teams: &'static [TeamSeed],
}

/// A team inside a seeded org.
///
/// Seeded so the Teams settings pane and the app-access grant picker have real
/// content on a fresh `oxy seed`, instead of an empty state that makes the feature
/// look unbuilt. Deliberately UNEVEN across orgs — one with several teams, one with
/// a single team, one with none — because the interesting UI states are the empty
/// one and the crowded one, and a uniform seed shows neither.
struct TeamSeed {
    name: &'static str,
    description: &'static str,
    /// Indices into the org's generated people (`person(i, slug)`); 0 is the owner.
    /// Every index must be `<= OrgSeed::members` — pinned by a test, because an
    /// out-of-range index would seed a team that silently grants nothing.
    members: &'static [usize],
}

/// A partnership: the org that holds it, its clients, its **ceiling**, and how
/// many of its people are **operators**. `ceiling` = [members, apps, develop_apps,
/// audit, billing, secrets, create_orgs, org_settings].
struct PartnerSeed {
    org_slug: &'static str,
    manages: &'static [&'static str],
    ceiling: [bool; 8],
    /// How many of the partner org's people (owner first, then members) get
    /// partner access. Fewer than the org has leaves an ordinary employee with
    /// NO access — the "sees no console" case.
    operators: usize,
}

/// A generated person — a realistic name and a matching email.
struct Person {
    name: String,
    email: String,
}

// Curated realistic name pools. Index-paired (`first[i]`/`last[i]`) so the i-th
// person is a stable, unique "First Last" — far more convincing than the old
// role-strings, and fully reproducible (the seed keys UUIDs off the email).
const FIRST_NAMES: &[&str] = &[
    "Maya", "Oliver", "Priya", "Diego", "Sofia", "Liam", "Amara", "Noah", "Yuki", "Elena", "Omar",
    "Grace", "Kenji", "Fatima", "Lucas", "Nadia", "Theo", "Ingrid", "Marcus", "Leila", "Ravi",
    "Chloe", "Andre", "Zara", "Hugo", "Mira", "Silas", "Nora", "Idris", "Vera",
];
const LAST_NAMES: &[&str] = &[
    "Nguyen",
    "Okafor",
    "Silva",
    "Kim",
    "Rossi",
    "Haddad",
    "Larsson",
    "Mehta",
    "Costa",
    "Dubois",
    "Tanaka",
    "Flores",
    "Novak",
    "Reyes",
    "Adeyemi",
    "Cohen",
    "Bauer",
    "Petrov",
    "Santos",
    "Ivanova",
    "Khan",
    "Werner",
    "Fontaine",
    "Diallo",
    "Marino",
    "Bishop",
    "Sato",
    "Lindqvist",
    "Osei",
    "Vargas",
];

/// The i-th person at `org_slug`: a realistic name + a matching email
/// (`first.last@<slug>.test`). Deterministic, and — for `i` within the pool —
/// unique within the org (index differs) and across orgs (domain differs).
fn person(i: usize, org_slug: &str) -> Person {
    let first = FIRST_NAMES[i % FIRST_NAMES.len()];
    let last = LAST_NAMES[i % LAST_NAMES.len()];
    Person {
        name: format!("{first} {last}"),
        email: format!(
            "{}.{}@{org_slug}.test",
            first.to_lowercase(),
            last.to_lowercase()
        ),
    }
}

const ORGS: &[OrgSeed] = &[
    // The partner — a REAL org. Its people work here; it has a workspace of its
    // own (the point of collapsing partners into orgs). Owner + 6 members = 7
    // people, so a partnership can leave one without operator access.
    OrgSeed {
        slug: "acme",
        name: "Acme Consulting",
        members: 6,
        workspaces: &["Acme Internal Analytics"],
        // The crowded case: three teams, overlapping membership (person 1 is in two),
        // and one person (6) in none — so the grant picker, the member counts, and
        // the "belongs to no team" case all have real data.
        teams: &[
            TeamSeed {
                name: "Analytics Guild",
                description: "Builds and reviews the shared models",
                members: &[0, 1, 2, 3],
            },
            TeamSeed {
                name: "Client Delivery",
                description: "Runs client engagements",
                members: &[1, 4, 5],
            },
            TeamSeed {
                name: "Leadership",
                description: "Partners and practice leads",
                members: &[0],
            },
        ],
    },
    OrgSeed {
        slug: "northwind",
        name: "Northwind Traders",
        members: 2,
        workspaces: &["Northwind Analytics", "Northwind Sandbox"],
        // One team that does NOT include the owner — the case that proves officer
        // break-glass is doing the work rather than the grant.
        teams: &[TeamSeed {
            name: "Finance",
            description: "Sees revenue and margin apps",
            members: &[1, 2],
        }],
    },
    OrgSeed {
        slug: "globex",
        name: "Globex Corporation",
        members: 2,
        workspaces: &["Globex Analytics", "Globex Sandbox"],
        teams: &[TeamSeed {
            name: "Store Managers",
            description: "Regional managers with store-level access",
            members: &[1],
        }],
    },
    // The second partner — narrow ceiling (see PARTNERS).
    OrgSeed {
        slug: "initech",
        name: "Initech",
        members: 2,
        workspaces: &["Initech Analytics"],
        teams: &[],
    },
    OrgSeed {
        slug: "umbrella",
        name: "Umbrella Industries",
        members: 1,
        workspaces: &["Umbrella Analytics"],
        teams: &[],
    },
    // Deliberately UNMANAGED — no partner, and no teams. The admin UI should show
    // both states, and the Teams pane's empty state is worth seeing too.
    OrgSeed {
        slug: "vandelay",
        name: "Vandelay Industries",
        members: 1,
        workspaces: &["Vandelay Analytics"],
        teams: &[],
    },
];

const PARTNERS: &[PartnerSeed] = &[
    PartnerSeed {
        org_slug: "acme",
        manages: &["northwind", "globex"],
        // A broad ceiling: Acme may do everything EXCEPT billing and secrets —
        // the two flags that stay Owner-only to grant.
        //   members, apps, develop, audit, billing, secrets, create_orgs, settings
        ceiling: [true, true, true, true, false, false, true, true],
        // Owner + 5 of 6 members are operators; person 6 (the last member) is an
        // ordinary Acme employee with NO partner access — must see no console.
        operators: 6,
    },
    PartnerSeed {
        org_slug: "initech",
        manages: &["umbrella"],
        // A NARROW ceiling: apps + audit only. Operators can publish apps and read
        // Umbrella's audit log — and nothing else (not manage members, query data,
        // or onboard clients): what Oxy grants the partner is the whole story, the
        // same for every operator.
        ceiling: [false, true, false, true, false, false, false, false],
        // Owner + both members.
        operators: 3,
    },
];

/// Deterministic UUID v5 for a seeded row — stable across machines/re-runs, so
/// tests can hard-code ids. Namespaced by row `kind` to avoid collisions.
fn seed_id(kind: &str, key: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_DNS,
        format!("oxy.partner-seed.{kind}.{key}").as_bytes(),
    )
}

/// Refuse to run against a database that looks like production.
///
/// The seed creates real users through the real sign-in path and grants a real
/// partnership over real orgs. "Dev/test only" in a docstring stops nobody with a
/// prod `OXY_DATABASE_URL` exported in their shell — which is a normal state for an
/// on-call engineer. The `.test` emails make it survivable, not safe.
///
/// Opt out with `OXY_SEED_ALLOW_REMOTE=1` when you genuinely mean it (a shared
/// staging box), so the guard is an obstacle to accidents, not to work.
/// Does the target DB look local (or was remote seeding explicitly allowed)? The
/// seed creates real users, orgs and a partner grant, so it must not touch prod.
/// Shared with `seed_platform_grants`, which seeds STAFF standing and therefore
/// needs the same locality guard — issuing platform grants against a real database
/// would be handing out console access — and with `seed_storage`, which fabricates
/// usage rows that a real deployment would then bill against.
///
/// One accessor on purpose: every write path that fabricates data gates on the
/// same definition of "local", because a second, drifting copy is how one seed
/// ends up safe and its sibling doesn't.
pub(crate) fn is_local_db() -> bool {
    is_local()
}

fn is_local() -> bool {
    remote_seeding_allowed() || database_url_looks_local()
}

/// `OXY_SEED_ALLOW_REMOTE` — consent to fabricated demo rows on a database that
/// doesn't look local. It covers rows, not credentials: see `seed_llm_keys`.
pub(crate) fn remote_seeding_allowed() -> bool {
    std::env::var("OXY_SEED_ALLOW_REMOTE").is_ok()
}

/// `OXY_DATABASE_URL` alone looks local, with no escape hatch consulted.
pub(crate) fn database_url_looks_local() -> bool {
    let url = std::env::var("OXY_DATABASE_URL").unwrap_or_default();
    url.is_empty()
        || url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("@postgres")
        || url.contains("host.docker.internal")
}

/// Hard error when the DB isn't local — used by the destructive `--clear` path,
/// where silently skipping would be more surprising than refusing. `pub(crate)`
/// so the seed command can guard the WHOLE clear branch up front (clear_demo has
/// no gate of its own).
pub(crate) fn refuse_if_not_local() -> Result<(), OxyError> {
    if is_local() {
        return Ok(());
    }
    let url = std::env::var("OXY_DATABASE_URL").unwrap_or_default();
    Err(OxyError::ConfigurationError(format!(
        "refusing to seed: OXY_DATABASE_URL does not look local ({}). \
         This creates users, orgs and a partner grant. Set OXY_SEED_ALLOW_REMOTE=1 if you mean it.",
        url.split('@').next_back().unwrap_or("<hidden>")
    )))
}

/// Seed all tenant + partner test data. Idempotent. `demo_path` is the demo
/// project every seeded workspace points at. On a non-local DB this **skips**
/// (does not error) so a folded `oxy seed` still seeds the demo workspace safely.
pub async fn seed_partner_tenants(demo_path: &str) -> Result<(), OxyError> {
    if !is_local() {
        println!(
            "{} skipping partner/tenant seed — OXY_DATABASE_URL is not local \
             (set OXY_SEED_ALLOW_REMOTE=1 to force)",
            "⚠️".info()
        );
        return Ok(());
    }
    println!("{} seeding partner + tenant test data", "🌱".info());
    let conn = establish_connection().await?;

    for org in ORGS {
        seed_org(&conn, org, demo_path).await?;
    }
    for partner in PARTNERS {
        seed_partner(&conn, partner).await?;
    }

    print_summary();
    Ok(())
}

/// Ensure one org, its owner + generated members, and its workspaces exist.
async fn seed_org(conn: &Conn, org: &OrgSeed, demo_path: &str) -> Result<(), OxyError> {
    let org_id = ensure_org(conn, org.slug, org.name).await?;

    // Person 0 is the owner; 1..=members are members — realistic + deterministic.
    for i in 0..=org.members {
        let p = person(i, org.slug);
        let uid = ensure_user(&p.email, &p.name).await?;
        let role = if i == 0 {
            OrgRole::Owner
        } else {
            OrgRole::Member
        };
        ensure_org_member(conn, org_id, uid, role).await?;
    }
    // First one is the landing workspace — the same one `seeded_workspace_id`
    // resolves and the example apps deploy to, so the org's home page opens on the
    // workspace that actually holds them.
    for (i, name) in org.workspaces.iter().enumerate() {
        ensure_workspace(conn, org_id, name, demo_path, i == 0).await?;
    }
    for team in org.teams {
        seed_team(conn, org_id, org.slug, team).await?;
    }
    println!(
        "  {} org {} ({}) — {} people, {} team(s)",
        "✓".success(),
        org.slug,
        org.name,
        org.members + 1,
        org.teams.len()
    );
    Ok(())
}

/// Ensure one team and its roster. Idempotent like the rest of the seed: the team
/// is keyed by a derived UUID, so re-running updates rather than duplicating.
async fn seed_team(
    conn: &Conn,
    org_id: Uuid,
    org_slug: &str,
    team: &TeamSeed,
) -> Result<(), OxyError> {
    let team_id = seed_id("team", &format!("{org_slug}:{}", team.name));
    if OrgTeams::find_by_id(team_id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("find team {}: {e}", team.name)))?
        .is_none()
    {
        org_teams::ActiveModel {
            id: ActiveValue::Set(team_id),
            org_id: ActiveValue::Set(org_id),
            name: ActiveValue::Set(team.name.to_string()),
            description: ActiveValue::Set(Some(team.description.to_string())),
            created_at: ActiveValue::NotSet,
            updated_at: ActiveValue::NotSet,
            created_by: ActiveValue::Set(None),
        }
        .insert(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("insert team {}: {e}", team.name)))?;
    }

    for i in team.members {
        let p = person(*i, org_slug);
        let uid = ensure_user(&p.email, &p.name).await?;
        let member_id = seed_id("team_member", &format!("{team_id}:{uid}"));
        if OrgTeamMembers::find_by_id(member_id)
            .one(conn)
            .await
            .map_err(|e| OxyError::DBError(format!("find team member: {e}")))?
            .is_some()
        {
            continue;
        }
        org_team_members::ActiveModel {
            id: ActiveValue::Set(member_id),
            team_id: ActiveValue::Set(team_id),
            user_id: ActiveValue::Set(uid),
            created_at: ActiveValue::NotSet,
            created_by: ActiveValue::Set(None),
        }
        .insert(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("insert team member: {e}")))?;
    }
    Ok(())
}

/// The id of the workspace this seed CREATES for `org_slug` — the one the example
/// app belongs in.
///
/// The app seed used to resolve its target with `ORDER BY name ASC LIMIT 1`, which
/// is only correct while the org has exactly the workspaces the seed made. A
/// developer who adds one (or a client who does) can sort ahead of it — `"AAA"`
/// beats `"Acme Internal Analytics"` — and a re-seed then silently moves the apps
/// onto that workspace, off the one the org subdomain names as default. The symptom
/// is the org's home page rendering an empty grid with no error anywhere.
///
/// The seeded workspace has a derived id (`seed_id("workspace", "<org>:<name>")`),
/// so name it directly instead of guessing by sort order.
pub fn seeded_workspace_id(org_slug: &str, org_id: Uuid) -> Option<Uuid> {
    let name = ORGS
        .iter()
        .find(|o| o.slug == org_slug)?
        .workspaces
        .first()?;
    Some(workspace_seed_id(org_id, name))
}

/// The derived id of a seeded workspace — shared by the writer (`ensure_workspace`)
/// and this reader so the two cannot drift. A divergence wouldn't error; it would
/// send the app seed back to its sort-order fallback, which is the bug
/// `seeded_workspace_id` exists to retire.
fn workspace_seed_id(org_id: Uuid, name: &str) -> Uuid {
    seed_id("workspace", &format!("{org_id}:{name}"))
}

/// Every workspace this seed creates, as (`<org> / <name>`, id), for orgs that
/// exist — what `oxy seed` compiles and stores LLM keys on. Empty on a non-local
/// DB, matching the skip in [`seed_partner_tenants`].
pub(crate) async fn seeded_workspace_ids(conn: &Conn) -> Result<Vec<(String, Uuid)>, OxyError> {
    if !is_local() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for org in ORGS {
        let Some(org_id) = find_org_id(conn, org.slug).await? else {
            continue;
        };
        for name in org.workspaces {
            ids.push((
                format!("{} / {name}", org.slug),
                workspace_seed_id(org_id, name),
            ));
        }
    }
    Ok(ids)
}

/// The Acme people `/api/auth/dev-login?as=` names, derived from the topology
/// above rather than restated: `owner` is person 0; `partner` is the first
/// non-owner holding a partner binding; `member` is the first member holding
/// none. The dev-login persona table is compiled in and pinned against this.
#[cfg(test)]
pub(crate) fn acme_persona_emails() -> Vec<(&'static str, String)> {
    let acme = ORGS.iter().find(|o| o.slug == "acme").expect("acme seeded");
    let partner = PARTNERS
        .iter()
        .find(|p| p.org_slug == "acme")
        .expect("acme is a partner");
    let operator = (1..partner.operators).next().unwrap_or(0);
    let plain_member = (partner.operators.max(1)..=acme.members)
        .next()
        .expect("acme leaves a member without partner access");
    vec![
        ("owner", person(0, "acme").email),
        ("partner", person(operator, "acme").email),
        ("member", person(plain_member, "acme").email),
    ]
}

/// The team an org's example app is restricted to, if the seed defines one.
///
/// Only Acme, because it is the only seeded org the example app is deployed to
/// besides `local` — and leaving `local`'s copy open is what makes a fresh seed show
/// BOTH states side by side.
///
/// "Client Delivery" deliberately **excludes the owner** (person 0) and includes
/// only 1, 4 and 5 of Acme's seven people. So one seed exercises every branch of
/// `Ring::AppAccess` at once: granted-via-team (1/4/5), officer break-glass (0, who
/// holds no grant), and filtered-out (2, 3, 6 — who don't see the card at all).
pub fn restricted_team_for(org_slug: &str) -> Option<Uuid> {
    (org_slug == "acme").then(|| seed_id("team", "acme:Client Delivery"))
}

/// Ensure one partnership: the grant, its ceiling, its clients, and its people's
/// role bindings.
async fn seed_partner(conn: &Conn, partner: &PartnerSeed) -> Result<(), OxyError> {
    let Some(partner_org_id) = find_org_id(conn, partner.org_slug).await? else {
        return Ok(());
    };
    ensure_grant(conn, partner_org_id).await?;
    ensure_ceiling(conn, partner_org_id, partner.ceiling).await?;

    for slug in partner.manages {
        if let Some(org_id) = find_org_id(conn, slug).await? {
            ensure_partner_org(conn, partner_org_id, org_id).await?;
        }
    }

    for i in 0..partner.operators {
        // A partner's operators are ORDINARY MEMBERS of the partner org (person i,
        // owner first). The access row hangs off that membership.
        let p = person(i, partner.org_slug);
        let user_id = ensure_user(&p.email, &p.name).await?;
        let Some(member) = OrgMembers::find()
            .filter(org_members::Column::OrgId.eq(partner_org_id))
            .filter(org_members::Column::UserId.eq(user_id))
            .one(conn)
            .await
            .map_err(|e| OxyError::DBError(format!("query member {}: {e}", p.email)))?
        else {
            continue;
        };

        ensure_binding(conn, member.id).await?;
    }

    println!(
        "  {} partner {} → {} client(s), {} operator(s)",
        "✓".success(),
        partner.org_slug,
        partner.manages.len(),
        partner.operators
    );
    Ok(())
}

async fn ensure_org(conn: &Conn, slug: &str, name: &str) -> Result<Uuid, OxyError> {
    if let Some(existing) = Organizations::find()
        .filter(organizations::Column::Slug.eq(slug))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query org {slug}: {e}")))?
    {
        return Ok(existing.id);
    }
    let now = Utc::now().fixed_offset();
    let id = seed_id("org", slug);
    organizations::ActiveModel {
        id: ActiveValue::Set(id),
        name: ActiveValue::Set(name.to_string()),
        slug: ActiveValue::Set(slug.to_string()),
        logo: ActiveValue::NotSet,
        logo_content_type: ActiveValue::NotSet,
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert org {slug}: {e}")))?;
    Ok(id)
}

async fn find_org_id(conn: &Conn, slug: &str) -> Result<Option<Uuid>, OxyError> {
    Ok(Organizations::find()
        .filter(organizations::Column::Slug.eq(slug))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("lookup org {slug}: {e}")))?
        .map(|o| o.id))
}

/// Email-keyed, idempotent user create via `UserService` (same path as OAuth
/// login), so a seeded email can sign in with a magic link.
async fn ensure_user(email: &str, name: &str) -> Result<Uuid, OxyError> {
    let user = UserService::get_or_create_user(&Identity {
        // A seed path: this must be able to mint the row, so no id.
        user_id: None,
        email: email.to_string(),
        name: Some(name.to_string()),
        picture: None,
    })
    .await?;
    Ok(user.id)
}

async fn ensure_org_member(
    conn: &Conn,
    org_id: Uuid,
    user_id: Uuid,
    role: OrgRole,
) -> Result<(), OxyError> {
    let existing = OrgMembers::find()
        .filter(org_members::Column::OrgId.eq(org_id))
        .filter(org_members::Column::UserId.eq(user_id))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query org_member: {e}")))?;
    if existing.is_some() {
        return Ok(());
    }
    let now = Utc::now().fixed_offset();
    org_members::ActiveModel {
        id: ActiveValue::Set(seed_id("org_member", &format!("{org_id}:{user_id}"))),
        org_id: ActiveValue::Set(org_id),
        user_id: ActiveValue::Set(user_id),
        role: ActiveValue::Set(role),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert org_member: {e}")))?;
    Ok(())
}

/// A workspace row pointed at the demo project (`demo_path`), so a seeded org's
/// workspace opens to real content instead of an empty shell.
///
/// `is_landing` marks the org's first workspace — the one `seeded_workspace_id`
/// names and the one the example apps are deployed to. It gets a `last_opened_at`
/// so the frontend's workspace picker lands there by default; see the comment on
/// the write below for why that column and not another.
async fn ensure_workspace(
    conn: &Conn,
    org_id: Uuid,
    name: &str,
    demo_path: &str,
    is_landing: bool,
) -> Result<(), OxyError> {
    let id = workspace_seed_id(org_id, name);
    if let Some(row) = Workspaces::find_by_id(id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query workspace {name}: {e}")))?
    {
        // Patch the path if it's missing or stale. Earlier seeds created these
        // path-less, which made the recovery/latency worker skip + warn on them
        // ("workspace has no path"); a re-run of `oxy seed` now heals them.
        let stale_path = row.path.as_deref() != Some(demo_path);
        // Backfill only — never restamp. A box seeded before this existed heals on
        // the next run, but a value already there is left alone.
        let needs_landing = is_landing && row.last_opened_at.is_none();
        if stale_path || needs_landing {
            let now = Utc::now().fixed_offset();
            let mut active = row.into_active_model();
            if stale_path {
                active.path = ActiveValue::Set(Some(demo_path.to_string()));
            }
            if needs_landing {
                active.last_opened_at = ActiveValue::Set(Some(now));
            }
            active.updated_at = ActiveValue::Set(now);
            active
                .update(conn)
                .await
                .map_err(|e| OxyError::DBError(format!("patch workspace {name}: {e}")))?;
        }
        return Ok(());
    }
    let now = Utc::now().fixed_offset();
    workspaces::ActiveModel {
        id: ActiveValue::Set(id),
        name: ActiveValue::Set(name.to_string()),
        git_namespace_id: ActiveValue::Set(None),
        git_remote_url: ActiveValue::Set(None),
        created_at: ActiveValue::Set(now),
        updated_at: ActiveValue::Set(now),
        path: ActiveValue::Set(Some(demo_path.to_string())),
        // The picker (`pickWorkspace`) sorts navigable workspaces by
        // `last_opened_at` descending and treats NULL as the epoch. Seeding every
        // workspace NULL left that comparison a tie for every org, so the landing
        // fell through to whatever order the API happened to return — and a
        // workspace a developer added later could win it. Stamping the org's first
        // workspace makes it the default landing outright.
        //
        // Nothing in the server writes this column today, so the value stays put;
        // if that changes, a real open is newer than the seed and wins, which is
        // the behavior we want either way.
        //
        // Accepted cost: the column is also operator-facing — the admin workspace
        // list renders it as "Last opened" and the detail page computes an age off
        // it — so every seeded landing workspace will report a visit nobody made.
        // That is a lie only on a seeded box, where every row is fabricated anyway,
        // and it buys a deterministic landing on real data.
        last_opened_at: ActiveValue::Set(is_landing.then_some(now)),
        created_by: ActiveValue::Set(None),
        org_id: ActiveValue::Set(Some(org_id)),
        status: ActiveValue::Set(WorkspaceStatus::Ready),
        error: ActiveValue::Set(None),
        monthly_vlm_budget_micros: ActiveValue::Set(None),
        current_revision_id: ActiveValue::Set(None),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert workspace {name}: {e}")))?;
    Ok(())
}

/// The partnership itself — keyed on the org, because the partner IS the org.
async fn ensure_grant(conn: &Conn, org_id: Uuid) -> Result<(), OxyError> {
    if PartnerGrants::find_by_id(org_id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query partner_grant: {e}")))?
        .is_some()
    {
        return Ok(());
    }
    partner_grants::ActiveModel {
        org_id: ActiveValue::Set(org_id),
        status: ActiveValue::Set("active".to_string()),
        created_by: ActiveValue::Set(None),
        created_at: ActiveValue::Set(Utc::now().fixed_offset()),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert partner_grant: {e}")))?;
    Ok(())
}

/// The ceiling — what Oxy permits this partner AT ALL. Every role a partner hands
/// out is intersected with this.
async fn ensure_ceiling(conn: &Conn, org_id: Uuid, caps: [bool; 8]) -> Result<(), OxyError> {
    if PartnerCapabilities::find_by_id(org_id)
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query partner_capabilities: {e}")))?
        .is_some()
    {
        return Ok(());
    }
    partner_capabilities::ActiveModel {
        org_id: ActiveValue::Set(org_id),
        manage_members: ActiveValue::Set(caps[0]),
        manage_apps: ActiveValue::Set(caps[1]),
        develop_apps: ActiveValue::Set(caps[2]),
        view_audit: ActiveValue::Set(caps[3]),
        manage_billing: ActiveValue::Set(caps[4]),
        manage_secrets: ActiveValue::Set(caps[5]),
        create_orgs: ActiveValue::Set(caps[6]),
        manage_org_settings: ActiveValue::Set(caps[7]),
        updated_at: ActiveValue::Set(Utc::now().fixed_offset()),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert partner_capabilities: {e}")))?;
    Ok(())
}

/// Attach a client. `managed_org_id` is UNIQUE — one partner per client — so if
/// the org is already attached we leave the existing link alone.
async fn ensure_partner_org(
    conn: &Conn,
    partner_org_id: Uuid,
    managed_org_id: Uuid,
) -> Result<(), OxyError> {
    if PartnerOrgs::find()
        .filter(partner_orgs::Column::ManagedOrgId.eq(managed_org_id))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query partner_org: {e}")))?
        .is_some()
    {
        return Ok(());
    }
    partner_orgs::ActiveModel {
        id: ActiveValue::Set(seed_id("partner_org", &managed_org_id.to_string())),
        partner_org_id: ActiveValue::Set(partner_org_id),
        managed_org_id: ActiveValue::Set(managed_org_id),
        created_by: ActiveValue::Set(None),
        created_at: ActiveValue::Set(Utc::now().fixed_offset()),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert partner_org: {e}")))?;
    Ok(())
}

/// Partner access for this member — one row means they're an operator.
async fn ensure_binding(conn: &Conn, org_member_id: Uuid) -> Result<Uuid, OxyError> {
    if let Some(existing) = PartnerRoleBindings::find()
        .filter(partner_role_bindings::Column::OrgMemberId.eq(org_member_id))
        .one(conn)
        .await
        .map_err(|e| OxyError::DBError(format!("query partner_role_binding: {e}")))?
    {
        return Ok(existing.id);
    }
    let id = seed_id("partner_binding", &org_member_id.to_string());
    partner_role_bindings::ActiveModel {
        id: ActiveValue::Set(id),
        org_member_id: ActiveValue::Set(org_member_id),
        created_at: ActiveValue::Set(Utc::now().fixed_offset()),
    }
    .insert(conn)
    .await
    .map_err(|e| OxyError::DBError(format!("insert partner_role_binding: {e}")))?;
    Ok(id)
}

fn print_summary() {
    let users: usize = ORGS.iter().map(|o| o.members + 1).sum();
    let workspaces: usize = ORGS.iter().map(|o| o.workspaces.len()).sum();
    println!(
        "{} seeded {} orgs, {} users, {} workspaces, {} partnership(s)",
        "✅".success(),
        ORGS.len(),
        users,
        workspaces,
        PARTNERS.len()
    );
    println!("\n  Sign in with a magic link as any owner below (or any first.last@<org>.test):");
    for o in ORGS {
        let owner = person(0, o.slug);
        let role = if let Some(p) = PARTNERS.iter().find(|p| p.org_slug == o.slug) {
            format!(
                "PARTNER — {} operators, manages {}",
                p.operators,
                p.manages.join(", ")
            )
        } else if PARTNERS.iter().any(|p| p.manages.contains(&o.slug)) {
            "managed client".to_string()
        } else {
            "unmanaged".to_string()
        };
        println!(
            "    {:<20} owner {} <{}> — {role}",
            o.name, owner.name, owner.email
        );
    }
}

/// Tear down seeded partner + tenant rows. Deleting the orgs cascades to the
/// grants, ceilings, client links, role bindings and assignments (every FK is
/// `on_delete = Cascade`). Seeded users are left in place, like `clear_demo`.
pub async fn clear_partner_tenants() -> Result<(), OxyError> {
    // Deleting orgs cascades to members, workspaces and partnerships — the last
    // thing you want pointed at prod by accident.
    refuse_if_not_local()?;
    let conn = establish_connection().await?;
    let mut removed = 0u64;
    for org in ORGS {
        removed += Organizations::delete_many()
            .filter(organizations::Column::Slug.eq(org.slug))
            .exec(&conn)
            .await
            .map_err(|e| OxyError::DBError(format!("delete org {}: {e}", org.slug)))?
            .rows_affected;
    }
    println!(
        "{} cleared {removed} seeded org(s) (cascades removed partnerships, members + workspaces)",
        "🧹".info()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn person_is_deterministic_and_realistic() {
        // Same (index, org) → same person, so the seed is reproducible.
        assert_eq!(person(0, "acme").email, person(0, "acme").email);
        let p = person(0, "acme");
        // A real "First Last" and a matching first.last@org.test — not "owner".
        assert!(
            p.name.contains(' '),
            "name should be 'First Last': {}",
            p.name
        );
        assert_ne!(p.name.to_lowercase(), "owner");
        assert!(p.email.ends_with("@acme.test"), "email: {}", p.email);
        assert!(
            p.email.contains('.'),
            "email should be first.last: {}",
            p.email
        );
    }

    #[test]
    fn people_are_unique_within_an_org() {
        // acme: owner + 6 members = person(0..=6). All seven emails distinct, so
        // no two seeded users collide.
        let emails: HashSet<_> = (0..=6).map(|i| person(i, "acme").email).collect();
        assert_eq!(emails.len(), 7);
    }

    #[test]
    fn pools_cover_the_biggest_org() {
        // person() indexes the pools mod their length; keep the largest org within
        // the pool so its members stay unique.
        let max_people = ORGS.iter().map(|o| o.members + 1).max().unwrap();
        assert!(max_people <= FIRST_NAMES.len(), "grow FIRST_NAMES");
        assert!(max_people <= LAST_NAMES.len(), "grow LAST_NAMES");
    }

    #[test]
    fn team_members_index_real_people() {
        // An index past the org's roster would seed a team missing a person, and the
        // grant would silently reach fewer people than the seed claims. Nothing at
        // runtime would complain — `person(i, slug)` just wraps the name pools.
        for org in ORGS {
            for team in org.teams {
                assert!(
                    !team.members.is_empty(),
                    "{}/{}: an empty team grants nothing — drop it or give it people",
                    org.slug,
                    team.name
                );
                for i in team.members {
                    assert!(
                        *i <= org.members,
                        "{}/{}: member index {i} is past the org's {} people",
                        org.slug,
                        team.name,
                        org.members + 1
                    );
                }
            }
        }
    }

    #[test]
    fn team_names_are_unique_within_an_org() {
        // `org_teams` has a case-insensitive unique index on (org_id, name), so a
        // duplicate would make the seed fail on insert rather than be idempotent.
        for org in ORGS {
            let names: HashSet<_> = org.teams.iter().map(|t| t.name.to_lowercase()).collect();
            assert_eq!(
                names.len(),
                org.teams.len(),
                "{}: duplicate team name (case-insensitively)",
                org.slug
            );
        }
    }

    #[test]
    fn team_ids_are_deterministic_and_distinct() {
        // Re-seeding must update rather than duplicate, which relies on the derived
        // id being stable — and two teams must never collide onto one id.
        let a = seed_id("team", "acme:Client Delivery");
        assert_eq!(a, seed_id("team", "acme:Client Delivery"));
        assert_ne!(a, seed_id("team", "acme:Analytics Guild"));
        // Same team name in two orgs is legal and must stay distinct.
        assert_ne!(
            seed_id("team", "acme:Finance"),
            seed_id("team", "globex:Finance")
        );
    }

    #[test]
    fn the_restricted_seed_app_targets_a_real_team_that_excludes_its_owner() {
        // The seeded restriction is only interesting if it exercises more than one
        // branch of Ring::AppAccess. Acme's "Client Delivery" must exist, and must
        // NOT contain person 0 — otherwise the owner reaches the app by grant and
        // officer break-glass never gets exercised by a fresh seed.
        let acme = ORGS.iter().find(|o| o.slug == "acme").expect("acme seeded");
        let team = acme
            .teams
            .iter()
            .find(|t| t.name == "Client Delivery")
            .expect("restricted_team_for() names a team the seed creates");
        assert!(
            !team.members.contains(&0),
            "Client Delivery must exclude the owner so break-glass is exercised"
        );
        // And somebody must be left out entirely, or the filtered-out state — the
        // one this whole feature exists for — never appears in seeded data.
        let granted: HashSet<_> = team.members.iter().copied().collect();
        let excluded = (0..=acme.members).filter(|i| !granted.contains(i)).count();
        assert!(
            excluded >= 2,
            "at least two of Acme's people should NOT reach the restricted app"
        );
    }

    #[test]
    fn only_an_org_with_a_seeded_app_is_restricted() {
        // `restricted_team_for` is consumed by the app seed, which deploys to `local`
        // and `acme` only. Naming any other org would set visibility on an app that
        // doesn't exist — a silent no-op that reads like a bug when someone looks.
        for org in ORGS {
            if restricted_team_for(org.slug).is_some() {
                assert_eq!(
                    org.slug, "acme",
                    "only Acme gets the example app besides `local`"
                );
            }
        }
        assert!(restricted_team_for("acme").is_some());
        assert!(restricted_team_for("vandelay").is_none());
    }

    #[test]
    fn partners_reference_real_orgs_and_fit_their_people() {
        for p in PARTNERS {
            let org = ORGS
                .iter()
                .find(|o| o.slug == p.org_slug)
                .unwrap_or_else(|| panic!("partner org {} not in ORGS", p.org_slug));
            // Can't grant access to more people than the org has.
            assert!(
                p.operators <= org.members + 1,
                "{}: {} operators but {} people",
                p.org_slug,
                p.operators,
                org.members + 1
            );
            // Every managed client is a real seeded org too.
            for client in p.manages {
                assert!(
                    ORGS.iter().any(|o| o.slug == *client),
                    "{} manages unknown org {client}",
                    p.org_slug
                );
            }
        }
    }

    #[test]
    fn acme_leaves_one_employee_without_partner_access() {
        // The deliberate test case: an ordinary employee who must see no console.
        let acme = ORGS.iter().find(|o| o.slug == "acme").unwrap();
        let partner = PARTNERS.iter().find(|p| p.org_slug == "acme").unwrap();
        assert!(
            partner.operators < acme.members + 1,
            "acme should leave >=1 member without partner access"
        );
    }
}
