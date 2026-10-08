-- geofence_memberships gets its tenant column, and the advisory lock the
-- safety-consumer takes around a membership diff.
--
-- # Why memberships were unscoped until now
--
-- The only writer is the safety-consumer, and it reads/writes rows keyed
-- by (user_id, geofence_id) where the geofence itself is already scoped:
-- the FK to geo.geofences transitively pins each row to one project. So no
-- cross-tenant READ was possible — but the composite FK pattern the rest
-- of the data plane uses (migration 0080) makes the tenancy explicit at
-- the row level, and the consumer's rebalance overlap window deserves
-- the same shape as wallets: the lock and every statement key on
-- (project_id, user_id), never user_id alone.
--
-- # Backfill
--
-- project_id comes from the fence: a membership is inside a fence, the
-- fence belongs to a project, and a fence cannot change projects. The
-- backfill is a single UPDATE because geo.geofence_memberships is tiny —
-- it holds only the memberships current as of the last ping.

ALTER TABLE geo.geofence_memberships
    ADD COLUMN project_id UUID;

UPDATE geo.geofence_memberships m
SET project_id = g.project_id
FROM geo.geofences g
WHERE g.id = m.geofence_id;

ALTER TABLE geo.geofence_memberships
    ALTER COLUMN project_id SET NOT NULL,
    DROP CONSTRAINT geofence_memberships_pkey,
    ADD PRIMARY KEY (project_id, user_id, geofence_id),
    ADD CONSTRAINT geofence_memberships_project_user_fkey
        FOREIGN KEY (project_id, user_id)
        REFERENCES auth.users(project_id, id)
        ON DELETE CASCADE;

-- The old single-column user FK asserted a strictly weaker fact ("this
-- user exists") than the composite one ("this user exists IN THIS
-- PROJECT"); keeping both would only double the index maintenance on
-- every ping. Same reasoning as migration 0080's wallet drop.
ALTER TABLE geo.geofence_memberships
    DROP CONSTRAINT geofence_memberships_user_id_fkey;

-- The old single-column user index is superseded by the PK's leading
-- columns for user-scoped scans once project_id is in every predicate;
-- drop it rather than pay two indexes on every ping.
DROP INDEX IF EXISTS geo.idx_geofence_memberships_user;
