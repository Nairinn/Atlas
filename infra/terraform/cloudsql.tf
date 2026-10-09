# Cloud SQL for PostgreSQL.
#
# PostGIS is required — migration 0001 does `CREATE EXTENSION postgis` and
# every geo query depends on it. Cloud SQL supports the extension on
# PostgreSQL 15; it still has to be created inside the database, which the
# migrator does on first run.
#
# Private IP only: the instance has no public address and is reachable
# solely over the VPC peering established in network.tf.

resource "google_sql_database_instance" "atlas" {
  name             = "${var.cluster_name}-${var.environment}-pg"
  database_version = "POSTGRES_15"
  region           = var.region

  # Guards against `terraform destroy` on an instance holding real data.
  deletion_protection = var.db_deletion_protection

  settings {
    tier              = var.db_tier
    availability_type = var.environment == "prod" ? "REGIONAL" : "ZONAL"
    disk_size         = var.db_disk_size_gb
    disk_type         = "PD_SSD"
    # Running out of disk on Postgres is an outage, not a degradation.
    disk_autoresize       = true
    disk_autoresize_limit = var.db_disk_size_gb * 10

    ip_configuration {
      ipv4_enabled                                  = false
      private_network                               = google_compute_network.atlas.id
      enable_private_path_for_google_cloud_services = true
      ssl_mode                                      = "ENCRYPTED_ONLY"
    }

    backup_configuration {
      enabled = true
      # Point-in-time recovery needs WAL archiving; without it a backup
      # only restores to the nightly snapshot.
      point_in_time_recovery_enabled = true
      start_time                     = "04:00"
      transaction_log_retention_days = 7
      backup_retention_settings {
        retained_backups = 30
        retention_unit   = "COUNT"
      }
    }

    maintenance_window {
      day          = 7 # Sunday
      hour         = 5
      update_track = "stable"
    }

    database_flags {
      # Every geo-engine replica holds a pool of 20 (see the HPA ceiling
      # note in infra/k8s/base/autoscaling.yaml). 200 covers 10 geo
      # replicas plus the other services with headroom. Raise this and the
      # HPA maximum together, or put PgBouncer in front.
      name  = "max_connections"
      value = "200"
    }

    database_flags {
      # Log statements slower than 1s. The default (-1) logs nothing, which
      # leaves no way to find a slow query after the fact.
      name  = "log_min_duration_statement"
      value = "1000"
    }

    insights_config {
      query_insights_enabled  = true
      record_application_tags = true
    }
  }

  depends_on = [google_service_networking_connection.private_service_connection]
}

resource "google_sql_database" "atlas" {
  name     = "atlas"
  instance = google_sql_database_instance.atlas.name
}

# The application user.
#
# The password is NOT set here. Generating one in Terraform would write it
# to state in plaintext, and state is the one place a database password
# should not be. Set it out of band and record it in Secret Manager:
#
#   PW=$(openssl rand -base64 32)
#   gcloud sql users set-password atlas \
#     --instance=<instance> --password="$PW"
#   printf 'postgres://atlas:%s@<private-ip>:5432/atlas' "$PW" | \
#     gcloud secrets versions add atlas-$ENV-database-url --data-file=-
#
# Terraform manages the user's existence; a human manages its secret.
resource "google_sql_user" "atlas" {
  name     = "atlas"
  instance = google_sql_database_instance.atlas.name

  lifecycle {
    # Terraform never learns the real password, so it must not try to
    # reconcile the empty value it has against the live one.
    ignore_changes = [password]
  }
}

# --- per-service database roles ----------------------------------------------
#
# One shared `atlas` user means every pod holds a credential that can do
# everything every other pod can. Splitting by what each workload actually
# needs:
#
#   atlas_migrator  DDL: owns the migration history, creates/alters objects.
#   atlas_<service> DML per service: SELECT/INSERT/UPDATE/DELETE on its own
#                   schemas only.
#   atlas_gateway   SELECT on control.api_keys only: the gateway reads
#                   keys, never writes them.
#
# Passwords are NOT set here (same rule as the atlas user above): a value
# in Terraform is a value in state. Set out of band:
#
#   PW=$(openssl rand -base64 32)
#   gcloud sql users set-password atlas_gateway \
#     --instance=<instance> --password="$PW"
#   printf 'postgres://atlas_gateway:%s@<private-ip>:5432/atlas' "$PW" | \
#     gcloud secrets versions add atlas-<env>-gateway-db-url --data-file=-
#
# The GRANTs themselves live in migrations/0040_control_plane.sql's wake in
# scripts/db-grants.sql, applied by the migrator's post-migrate hook, NOT
# in Terraform: grants reference schema objects Terraform does not model,
# so a `terraform apply` would fight the migrator over them.

resource "google_sql_user" "migrator" {
  name     = "atlas_migrator"
  instance = google_sql_database_instance.atlas.name

  lifecycle {
    ignore_changes = [password]
  }
}

resource "google_sql_user" "gateway" {
  name     = "atlas_gateway"
  instance = google_sql_database_instance.atlas.name

  lifecycle {
    ignore_changes = [password]
  }
}

resource "google_sql_user" "service" {
  # One SQL user per DML workload: auth, geo, payments, control.
  for_each = toset(["auth", "geo", "payments", "control"])

  name     = "atlas_${each.value}"
  instance = google_sql_database_instance.atlas.name

  lifecycle {
    ignore_changes = [password]
  }
}
