-- Key scopes (D4): `data` keys call the gateway; `admin` keys also
-- mutate through the control plane.
--
-- Before this, one key could do everything: the same credential that a
-- mobile backend uses to serve user traffic could also create projects
-- and mint more keys. Splitting the two means a leaked data-plane key
-- cannot escalate into account takeover, and the scope is enforced at
-- BOTH planes rather than trusted from the caller.
--
-- Existing project-scoped keys migrate to `admin` so nothing breaks:
-- they were issued to the operators who deploy, and their current
-- behaviour includes control-plane mutations. New keys default to
-- `data`, the smaller grant.

ALTER TABLE control.api_keys
    ADD COLUMN scope TEXT NOT NULL DEFAULT 'data'
        CHECK (scope IN ('data', 'admin'));

UPDATE control.api_keys SET scope = 'admin' WHERE project_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Prefix collisions: key_prefix is the human handle for revocation, and a
-- duplicate inside one project makes one key unrevokable by prefix (the
-- revoker refuses to pick arbitrarily among matches). Collisions are
-- prevented at mint time by re-rolling, so this index is the backstop:
-- it makes a collision a hard failure rather than a silent ambiguity.
-- Per project, because the same prefix in two different customers'
-- projects is harmless: revocation is always scoped.
CREATE UNIQUE INDEX idx_api_keys_project_prefix_active
    ON control.api_keys(project_id, key_prefix)
    WHERE status = 'active' AND project_id IS NOT NULL;
