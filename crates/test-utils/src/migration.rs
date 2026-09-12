//! Shared-database migration ordering for integration test fixtures.
//!
//! `agentic-pipeline`, `agentic-runtime`, and `agentic-airway` integration
//! tests all run against **one** Postgres database: CI points every test
//! binary's `OXY_DATABASE_URL` at the same instance, and locally the
//! testcontainer fixtures reuse a single container via
//! `ReuseDirective::Always`. Each test binary is its own process, and
//! nextest gives no ordering guarantee between binaries — so whichever
//! fixture's `test_db()` reaches the shared database first is the one that
//! actually creates the schema for everyone else.
//!
//! # Why central must run first
//!
//! Production always runs the central migrator (`crates/migration`) before
//! the agentic-runtime migrator (see `cli/commands/serve.rs`, `airway.rs`,
//! `admin.rs`), so `agentic_runtime::migration` is written defensively to
//! tolerate running *second*. The clearest evidence is in
//! `RationalizeStatusModel` (`crates/agentic/runtime/src/migration.rs`):
//!
//! ```text
//! // Ensure thread_id column exists (may have been added by central migrator
//! // or may be missing in test databases that only run runtime migrations).
//! if !column_exists(manager, "agentic_runs", "thread_id").await? {
//!     db.execute_unprepared("ALTER TABLE agentic_runs ADD COLUMN thread_id UUID").await?;
//! }
//! ```
//!
//! Central's equivalent migrations (`m20260318_000001_add_thread_id_to_agentic_runs`,
//! `m20260401_000001_add_spec_hint_to_agentic_runs`,
//! `m20260402_000001_add_thinking_mode_to_agentic_runs`) do **not** carry that
//! guard, because production never needs it — central always leads. If a test
//! fixture runs `RuntimeMigrator::up` alone and happens to reach the shared DB
//! first, central then arrives second and fails outright: `42701 column
//! "thread_id" of relation "agentic_runs" already exists`, or `42P07` on an
//! index central creates without `.if_not_exists()`. This is exactly the
//! non-determinism this helper exists to remove.
//!
//! # The fix, and why the migrators are injected
//!
//! Every fixture on the shared database migrates in the same order production
//! does: central, then runtime, then (optionally) a domain migrator. Whichever
//! binary gets there first leaves the DB correctly migrated; every other binary
//! finds everything already applied and its migration calls no-op through.
//!
//! **The order is carried by the types, not by the caller.** [`migrate_shared_test_db`]
//! hard-codes central and takes the runtime migrator as a type parameter; it
//! returns a [`DomainMigrations`] token, and a domain migrator can *only* be
//! run through that token. There is no way to obtain one without having already
//! run central-then-runtime, so a fixture cannot get the order wrong by
//! accident — which is the property, and the reason this lives in one helper
//! instead of three copy-pasted lines per fixture:
//!
//! ```ignore
//! oxy_test_utils::migration::migrate_shared_test_db::<RuntimeMigrator>(&url, &db)
//!     .await
//!     .expect("shared migrations")
//!     .then::<AirwayMigrator>()
//!     .await
//!     .expect("airway migrations")
//!     .finish()
//!     .await;
//! ```
//!
//! Injecting the migrators rather than naming them is also what keeps this
//! crate's dependency direction right. `oxy-test-utils` is a shared *platform*
//! test crate — `oxy` and `oxy-app` dev-depend on it too — and platform crates
//! must never depend on an `agentic-*` crate (`internal-docs/backend-architecture.md`,
//! "Platform → Agentic: NEVER"). Naming `RuntimeMigrator` here would have put
//! the whole agentic tree behind every `cargo nextest run -p oxy` and made this
//! crate unusable anywhere that doesn't want it. Generic parameters cost
//! nothing at the call site — every fixture that passes `RuntimeMigrator`
//! already depends on `agentic-runtime` — and cost this crate no dependency at
//! all.
//!
//! # Ordering is not enough — the sequence has to be serialized
//!
//! Ordering *within* a fixture does not order the fixtures against **each
//! other**, and that was the remaining half of the bug. Against a database
//! that already has the schema, every one of these calls is a no-op and the
//! races are invisible. Against a **fresh** one, several binaries run
//! `migration::Migrator::up` at the same moment and central's migrations are
//! not idempotent, so the losers die on the DDL the winner already ran:
//!
//! ```text
//! 42P07  relation "idx_agentic_run_events_run_id_seq" already exists
//! 42701  column "thread_id" of relation "agentic_runs" already exists
//! ```
//!
//! Making each colliding migration idempotent is whack-a-mole on *shipped*
//! migrations — patching the index above just moved the failure to the next
//! one. So the whole sequence runs under a Postgres **advisory lock**
//! ([`SHARED_TEST_DB_MIGRATION_LOCK_KEY`]), which serializes across processes
//! however many there are: the first binary migrates, the rest block, and each
//! then finds everything applied. This is the same mechanism, for the same
//! reason, as `oxy-app`'s `server::test_support` and `oxy serve`'s boot
//! migration (`cli/commands/serve.rs`) — concurrent `Migrator::up` against one
//! database is never safe, in tests or in the fleet.
//!
//! The lock covers the domain migrators too, not just central + runtime: it is
//! released by [`DomainMigrations::finish`] (or, if a fixture forgets, when the
//! token drops), so the entire central → runtime → domain sequence is one
//! critical section.
//!
//! Two things about the failure mode are worth keeping, because they make it
//! look worse than a race usually does:
//!
//! - **What a failure leaves behind depends on the sea-orm-migration major.**
//!   1.x wrapped a whole `Migrator::up` in ONE Postgres transaction
//!   (`exec_with_connection`), so a failure on the last pending migration
//!   rolled back everything and a poisoned database showed no
//!   `seaql_migrations` table at all. 2.0 (#2900) commits each migration on its
//!   own (`exec_up_with`), so the migrations before the failing one stay
//!   applied and recorded — a partially-applied database, not an empty one.
//!   A migration may also opt out of its transaction (`use_transaction()`), as
//!   the concurrent index build in `m20260911_000002` does.
//! - **It does not heal.** A fixture that runs `RuntimeMigrator::up` *without*
//!   central creates `agentic_runs` already carrying `thread_id`, and every
//!   central run afterwards dies on `42701` forever — every test in every
//!   binary, not just the one that lost a race. `pipeline_lease_test` was
//!   exactly that fixture and cost 174 of 432 tests on a fresh database; it now
//!   goes through this helper like everything else. **A lock cannot save a
//!   fixture that opts out of it** — if you add a fixture on the shared
//!   database, migrate through here.
//!
//! One gap this does not close: `oxy-app`'s `server::test_support` bootstrap
//! migrates the same database under a *different* key, and its `kind(lib)`
//! tests are not in the `serial-db` group, so a whole-workspace run against a
//! fresh database can still overlap the two families. That is the one race the
//! surviving `.if_not_exists()` guards in `crates/migration` still earn their
//! keep on; closing it means giving both bootstraps one key.

