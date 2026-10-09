-- Drop superseded and unused geo indexes.
--
-- Every spatial query in the repo was re-grepped before this was
-- written (services/geo-engine/src/queries/, safety-consumer alerts.rs):
-- all of them filter project_id, and most also user_id, BEFORE any
-- spatial predicate. That decides each drop below.
--
-- Geometry GIST indexes are dead outright: every ST_DWithin/ST_Distance
-- call casts its column to ::geography, and a GIST over geometry cannot
-- serve a geography predicate, so these were never used by any plan.
-- Unscoped geography twins are superseded by the project-scoped ones
-- migration 0050 added: with project_id an equality predicate in every
-- query, the planner uses the scoped index and never the global one.
--
-- Deliberately NOT dropped: idx_geo_locations_project_position_geog and
-- idx_safety_votes_project_position_geog (the live spatial paths),
-- idx_geo_geofences_project_user (0130), and
-- idx_geo_geofences_project_user_active (0050) — the geofence spatial
-- queries all constrain (project_id, user_id) first, so the btree
-- narrows to one user's fences and the residual spatial filter runs on
-- a handful of rows; a GIST there would only speed up queries that
-- filter spatially WITHOUT a user, and none exist.
--
-- Easy to revert: re-create any of these from migrations 0020-0050.

DROP INDEX IF EXISTS geo.idx_geo_locations_position;
DROP INDEX IF EXISTS geo.idx_geo_locations_position_geog;
DROP INDEX IF EXISTS geo.idx_geo_geofences_center;
DROP INDEX IF EXISTS geo.idx_geo_geofences_center_geog;
DROP INDEX IF EXISTS geo.idx_geo_geofences_project_center_geog;
DROP INDEX IF EXISTS geo.idx_geo_geofences_user_active;
DROP INDEX IF EXISTS geo.idx_safety_votes_position;
-- (idx_geofence_memberships_user was already dropped by 0120; IF EXISTS
-- keeps this migration idempotent regardless.)
DROP INDEX IF EXISTS geo.idx_geofence_memberships_user;
