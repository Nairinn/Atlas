//! `atlas-migrate` — applies the SQL in `migrations/` and records what ran.
//!
//! # Why this exists
//!
//! Until now `migrations/` was mounted into the Postgres container at
//! `/docker-entrypoint-initdb.d`. That directory runs **only on a
//! database's first boot**. Every consequence of that was bad:
//!
//!   * A new migration file was silently ignored by anyone who already had
//!     a volume — including every developer on the team and every deployed
//!     environment. The schema you got depended on when you first ran
//!     `docker compose up`.
//!   * Nothing recorded which migrations had been applied, so there was no
//!     way to ask a database what state it was in.
//!   * There was no path at all to apply a schema change to a running
//!     production database. That alone makes the old arrangement
//!     unshippable.
//!
//! This binary replaces it. It tracks applied versions in `_sqlx_migrations`,
//! applies only what is pending, wraps each migration in a transaction, and
//! verifies checksums so an already-applied file cannot be edited out from
//! under a deployed environment.
//!
//! # Deployment shape
//!
//! Migrations run as a separate step that completes before any service
//! starts — a Kubernetes `Job` with the app Deployments waiting on it, and
//! a one-shot container in docker-compose. Services never migrate on
//! startup: with several replicas booting at once that is a race, and a
//! failed migration would take down every pod instead of one Job.
//!
//! The SQL is embedded at compile time, so the runtime image carries no
//! files and cannot drift from the binary.
//!
//! # Adopting this on an existing database
//!
//! A database already provisioned by the old initdb path has the schema
//! but no `_sqlx_migrations` table, so a plain `run` would try to re-apply
//! everything and fail on the first `CREATE TABLE` (the failure is clean —
//! the transaction rolls back — but the database stays unusable by this
//! tool).
//!
//! `baseline --through <version>` marks migrations up to that version as
//! applied without executing them, and leaves everything above it pending
//! for a subsequent `run`. The version is required rather than defaulted:
//! this tool cannot verify which migrations a legacy database actually
//! received, and guessing would silently skip real schema changes. Only an
//! operator who has looked at the schema knows. It is the one command here
//! that can write history that never happened, so it also refuses unless
//! the database looks provisioned at all.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use std::time::Duration;
use tracing::{error, info, warn};

/// Embedded at compile time from the repo's single source of schema truth.
static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

#[derive(Debug, Parser)]
#[command(
    name = "atlas-migrate",
    version,
    about = "Apply Atlas database migrations."
)]
struct Cli {
    /// Postgres connection string. Defaults to $DATABASE_URL.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// How long to wait for Postgres to accept connections. A migration
    /// Job usually starts at the same moment the database does.
    #[arg(long, default_value_t = 60)]
    connect_timeout_seconds: u64,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply every pending migration. Default when no subcommand is given.
    Run,
    /// Print applied and pending migrations without changing anything.
    Status {
        /// Exit non-zero if anything is pending.
        ///
        /// This is what service pods use as an init container: it blocks
        /// startup until the schema is actually current. Checking the real
        /// condition beats watching the migration Job's status — it needs
        /// no RBAC to read Jobs, and it stays correct if the schema was
        /// applied by some other route.
        #[arg(long)]
        check: bool,
    },
    /// Record migrations as applied WITHOUT running them, for adopting a
    /// database provisioned by the old initdb-mount path.
    Baseline {
        /// Highest version to mark as applied. Everything above it stays
        /// pending and a later `run` will apply it.
        ///
        /// There is no default on purpose. This tool cannot verify which
        /// migrations a legacy database actually received — only an
        /// operator who has looked at the schema knows that — so it has to
        /// be stated rather than guessed. This mirrors Flyway's
        /// `baselineVersion`.
        #[arg(long)]
        through: i64,

        /// Required. Baseline writes history that does not reflect
        /// anything actually executed, so it should never be reachable by
        /// accident from a script.
        #[arg(long)]
        i_understand_this_skips_sql: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let pool = connect(&cli.database_url, cli.connect_timeout_seconds).await?;

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run(&pool).await,
        Command::Status { check } => status(&pool, check).await,
        Command::Baseline {
            through,
            i_understand_this_skips_sql,
        } => {
            if !i_understand_this_skips_sql {
                bail!(
                    "baseline records migrations as applied without running them.\n\
                     Re-run with --i-understand-this-skips-sql if that is what you want."
                );
            }
            baseline(&pool, through).await
        }
    }
}

/// Connect, retrying until the deadline.
///
/// A migration Job and the database it targets are usually scheduled
/// together, so "connection refused" for the first few seconds is normal
/// and not a reason to fail the rollout.
async fn connect(url: &str, timeout_seconds: u64) -> Result<PgPool> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(Duration::from_secs(5))
            .connect(url)
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(e) if std::time::Instant::now() < deadline => {
                warn!(attempt, error = %e, "postgres not ready, retrying in 2s");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                return Err(e).context("connecting to postgres (giving up after timeout)");
            }
        }
    }
}

async fn run(pool: &PgPool) -> Result<()> {
    let before = applied_versions(pool).await?;

    // sqlx takes an advisory lock for the duration, so two Jobs racing —
    // a retried rollout, say — cannot apply the same migration twice.
    MIGRATOR
        .run(pool)
        .await
        .context("applying migrations (the failed migration was rolled back)")?;

    let after = applied_versions(pool).await?;
    let newly_applied: Vec<i64> = after
        .iter()
        .filter(|v| !before.contains(v))
        .copied()
        .collect();

    if newly_applied.is_empty() {
        info!("database is up to date; nothing to apply");
    } else {
        for v in &newly_applied {
            let description = MIGRATOR
                .iter()
                .find(|m| m.version == *v)
                .map(|m| m.description.to_string())
                .unwrap_or_default();
            info!(version = v, description = %description, "applied");
        }
        info!(count = newly_applied.len(), "migrations applied");
    }
    Ok(())
}

