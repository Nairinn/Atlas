//! Geofence transition detection.
//!
//! The pure part ([`diff`]) is separated from the database part
//! ([`apply_position`]) so the interesting logic — which crossings count
//! as events — is unit-testable without Postgres.

use uuid::Uuid;

use crate::pb::events::safety_alert_event::AlertType;
use crate::pb::events::SafetyAlertEvent;

/// Build the wire event for one crossing.
pub fn alert_event(
    project_id: Uuid,
    user_id: Uuid,
    geofence_id: Uuid,
    alert_type: AlertType,
    triggered_at: i64,
) -> SafetyAlertEvent {
    SafetyAlertEvent {
        // Carried through from the LocationUpdateEvent that produced this
        // crossing, so whatever consumes safety alerts downstream inherits
        // the tenant rather than having to infer it.
        project_id: project_id.to_string(),
        user_id: user_id.to_string(),
        geofence_id: geofence_id.to_string(),
        alert_type: alert_type as i32,
        triggered_at,
    }
}

/// What changed for one user at one position.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Transitions {
    pub entered: Vec<Uuid>,
    pub exited: Vec<Uuid>,
}

impl Transitions {
    pub fn is_empty(&self) -> bool {
        self.entered.is_empty() && self.exited.is_empty()
    }
}

/// Compare the fences a user is inside now against the ones they were
/// inside before.
///
/// Both inputs come back from Postgres already sorted, and the outputs
/// preserve that order so alert emission is deterministic — useful when
/// a test or an operator is reading a stream of events.
pub fn diff(previous: &[Uuid], current: &[Uuid]) -> Transitions {
    Transitions {
        entered: current
            .iter()
            .filter(|id| !previous.contains(id))
            .copied()
            .collect(),
        exited: previous
            .iter()
            .filter(|id| !current.contains(id))
            .copied()
            .collect(),
    }
}

