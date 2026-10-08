-- Non-partial index on geo.geofences(project_id, user_id).
--
-- Migration 0050 already created the partial twin
-- idx_geo_geofences_project_user_active (project_id, user_id) WHERE
-- active = TRUE, which serves every hot path: fences_containing and
-- ListGeofences(active_only=true). What it cannot serve is the
-- ListGeofences(active_only=false) scan — the admin "show me everything
-- including soft-deleted" view — because a partial index simply does not
-- contain those rows. That query currently falls back to the project-wide
-- GIST index and filters. One plain btree on the same column pair closes
-- it; maintenance cost is one more index write per geofence DDL.
--
-- Kept despite the overlap rather than merged: the partial one stays
-- smaller for the ping-time hot path, and replacing it with this one
-- would trade hot-path size for a query an admin runs occasionally.

CREATE INDEX IF NOT EXISTS idx_geo_geofences_project_user
    ON geo.geofences(project_id, user_id);