use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, DbErr};
use sea_orm_migration::MigratorTrait;

/// Advisory-lock key for the shared-test-database migration sequence.
///
/// Arbitrary but fixed. Postgres advisory keys share **one** namespace per
/// database (session and transaction variants included), so this only has to
/// be distinct from every other key Oxy takes on the same instance — the test
/// database is exactly where they all meet:
///
/// | Key | Owner |
/// | --- | ----- |
/// | `0x0787_5EED` | `oxy-app` `server::test_support::MIGRATION_LOCK_KEY` |
/// | `0x4149_5257_4450` | `oxy-app` `test_support::AIRWAY_DEPLOYMENT_LOCK_KEY` |
/// | `0x4149_5257_4159` | `oxy-app` `admin::airway_config::handlers_tests::LOCK_KEY` |
/// | `0x0041_4952_5741_5950` | `oxy-app` `admin::airway_config::preview_scan_tests::LOCK_KEY` |
/// | `0x0078_795F_6D69_6772` | `oxy serve` boot migration (`MIGRATION_ADVISORY_LOCK_KEY`) |
/// | workspace-id derived | per-workspace lazy compile (`middlewares::workspace_context`) |
/// | name+project hash | `SecretManager` per-secret write lock |
///
/// `SHRDMG` in ASCII (`53 48 52 44 4D 47`), so it is greppable and obviously
/// not a hash collision with the derived keys above.
const SHARED_TEST_DB_MIGRATION_LOCK_KEY: i64 = 0x5348_5244_4D47;

