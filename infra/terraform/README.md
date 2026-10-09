# Terraform: GCP infrastructure

Provisions everything the Kubernetes manifests in `infra/k8s` assume
exists: a VPC, a GKE cluster with an enforcing CNI, a private Cloud SQL
instance with PostGIS, Secret Manager containers, and the Workload
Identity binding.

## What this does not do

Three things are deliberately left out, because they are cluster add-ons
with their own release cadences rather than infrastructure:

| Add-on | Why the manifests need it |
|---|---|
| **ingress-nginx** | `ingress.yaml` sets `ingressClassName: nginx` and the rate-limit annotations on the control plane |
| **cert-manager** | `ingress.yaml` requests certs via `cluster-issuer: letsencrypt-prod` |
| **Strimzi** | `kafka-topics.yaml` is a set of `KafkaTopic` CRDs, and every service needs a broker |

Installing them from Terraform means the Helm provider, which couples
cluster creation to application bootstrap and makes a `terraform destroy`
of the cluster hang on Helm releases it can no longer reach. They are a
documented bootstrap step instead (below).

Secret *values* are also not managed here. Terraform creates the Secret
Manager containers and grants access to them; the versions are added out
of band, because a value set through Terraform is written to state in
plaintext.

## Prerequisites

```bash
gcloud services enable \
  compute.googleapis.com container.googleapis.com \
  sqladmin.googleapis.com servicenetworking.googleapis.com \
  secretmanager.googleapis.com iam.googleapis.com \
  --project <PROJECT_ID>
```

Uncomment and fill in the `backend "gcs"` block in `versions.tf` before
more than one person runs this. Local state is fine for a first read; it
is not fine for a team.

## Apply

```bash
terraform init
terraform apply -var project_id=<PROJECT_ID> -var environment=prod
```

By default `authorized_networks` is empty, so the Kubernetes API is
reachable only from inside the VPC. Pass your egress range to use kubectl
directly:

```bash
terraform apply -var project_id=<PROJECT_ID> \
  -var 'authorized_networks=[{cidr_block="203.0.113.0/24",display_name="office"}]'
```

## Bootstrap, in order

The ordering matters — the migration Job needs the database secret, and
the Ingress needs cert-manager's CRDs to exist before it is applied.

**1. Point kubectl at the cluster**

```bash
$(terraform output -raw get_credentials_command)
```

**2. Set the database password and record the DSN**

Terraform created the `atlas` user but not its password, on purpose.
Secret ids are environment-suffixed, so set `ENV` first:

```bash
ENV=prod   # dev | staging | prod — must match var.environment

PW=$(openssl rand -base64 32)
gcloud sql users set-password atlas \
  --instance="$(terraform output -raw database_instance_name)" --password="$PW"

printf 'postgres://atlas:%s@%s:5432/atlas' \
  "$PW" "$(terraform output -raw database_private_ip)" \
  | gcloud secrets versions add "atlas-$ENV-database-url" --data-file=-

openssl rand -base64 48 | gcloud secrets versions add "atlas-$ENV-jwt-secret" --data-file=-

# Repeat for the per-role users (see the list in cloudsql.tf):
# atlas_migrator, atlas_gateway, atlas_auth, atlas_geo, atlas_payments,
# atlas_control — each DSN goes to its environment-suffixed container:
# atlas-$ENV-migrator-db-url, atlas-$ENV-gateway-db-url,
# atlas-$ENV-{auth,geo,payments,control}-db-url.
```

**3. Install the add-ons**

```bash
helm repo add ingress-nginx https://kubernetes.github.io/ingress-nginx
helm repo add jetstack https://charts.jetstack.io
helm repo add strimzi https://strimzi.io/charts/
helm repo update

helm install ingress-nginx ingress-nginx/ingress-nginx \
  -n ingress-nginx --create-namespace
helm install cert-manager jetstack/cert-manager \
  -n cert-manager --create-namespace --set crds.enabled=true
helm install strimzi strimzi/strimzi-kafka-operator \
  -n atlas --create-namespace
```

The namespace labels matter: `network-policy.yaml` allows ingress from
namespaces named `ingress-nginx` and `monitoring`, matched on the
`kubernetes.io/metadata.name` label that Kubernetes sets automatically.

You also need a `ClusterIssuer` named `letsencrypt-prod`; cert-manager
does not create one.

**4. Make the secrets available in-cluster**

The manifests read a Kubernetes Secret named `atlas-secrets`. Sync it from
Secret Manager with External Secrets Operator or the Secrets Store CSI
driver, using the service account in the `workload_service_account`
output. For a scratch cluster, create it directly — see
`infra/k8s/base/secrets.example.yaml`.

**5. Allow image pulls**

Images live in GHCR and packages are private by default, so the cluster
needs a pull secret:

```bash
kubectl -n atlas create secret docker-registry ghcr \
  --docker-server=ghcr.io --docker-username=<github-user> \
  --docker-password=<token-with-read:packages>
kubectl -n atlas patch serviceaccount default \
  -p '{"imagePullSecrets":[{"name":"ghcr"}]}'
```

Making the packages public instead removes this step entirely.

**6. Deploy**

```bash
kubectl -n atlas delete job atlas-migrate --ignore-not-found
kubectl apply -k infra/k8s/overlays/prod
kubectl -n atlas wait --for=condition=complete job/atlas-migrate --timeout=5m
```

## Notes on the choices

**Standard cluster, not Autopilot.** Dataplane V2 is set explicitly
because the NetworkPolicies are load-bearing, and a cluster without an
enforcing CNI accepts them and silently ignores them. Autopilot also
rewrites Pod resource requests, which would invalidate the connection-pool
arithmetic behind geo-engine's HPA ceiling. See the comment at the top of
`gke.tf`.

**max_connections is coupled to the HPA.** Cloud SQL is set to 200, and
geo-engine's HPA maxes at 10 replicas holding a pool of 20 each. Raise one
without the other and you exhaust the database, or waste money. PgBouncer
is the answer above that.

**Private nodes need Cloud NAT.** Without it the cluster comes up and then
fails every image pull, which is a confusing way to find the dependency.
