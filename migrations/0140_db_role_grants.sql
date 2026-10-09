-- Per-role database grants.
--
-- The Terraform creates the SQL users (atlas_migrator, atlas_gateway,
-- atlas_auth, atlas_geo, atlas_payments, atlas_control — see
-- infra/terraform/cloudsql.tf); this migration gives each exactly the
-- privileges its workload needs. One shared user meant every pod held a
-- credential that could do everything every other pod could.
--
-- Why here and not in Terraform: grants reference schema objects the
-- migrator owns, and a `terraform apply` would fight the migrator over
-- them. Applying as migration 0140 keeps grants in lockstep with the
-- schema they cover.
--
-- The grants run inside DO blocks that skip absent roles with a notice
-- rather than error: docker-compose and the CI integration job connect
-- as the single superuser `atlas` user and never create these roles, so
-- there the migration is a logged no-op. Where the Terraform users exist
-- (staging/prod), the grants apply for real.

-- auth: its own schema only.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'atlas_auth') THEN
        GRANT USAGE ON SCHEMA auth TO atlas_auth;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA auth TO atlas_auth;
        ALTER DEFAULT PRIVILEGES IN SCHEMA auth
            GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO atlas_auth;
    ELSE
        RAISE NOTICE 'role atlas_auth absent (dev/CI single-user mode); skipping grants';
    END IF;
END $$;

-- geo: geo schema (locations, geofences, votes, memberships). The
-- location and safety consumers connect with this role too.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'atlas_geo') THEN
        GRANT USAGE ON SCHEMA geo TO atlas_geo;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA geo TO atlas_geo;
        ALTER DEFAULT PRIVILEGES IN SCHEMA geo
            GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO atlas_geo;
    ELSE
        RAISE NOTICE 'role atlas_geo absent (dev/CI single-user mode); skipping grants';
    END IF;
END $$;

-- payments: payments schema only. Money movement lives behind this
-- grant and the application's own checks. The fare consumer connects
-- with this role.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'atlas_payments') THEN
        GRANT USAGE ON SCHEMA payments TO atlas_payments;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA payments TO atlas_payments;
        ALTER DEFAULT PRIVILEGES IN SCHEMA payments
            GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO atlas_payments;
    ELSE
        RAISE NOTICE 'role atlas_payments absent (dev/CI single-user mode); skipping grants';
    END IF;
END $$;

-- Control plane: control schema (projects, keys, audit).
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'atlas_control') THEN
        GRANT USAGE ON SCHEMA control TO atlas_control;
        GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA control TO atlas_control;
        ALTER DEFAULT PRIVILEGES IN SCHEMA control
            GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO atlas_control;
    ELSE
        RAISE NOTICE 'role atlas_control absent (dev/CI single-user mode); skipping grants';
    END IF;
END $$;

-- Gateway: reads API keys, projects, and audit rows — never writes. The
-- narrowest grant in the platform: a leaked gateway credential cannot
-- mint keys or read user data.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'atlas_gateway') THEN
        GRANT USAGE ON SCHEMA control TO atlas_gateway;
        GRANT SELECT ON ALL TABLES IN SCHEMA control TO atlas_gateway;
        ALTER DEFAULT PRIVILEGES IN SCHEMA control
            GRANT SELECT ON TABLES TO atlas_gateway;
    ELSE
        RAISE NOTICE 'role atlas_gateway absent (dev/CI single-user mode); skipping grants';
    END IF;
END $$;
