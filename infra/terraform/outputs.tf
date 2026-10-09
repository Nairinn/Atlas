output "cluster_name" {
  description = "GKE cluster name. Feed to `gcloud container clusters get-credentials`."
  value       = google_container_cluster.atlas.name
}

output "cluster_location" {
  value = google_container_cluster.atlas.location
}

output "get_credentials_command" {
  description = "Ready-to-run command to point kubectl at this cluster."
  value       = "gcloud container clusters get-credentials ${google_container_cluster.atlas.name} --region ${var.region} --project ${var.project_id}"
}

output "database_private_ip" {
  description = "Cloud SQL private IP. Goes into the atlas-${var.environment}-database-url secret."
  value       = google_sql_database_instance.atlas.private_ip_address
}

output "database_instance_name" {
  value = google_sql_database_instance.atlas.name
}

output "database_url_template" {
  description = "DSN shape for the atlas-${var.environment}-database-url secret. The password is deliberately not known to Terraform — substitute it yourself."
  value       = "postgres://atlas:<PASSWORD>@${google_sql_database_instance.atlas.private_ip_address}:5432/atlas"
}

output "workload_service_account" {
  description = "Google SA that the atlas/atlas Kubernetes service account impersonates."
  value       = google_service_account.workload.email
}

output "secret_ids" {
  description = "Secret Manager secrets whose versions must be populated out of band."
  value = [
    google_secret_manager_secret.database_url.secret_id,
    google_secret_manager_secret.jwt_secret.secret_id,
  ]
}

output "private_service_range_cidr" {
  description = "The Cloud SQL private-service-access range. The k8s egress NetworkPolicy (infra/k8s/base/network-policy.yaml, allow-cloudsql) must pin its ipBlock to exactly this; substitute it at deploy time."
  value       = "${google_compute_global_address.private_service_range.address}/${google_compute_global_address.private_service_range.prefix_length}"
}
