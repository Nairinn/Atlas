# Secret Manager containers for the runtime secrets.
#
# Every id is environment-suffixed: one GCP project can hold dev, staging,
# and prod side by side, and an unsuffixed "atlas-database-url" would be a
# single value three environments fight over — with prod's URL the one a
# dev `gcloud secrets versions add` overwrites.
#
# Only the containers are created here — no versions. A secret VALUE set
# through Terraform is stored in state in plaintext, so the values are
# added out of band with `gcloud secrets versions add` (see the README).
# That is the difference between "the secret exists in one managed place"
# and "the secret is in the state file that CI can read".

resource "google_secret_manager_secret" "database_url" {
  secret_id = "atlas-${var.environment}-database-url"

  replication {
    auto {}
  }

  labels = {
    environment = var.environment
    part-of     = "atlas"
  }
}

resource "google_secret_manager_secret" "jwt_secret" {
  secret_id = "atlas-${var.environment}-jwt-secret"

  replication {
    auto {}
  }

  labels = {
    environment = var.environment
    part-of     = "atlas"
  }
}

# Per-role database URL secret containers, environment-suffixed.
#
# One DATABASE_URL per role rather than per service keeps the secret count
# small while still matching the k8s Deployments' secretKeyRef keys: the
# migrator Job reads migrator-db-url, the gateway reads gateway-db-url,
# every DML workload reads service-db-url. Same values, different
# credentials, so a leaked gateway URL cannot SELECT from auth.users.

resource "google_secret_manager_secret" "migrator_db_url" {
  secret_id = "atlas-${var.environment}-migrator-db-url"

  replication {
    auto {}
  }

  labels = {
    environment = var.environment
    part-of     = "atlas"
  }
}

resource "google_secret_manager_secret" "gateway_db_url" {
  secret_id = "atlas-${var.environment}-gateway-db-url"

  replication {
    auto {}
  }

  labels = {
    environment = var.environment
    part-of     = "atlas"
  }
}

resource "google_secret_manager_secret" "service_db_url" {
  # One container per DML role; the value is the role's own URL.
  for_each = toset(["auth", "geo", "payments", "control"])

  secret_id = "atlas-${var.environment}-${each.value}-db-url"

  replication {
    auto {}
  }

  labels = {
    environment = var.environment
    part-of     = "atlas"
  }
}