/// Fences the user is inside at (lat, lng), by the same geography maths
/// geo-engine uses.
///
/// `radius_m` is meters, so both operands are cast to `geography`. On raw
/// 4326 geometry this would compare degrees and report every fence as
/// containing every user — the bug fixed in geo-engine's queries.
pub async fn fences_containing(
    pool: &sqlx::PgPool,
    project_id: Uuid,
    user_id: Uuid,
    lat: f64,
    lng: f64,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT id
        FROM geo.geofences
        WHERE project_id = $4
          AND user_id = $1
          AND active = TRUE
          AND ST_DWithin(
              center::geography,
              ST_SetSRID(ST_MakePoint($2, $3), 4326)::geography,
              radius_m
          )
        ORDER BY id
        "#,
    )
    .bind(user_id)
    .bind(lng)
    .bind(lat)
    .bind(project_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Read the user's recorded membership set.
pub async fn recorded_memberships(
    pool: &sqlx::PgPool,
    user_id: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let rows: Vec<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT geofence_id
        FROM geo.geofence_memberships
        WHERE user_id = $1
        ORDER BY geofence_id
        "#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Work out the transitions for a position, publish their alerts, and
/// persist the new membership set.
///
/// # Ordering, and why the publish is inside the transaction
///
/// The membership row is what suppresses the *next* alert, so whichever
/// of "write membership" and "publish alert" happens first decides the
/// failure mode:
///
/// * Commit first, then publish — a crash in between loses the alert
///   permanently. The replayed ping diffs against the already-updated
///   membership, finds no transition, and stays silent.
/// * Publish first, then commit — a crash in between duplicates the
///   alert, because the replayed ping still sees the old membership.
///
/// For a safety signal a duplicate "left the safe zone" is an annoyance
/// and a missed one is a failure, so this publishes first and commits
/// after. A publish error propagates, the transaction rolls back, the
/// Kafka offset is not committed, and the whole thing replays.
///
/// The cost is a broker round trip inside a database transaction, which
/// payments-service deliberately avoids for provider calls. It is
/// acceptable here where it is not there: the lock covers one user's
/// membership rows rather than a wallet, and the publish is a
/// sub-millisecond local-network ack with a hard 10s timeout, not a
/// third-party HTTP call.
///
/// A transaction-scoped advisory lock on (project, user) opens the
/// critical section, so two instances briefly overlapping on a partition
/// during a rebalance cannot interleave a read-diff-publish-write at all;
/// `FOR UPDATE` below only keeps the row set stable while it is held.
pub async fn apply_position(
    pool: &sqlx::PgPool,
    publisher: &impl crate::producer::AlertPublisher,
    project_id: Uuid,
    user_id: Uuid,
    lat: f64,
    lng: f64,
    occurred_at: i64,
) -> anyhow::Result<Transitions> {
    let mut tx = pool.begin().await?;

    // Advisory lock keyed on (project, user), taken at the START of the
    // transaction. `FOR UPDATE` alone only serialized the membership
    // read; the fences_containing read above ran outside any lock, so
    // two consumers briefly overlapping on a partition during a
    // rebalance could each compute a diff against a different view of
    // "previous" and emit duplicate ENTERED alerts for the same
    // crossing. The lock makes the whole read-diff-publish-write block
    // one critical section; the release is the commit.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text || ' ' || $2::text))")
        .bind(project_id.to_string())
        .bind(user_id.to_string())
        .execute(&mut *tx)
        .await?;

    let current = fences_containing(pool, project_id, user_id, lat, lng).await?;

    let previous_rows: Vec<(Uuid,)> = sqlx::query_as(
        r#"
        SELECT geofence_id
        FROM geo.geofence_memberships
        WHERE project_id = $1 AND user_id = $2
        ORDER BY geofence_id
        FOR UPDATE
        "#,
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_all(&mut *tx)
    .await?;
    let previous: Vec<Uuid> = previous_rows.into_iter().map(|(id,)| id).collect();

    let transitions = diff(&previous, &current);

    // Publish before committing — see the ordering note above. Any error
    // here returns early, dropping `tx` and rolling the membership change
    // back so the replay re-emits.
    for id in &transitions.entered {
        publisher
            .publish(&alert_event(
                project_id,
                user_id,
                *id,
                AlertType::GeofenceEntered,
                occurred_at,
            ))
            .await?;
    }
    for id in &transitions.exited {
        publisher
            .publish(&alert_event(
                project_id,
                user_id,
                *id,
                AlertType::GeofenceExited,
                occurred_at,
            ))
            .await?;
    }

    if !transitions.entered.is_empty() {
        // ON CONFLICT DO NOTHING keeps a replayed ping harmless even if
        // the diff above raced with another instance.
        sqlx::query(
            r#"
            INSERT INTO geo.geofence_memberships (project_id, user_id, geofence_id)
            SELECT $1, $2, UNNEST($3::uuid[])
            ON CONFLICT (project_id, user_id, geofence_id) DO NOTHING
            "#,
        )
        .bind(project_id)
        .bind(user_id)
        .bind(&transitions.entered)
        .execute(&mut *tx)
        .await?;
    }

    if !transitions.exited.is_empty() {
        sqlx::query(
            r#"
            DELETE FROM geo.geofence_memberships
            WHERE project_id = $1 AND user_id = $2 AND geofence_id = ANY($3::uuid[])
            "#,
        )
        .bind(project_id)
        .bind(user_id)
        .bind(&transitions.exited)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    if !transitions.is_empty() {
        metrics::counter!("atlas_safety_transitions_total", "kind" => "entered")
            .increment(transitions.entered.len() as u64);
        metrics::counter!("atlas_safety_transitions_total", "kind" => "exited")
            .increment(transitions.exited.len() as u64);
    }
    Ok(transitions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    #[test]
    fn entering_a_fence_is_an_entered_transition() {
        let t = diff(&[], &[id(1)]);
        assert_eq!(t.entered, vec![id(1)]);
        assert!(t.exited.is_empty());
    }

    #[test]
    fn leaving_a_fence_is_an_exited_transition() {
        let t = diff(&[id(1)], &[]);
        assert_eq!(t.exited, vec![id(1)]);
        assert!(t.entered.is_empty());
    }

    /// The core property: staying put emits nothing. Without this, every
    /// ping inside a fence would re-alert.
    #[test]
    fn staying_inside_emits_nothing() {
        let t = diff(&[id(1), id(2)], &[id(1), id(2)]);
        assert!(t.is_empty(), "unchanged membership must produce no alerts");
    }

    #[test]
    fn staying_outside_emits_nothing() {
        assert!(diff(&[], &[]).is_empty());
    }

    #[test]
    fn simultaneous_enter_and_exit_are_both_reported() {
        let t = diff(&[id(1), id(2)], &[id(2), id(3)]);
        assert_eq!(t.entered, vec![id(3)]);
        assert_eq!(t.exited, vec![id(1)]);
    }

    /// Overlapping fences are normal — a "downtown" fence can contain a
    /// "my block" fence — so entering several at once must report all.
    #[test]
    fn overlapping_fences_all_report() {
        let t = diff(&[], &[id(1), id(2), id(3)]);
        assert_eq!(t.entered, vec![id(1), id(2), id(3)]);
    }

    /// Replaying an already-applied ping is the at-least-once case, and
    /// it must be silent.
    #[test]
    fn replaying_an_applied_position_is_silent() {
        let previous = vec![id(1)];
        let current = vec![id(1)];
        assert!(diff(&previous, &current).is_empty());
    }

    #[test]
    fn alert_event_carries_the_crossing() {
        let e = alert_event(
            id(1),
            id(7),
            id(9),
            AlertType::GeofenceEntered,
            1_700_000_000,
        );
        assert_eq!(e.user_id, id(7).to_string());
        assert_eq!(e.geofence_id, id(9).to_string());
        assert_eq!(e.alert_type, AlertType::GeofenceEntered as i32);
        assert_eq!(e.triggered_at, 1_700_000_000);
    }

    /// The enum values are the wire contract with every consumer of
    /// atlas.safety.alerts, so pin them rather than trusting codegen
    /// ordering.
    #[test]
    fn alert_type_wire_values_match_the_proto() {
        assert_eq!(AlertType::Unknown as i32, 0);
        assert_eq!(AlertType::GeofenceEntered as i32, 1);
        assert_eq!(AlertType::GeofenceExited as i32, 2);
    }
}