/// Runs the central migrator, then `Runtime`, against `db` — the production
/// order every fixture sharing the test database must replicate — with the
/// whole sequence held under [`SHARED_TEST_DB_MIGRATION_LOCK_KEY`]. See the
/// module docs for why both the order and the lock are load-bearing.
///
/// `url` is the database `db` is connected to. It is a separate parameter
/// because the lock needs a session of its **own** (see [`AdvisoryLock`]) and
/// sea-orm will not hand back the URL behind a `DatabaseConnection`. Every
/// fixture already has it in scope — it built `db` from it.
///
/// `Runtime` is `agentic_runtime::migration::RuntimeMigrator` at every current
/// call site; it is a parameter so this crate needs no agentic dependency.
///
/// Idempotent: safe to call from every test binary's `test_db()`, even though
/// only one of them will actually apply anything on a given shared database.
///
/// Returns the [`DomainMigrations`] token — the only way to run a domain
/// migrator in the right place, and the handle that holds the lock.
pub async fn migrate_shared_test_db<'a, Runtime: MigratorTrait>(
    url: &str,
    db: &'a DatabaseConnection,
) -> Result<DomainMigrations<'a>, DbErr> {
    let lock = AdvisoryLock::acquire(url, SHARED_TEST_DB_MIGRATION_LOCK_KEY).await?;
    let migrated = async {
        migration::Migrator::up(db, None).await?;
        Runtime::up(db, None).await
    }
    .await;
    match migrated {
        Ok(()) => Ok(DomainMigrations { db, lock }),
        // Hand the lock back before surfacing the failure: the caller only sees
        // a `DbErr` and has nothing left to release.
        Err(e) => {
            lock.release().await;
            Err(e)
        }
    }
}

/// Proof that central and runtime have already been applied to the wrapped
/// connection, and the holder of the migration advisory lock. Only
/// [`migrate_shared_test_db`] can produce one.
///
/// End with [`finish`](Self::finish) — the token is `#[must_use]` so a fixture
/// that forgets gets a warning rather than a silently longer critical section.
#[must_use = "the migration advisory lock is held until this token is finished or dropped"]
pub struct DomainMigrations<'a> {
    db: &'a DatabaseConnection,
    lock: AdvisoryLock,
}

impl<'a> DomainMigrations<'a> {
    /// Runs one domain migrator (`AirwayMigrator`, `AutomationMigrator`,
    /// `AnalyticsMigrator`, …) after central and runtime, still under the lock.
    ///
    /// Chainable, for the rare fixture that needs two. Domain migrators own
    /// disjoint tables behind their own tracking tables, so their order
    /// relative to *each other* is not load-bearing — only their position
    /// after runtime is, and that is what holding this token proves.
    pub async fn then<Domain: MigratorTrait>(self) -> Result<Self, DbErr> {
        match Domain::up(self.db, None).await {
            Ok(()) => Ok(self),
            Err(e) => {
                self.lock.release().await;
                Err(e)
            }
        }
    }

    /// Releases the migration advisory lock. Call it at the end of the chain.
    ///
    /// Dropping the token instead is *correct but less prompt*: the lock lives
    /// on a dedicated connection, so dropping it ends that session and Postgres
    /// releases the lock. That is also what covers a fixture that panics
    /// mid-migration — the lock cannot outlive the process holding it, so a
    /// failed test can never wedge the rest of the suite behind it.
    pub async fn finish(self) {
        self.lock.release().await;
    }
}

/// A held `pg_advisory_lock`, on a connection of its own.
///
/// Advisory locks are per **session**, so a shared or pooled connection risks
/// the acquire and the release landing on different sessions — or the locked
/// session being handed to unrelated work in between. This always opens a
/// dedicated pool of exactly one connection for that reason. Deliberately a
/// copy of `oxy-app`'s `server::test_support::AdvisoryLock` rather than a
/// dependency: that one is `pub(crate)` inside the binary crate, and a
/// platform test crate cannot reach into it.
struct AdvisoryLock {
    conn: DatabaseConnection,
    key: i64,
}

impl AdvisoryLock {
    /// Blocks until every other session holding `key` has released it.
    async fn acquire(url: &str, key: i64) -> Result<Self, DbErr> {
        // A pool of exactly one connection — see the struct doc for why.
        let mut opt = ConnectOptions::new(url.to_string());
        opt.max_connections(1).min_connections(1);
        let conn = Database::connect(opt).await?;
        conn.execute_unprepared(&format!("SELECT pg_advisory_lock({key})"))
            .await?;
        Ok(Self { conn, key })
    }

    async fn release(self) {
        // Best-effort: a failure here is not worth failing a test over, because
        // dropping the connection ends the session and Postgres releases the
        // lock anyway.
        let key = self.key;
        let _ = self
            .conn
            .execute_unprepared(&format!("SELECT pg_advisory_unlock({key})"))
            .await;
    }
}