async fn status(pool: &PgPool, check: bool) -> Result<()> {
    let applied = applied_versions(pool).await?;
    if applied.is_empty() {
        info!("no migrations recorded — this database has never been migrated");
    }

    // Load checksums for the drift check: `--check` exists so the k8s
    // init container fails when the schema is wrong, and "applied but
    // edited" is wrong in the worst way — it looks up to date.
    let rows: Vec<(i64, Vec<u8>)> = if applied.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as("SELECT version, checksum FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await?
    };

    let mut pending = 0;
    let mut drifted: Vec<i64> = Vec::new();
    for m in MIGRATOR.iter() {
        let state = if applied.contains(&m.version) {
            if let Some((_, checksum)) = rows.iter().find(|(v, _)| *v == m.version) {
                if checksum.as_slice() != m.checksum.as_ref() {
                    drifted.push(m.version);
                    println!(
                        "{:>6}  {:<10} {}  [CHECKSUM MISMATCH: file changed after being applied]",
                        m.version, "DRIFTED", m.description
                    );
                    continue;
                }
            }
            "applied"
        } else {
            pending += 1;
            "PENDING"
        };
        println!("{:>6}  {:<10} {}", m.version, state, m.description);
    }

    // A version in the database that the binary does not know about means
    // the deployed image is older than the schema — usually a rollback
    // that went halfway.
    let known: Vec<i64> = MIGRATOR.iter().map(|m| m.version).collect();
    for v in &applied {
        if !known.contains(v) {
            warn!(
                version = v,
                "database has a migration this binary does not know about; \
                 the image may be older than the schema"
            );
        }
    }

    let mut failed = false;
    if pending > 0 {
        info!(pending, "migrations pending");
        if check {
            bail!("{pending} migration(s) pending");
        }
    }
    if !drifted.is_empty() {
        for v in &drifted {
            error!(
                version = v,
                "applied migration was edited after being applied"
            );
        }
        if check {
            bail!(
                "{} applied migration(s) have checksum mismatches — the recorded schema no                  longer matches the files. Restore the original file contents.",
                drifted.len()
            );
        }
        failed = true;
    }
    if !failed && pending == 0 && drifted.is_empty() {
        info!("database is up to date");
    }
    Ok(())
}

async fn baseline(pool: &PgPool, through: i64) -> Result<()> {
    if !MIGRATOR.iter().any(|m| m.version == through) {
        bail!("--through {through} is not a known migration version. Run `status` to list them.");
    }

    // Deliberately tolerant of existing history rather than demanding an
    // empty table. The realistic path here is an operator who reached for
    // `run` first, watched it fail partway, and is now adopting the
    // database — migration 1 is all `IF NOT EXISTS` and will already have
    // succeeded. Refusing that would leave them stuck. `ON CONFLICT DO
    // NOTHING` below makes re-recording an applied version a no-op.

    // Guard against baselining an empty database, which would leave it
    // permanently believing it has a schema it has never had.
    let provisioned: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.schemata WHERE schema_name = 'auth')",
    )
    .fetch_one(pool)
    .await?;
    if !provisioned {
        bail!(
            "refusing to baseline: the 'auth' schema does not exist, so this database \
             was never provisioned. Run `atlas-migrate run` instead."
        );
    }

    // One transaction: a baseline that dies halfway must not leave a
    // partial history, or the operator's mental model of "recorded
    // through N" is wrong and the next `run` will try to apply migrations
    // the schema already has.
    let mut tx = pool.begin().await?;
    // Create the history table by running the migrator against a schema it
    // will find fully applied... it cannot know that, so write the rows
    // directly. This mirrors sqlx's own table definition.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS _sqlx_migrations (
            version        BIGINT PRIMARY KEY,
            description    TEXT NOT NULL,
            installed_on   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            success        BOOLEAN NOT NULL,
            checksum       BYTEA NOT NULL,
            execution_time BIGINT NOT NULL
        )
        "#,
    )
    .execute(&mut *tx)
    .await?;

    let mut count = 0;
    for m in MIGRATOR.iter().filter(|m| m.version <= through) {
        sqlx::query(
            r#"
            INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
            VALUES ($1, $2, TRUE, $3, 0)
            ON CONFLICT (version) DO NOTHING
            "#,
        )
        .bind(m.version)
        .bind(m.description.as_ref())
        .bind(m.checksum.as_ref())
        .execute(&mut *tx)
        .await?;
        count += 1;
    }
    tx.commit().await?;

    let still_pending = MIGRATOR.iter().filter(|m| m.version > through).count();
    warn!(
        count,
        through,
        still_pending,
        "baselined: these migrations are recorded as applied but none were executed. \
         Verify the schema matches, then run `atlas-migrate run` to apply the rest."
    );
    Ok(())
}

/// Versions recorded as applied, or an empty vec when the history table
/// does not exist yet.
async fn applied_versions(pool: &PgPool) -> Result<Vec<i64>> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables WHERE table_name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await?;
    if !exists {
        return Ok(Vec::new());
    }

    let rows = sqlx::query("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| r.get::<i64, _>("version")).collect())
}
