# Atlas

**Atlas is a developer platform for building location-aware, real-time, transactional apps.** Drop it into a mobility project and you get auth, geospatial queries, payments, and an event bus without configuring any of it yourself.

The SDKs expose three namespaces, one per backend service: `atlas.auth` for JWT identity with optional geospatial claims, `atlas.geo` for PostGIS-backed nearby search, route scoring, and geofencing, and `atlas.payments` for wallets, idempotent transactions, and settlement. Events have no HTTP surface. They travel over a protobuf-encoded Kafka bus that your app consumes directly (see Event flow).

Atlas is multi-tenant: many applications share one deployment and none of them can see another's users, locations, or money. A developer drops an `atlas.toml` in their project root, runs `atlas deploy`, and calls the SDK in their language of choice.

## Why this project exists

Most consumer mobility apps re-solve the same hard problems in isolation: identity, geospatial search, trust scoring, real-time messaging, and payments. Atlas builds those once, as services, and ships them as a plug-and-play platform. It is intentionally polyglot and event-driven.

## Status

In active development. What exists:

* A working Rust CLI (`atlas`): config parsing, validation, deploy, status, logs, and key management
* Five backend services: **auth** (Kotlin), **geo** (Rust), **payments** (Kotlin), an **API gateway** (Rust/Axum) that terminates public HTTP and fans out over gRPC, and a **control plane** (Rust/Axum) backing the CLI
* Three **Kafka consumers**: location (retention), safety (geofence alerts), fare (settlement)
* **Three SDKs** (`sdks/`), TypeScript, Rust and Dart, covering the same gateway surface, with a CI check that fails if any of them lags it
* **Multi-tenancy** across the whole data plane, with the boundary enforced in SQL where it can be (see [Tenant isolation](#tenant-isolation))
* Password reset and email verification, JWT signing key rotation, Stripe Connect payments machinery (see [Payments](#payments-stripe-connect-and-never-holding-the-money)), a transactional outbox, and a reconciliation sweep
* `proto/` contracts for every internal service plus Kafka event schemas, per-schema SQL migrations including PostGIS, and a migration runner with recorded history
* A **restore drill**, an OpenAPI description, Prometheus alert rules, and a dependency scan, each with a CI check that fails when it drifts
* Local development via `docker-compose`; production infra via Terraform (GKE, private Cloud SQL, Secret Manager, Workload Identity) and Kubernetes manifests validated in CI; container images for all nine workloads pushed to GHCR on every push

The money path has one unfinished piece: no project has connected a real Stripe account yet. The `PaymentProvider` seam, the Stripe Connect adapter, onboarding, and webhooks are built; what remains is a business decision, covered in [Payments](#payments-stripe-connect-and-never-holding-the-money).

## Getting started

### Local development

```bash
docker compose up -d          # migrator runs first; services wait for it
docker compose run --rm migrator status   # applied vs pending
```

### Database migrations

Migrations used to be mounted into the Postgres container at `/docker-entrypoint-initdb.d`. That directory runs **only on a database's first boot**, so a new migration was skipped by anyone who already had a volume, nothing recorded what had been applied, and there was no way to change the schema of a running database.

`tools/migrator` replaces it. It records applied versions in `_sqlx_migrations`, applies only what is pending, wraps each migration in a transaction, and verifies checksums so an applied file cannot be edited out from under a deployed environment.

Services never migrate on startup. With several replicas booting at once that is a race, and a failed migration would crash-loop every pod instead of failing one Job. In Kubernetes it is a `Job`, and each service blocks on an init container running `atlas-migrate status --check`.

If you have a volume from before this landed, its schema came from the old initdb path and has no recorded history. Adopt it once:

```bash
docker compose run --rm migrator baseline --through 40 \
    --i-understand-this-skips-sql
```

`--through` has no default on purpose: the tool cannot verify which migrations a legacy database actually received, and guessing would skip real schema changes. Or just start clean with `docker compose down -v`.

### Building from source

Two system packages are required before `cargo build` will work, because every Rust service runs `tonic-build` in its `build.rs`:

```bash
apt-get install -y protobuf-compiler cmake   # protoc for codegen, cmake for librdkafka
```

The Kotlin build needs only a JDK 21: `protobuf-gradle-plugin` resolves `protoc` from Maven rather than the system.

## Technical scope

| Area | Technology |
|---|---|
| Backend services | Rust (Axum, Tonic) and Kotlin (Ktor) |
| Developer CLI | Rust (clap, reqwest, tokio) |
| Inter-service RPC | gRPC with shared `.proto` contracts |
| Event bus | Apache Kafka with protobuf-encoded payloads |
| Database | PostgreSQL 15 with PostGIS (per-service schemas, single instance) |
| SDKs | TypeScript, Rust, and Dart (`sdks/`) |
| Infrastructure | Terraform, Kubernetes on GKE, Docker Compose for local dev |
| Observability | Prometheus metrics, structured JSON logging |
| CI/CD | GitHub Actions |

## Developer surface

A new project starts with a single config file:

```toml
[project]
name        = "my-mobility-app"
api_key     = "atl_live_xxxxxxxxxxxxxxxxxxxxxxxx"
environment = "production"
region      = "us-central1"

[services]
auth     = true
geo      = true
payments = true
events   = true
```

Then the CLI takes over:

```bash
atlas validate    # parse and validate atlas.toml
atlas deploy      # provision services with the Atlas control plane
atlas status      # show current service health
atlas logs auth   # tail logs for a given service
atlas keys list   # manage API keys
```

The CLI talks to the real control plane by default. Pass `--mock` (or set `ATLAS_MOCK=1`) to exercise commands against deterministic in-memory responses without touching anything; every mock run prints `[MOCK — nothing was changed]`.

### Going live

The control plane runs on 8081, which is the CLI's default base URL. Start it, then mint the account key that `atlas.toml` needs. This is the one bootstrap step, because every other route requires a key and `atlas deploy` cannot create the account that would hold one:

```bash
docker compose up -d postgres control-plane

curl -sX POST http://localhost:8081/v1/accounts \
  -H 'content-type: application/json' \
  -d '{"email":"you@example.com"}'
# => {"account_id":"...","api_key":"atl_dev_...","prefix":"atl_dev_...", ...}
```

Paste that `api_key` into `atlas.toml` as `project.api_key` (it is shown once and is not recoverable), then:

```bash
atlas deploy             # creates the project; idempotent on re-run
atlas status             # live gRPC health probes
atlas keys create ci --expiry 90d --scope admin
atlas logs               # the project's audit trail (--since, --limit, and a service filter work)
```

Key prefixes name their tier: `atl_dev_` (development), `atl_test_` (test), `atl_live_` (live). The curl above mints a dev key because the control plane is running locally; `atlas keys create` on a live deployment mints `atl_live_`.

`POST /v1/accounts` is the only unauthenticated write in the platform. It is rate limited (3/min per client address by default) but has no email verification, so anyone who can reach it can create accounts.

### What `atlas status` actually measures

`healthy` is a live probe on every call: a gRPC `Health/Check` against auth, geo, and payments, and a TCP connect to Kafka for `events`. Nothing is cached, so a service that dies is `DOWN` on the next invocation.

The usage columns (`p95`, requests, error rate) are currently null, and the response says `"scope": "platform"`: the gateway's counters carry no project label, so the only honest number is platform-wide traffic, which is not this project's. They come back once gateway metrics are labelled by tenant. `healthy` is unaffected.

## HTTP API

The gateway is the only service exposed publicly. Everything behind it speaks gRPC on a private network.

| Method | Path | Key | Token | Backend RPC |
|---|---|---|---|---|
| POST | `/v1/auth/register` | required | — | `auth.Register` |
| POST | `/v1/auth/login` | required | — | `auth.Authenticate` |
| POST | `/v1/auth/logout` | required | Bearer | `auth.RevokeToken` |
| GET | `/v1/auth/me` | required | Bearer | `auth.ValidateToken` |
| POST | `/v1/auth/password-reset` | required | — | `auth.RequestPasswordReset` |
| POST | `/v1/auth/password-reset/confirm` | required | — | `auth.ResetPassword` |
| POST | `/v1/auth/email/verify` | required | — | `auth.RequestEmailVerification` |
| POST | `/v1/auth/email/verify/confirm` | required | — | `auth.VerifyEmail` |
| POST | `/v1/geo/locations` | required | Bearer | `geo.UpdateLocation` |
| GET | `/v1/geo/nearby` | required | Bearer | `geo.GetNearby` |
| POST | `/v1/geo/routes/score` | required | Bearer | `geo.ScoreRoute` |
| POST | `/v1/geo/geofences` | required | Bearer | `geo.CreateGeofence` |
| GET | `/v1/geo/geofences` | required | Bearer | `geo.ListGeofences` |
| DELETE | `/v1/geo/geofences/:id` | required | Bearer | `geo.DeleteGeofence` |
| POST | `/v1/geo/safety/votes` | required | Bearer | `geo.CastSafetyVote` |
| POST | `/v1/geo/geofences/check` | required | Bearer | `geo.TriggerGeofenceCheck` |
| POST | `/v1/payments/deposits` | required | Bearer | `payments.Deposit` |
| GET | `/v1/payments/wallet` | required | Bearer | `payments.GetWalletBalance` |
| POST | `/v1/payments/transactions` | required | Bearer | `payments.InitiateTransaction` |
| POST | `/v1/payments/connected-accounts` | required | Bearer | `payments.CreateConnectAccount` |
| GET | `/v1/payments/connected-accounts` | required | Bearer | `payments.GetConnectAccount` |
| GET | `/healthz`, `/readyz` | — | — | — |

The full contract is in [`docs/openapi.yaml`](docs/openapi.yaml): 23 operations with schemas, error codes, and the reasoning behind the odd ones (why reset requests always answer 202, why deleting someone else's geofence is a 404). `scripts/check-openapi-routes.py` compares it against the axum routers in CI, in both directions, so it cannot drift unnoticed: an operation the gateway serves but the spec omits fails, and so does one the spec invents.

Errors use one envelope, with a stable `code` for SDKs to branch on:

```json
{ "error": { "code": "invalid_argument", "message": "radius_m must be > 0" } }
```

### Two credentials

Every `/v1` request carries two independent identities:

| Header | Answers | Comes from | Lives |
|---|---|---|---|
| `X-Atlas-Key: atl_live_…` | which application is calling | `atlas keys create` | your server, in an env var |
| `Authorization: Bearer *** | which of your users is calling | `POST /v1/auth/login` | per user, per session |

Neither substitutes for the other. Both are required even on register and login, because creating a user means creating them in a project.

**The project key is a server-side secret.** Anyone holding it can act on your whole project, so it must not ship in a browser bundle or a mobile app.

Health probes take neither: a liveness check that 401s would have Kubernetes restart every pod.

### The identity rules

**No request body on this API has a `user_id` or a `project_id` field.** Both are injected by the gateway: `user_id` from the validated token, `project_id` from the resolved key. The backends trust that guarantee, which is why the gateway must be the only route in, and they reject an absent `project_id` rather than defaulting. On the trusted side of the boundary a missing value is a bug, and defaulting would turn that bug into cross-tenant access.

**A token is bound to the project that issued it.** The project is signed into the JWT at login, and the gateway 401s if it does not match the key the request arrived with. Tokens are handed to end users, and one of those users may run a competing app; without this check, anyone holding a token from project A could present it with their own project B key.

### Tenant isolation

Every data-plane table carries a `project_id`, every query is scoped by it, and three things are enforced by Postgres rather than by application code:

* **Cross-tenant transfers are impossible.** Composite `(wallet, project_id)` foreign keys mean a transfer whose wallets belong to different projects fails at COMMIT.
* **Email is unique per project**, not globally. Two customers can both have `alice@example.com`; they are different people.
* **Idempotency keys are scoped per project.** Two customers both using `order-1` is normal. Before this, the second one was handed the first one's transaction as a successful idempotent replay.

`GET /v1/geo/nearby` is the query that made this urgent: it searches by position rather than by user id, so unscoped it returned every Atlas user near a point regardless of who asked.

### Reconciliation, webhooks, and retries

These are the parts of a payments integration that are the same whichever processor you pick, so they exist and are tested before one is chosen.

**Reconciliation.** A transaction goes PENDING when the provider authorizes and leaves PENDING when it settles or fails. If the process dies in between (a deploy, an OOM, a provider timeout after the charge went through), the row stays PENDING forever, and nothing retries it because nothing knows whether retrying would be a second charge. The customer may have been charged with no balance to show for it, or may not have been, and both look identical from inside Atlas.

`ReconciliationSweep` asks the provider what actually happened (`PaymentProvider.lookup`) and resolves accordingly. When the provider is unreachable or returns something the adapter does not recognise, the row is left alone and counted as unresolved. Guessing is how a temporary provider outage becomes a pile of wrongly-settled balances, and unlike the stuck row, that damage is not fixed by running the job again later. `AtlasPaymentsUnreconciled` alerts on the unresolved count, because those rows need a human.

**Webhook signatures.** `HmacWebhookVerifier` implements the scheme Stripe, GitHub, Shopify and Adyen's HMAC mode all share: HMAC-SHA256 over the raw body with a timestamp. Three properties, each of which has been a real CVE somewhere. It signs the **raw** body (re-serialised JSON has different bytes). It compares in **constant time** (`==` leaks how much of a guessed signature was right). It enforces a **timestamp window** in both directions, with the timestamp inside the signed material so it cannot be moved forward to escape the window. The endpoint also caps the body: it is unauthenticated by nature, because the signature covers the body and so must be read first.

**Retries.** Every provider call is bounded by a timeout; an unbounded one parks a request thread and drains the connection pool behind it. But only `authorize` (which carries the idempotency key) and `lookup` (which changes nothing) are retried. `capture` and `refund` are attempted once: they identify a charge but carry no idempotency key of their own, and "usually idempotent" is not a property to bet customer money on. The recovery path for an ambiguous capture is the reconciliation sweep, not a repeat of the write.

### Payments: Stripe Connect, and never holding the money

**Atlas never holds funds.** Each project connects its own Stripe account (a row in `control.project_payment_config`, credentials stored AES-256-GCM-encrypted under `PAYMENT_CONFIG_ENC_KEY`), and each payee, such as a ride's driver, gets a Stripe Express connected account under it. Money is charged, held, and paid out by Stripe, on the tenant's account. Internal wallets mirror what happened at the processor; they are not stored value Atlas owes anyone. That is how the platform stays outside money-transmitter licensing.

A fare is a **destination charge**: the tenant's Stripe account charges the rider with `capture_method=manual`, `transfer_data.destination` = the driver's connected account, and `application_fee_amount` = the tenant's cut. Capture happens at settlement; Stripe moves the driver's share and keeps the fee with the tenant in the same operation. Payouts from there are Stripe's business, which is why `WITHDRAWAL` exists in the transaction-kind constraint and stays unimplemented: Atlas taking on payouts is Atlas taking custody.

Drivers onboard through the platform: `POST /v1/payments/connected-accounts` mints Stripe's hosted onboarding URL for the caller (a caller can only set up payouts for themselves), and the `account.updated` webhook flips `payouts_enabled`, which gates whether a transaction naming them as payee can proceed.

Everything around the processor call was already real and stays real: idempotency, the pending-then-capture ordering that keeps a crash recoverable, the transactional outbox, the ledger updates, and webhook signature verification. `FakePaymentProvider` remains the default for projects with no Stripe row. One deployment can serve a test tenant on the fake and a paying tenant on Stripe, per project, via `PerProjectPaymentProvider`.

Deposits remain the only way money enters a wallet. The sweep for transactions left pending by a crash between capture and credit runs on a timer (see [Reconciliation](#reconciliation-webhooks-and-retries)). The per-project Stripe webhook endpoint (`POST /webhooks/stripe/{project_id}`, one signing secret per tenant) acts on verified events rather than only logging them: `payment_intent.succeeded` and `payment_intent.amount_capturable_updated` settle the matching transaction, `charge.refunded` reverses it, `account.updated` updates payout readiness. Unknown event types are acknowledged and logged rather than answered with a 5xx, since retrying an event Atlas cannot act on would only retry it forever.

Stripe Connect keeps Atlas outside money transmission in the usual way, but jurisdictions differ and this is not legal advice; talk to a payments lawyer before live keys go in.

Four RPCs are unrouted: `auth.IssueToken` and `payments.DrainOutbox` (both marked internal in their `.proto`), plus `payments.SettleTransaction` and `payments.RefundTransaction`, which take a bare `transaction_id` with no ownership signal the gateway could check. Settlement is driven by the fare-consumer reacting to ride lifecycle events, not by a client call.

## Event flow

Synchronous calls go through the gateway; everything asynchronous goes through Kafka. Each topic has exactly one producer.

| Topic | Produced by | Consumed by | What the consumer does |
|---|---|---|---|
| `atlas.location.updates` | geo-engine | location-consumer | Enforces the 24h retention window on `geo.locations` |
| | | safety-consumer | Diffs geofence membership and emits crossings |
| `atlas.safety.alerts` | safety-consumer | *(developer's app)* | Geofence entry/exit, for the app to act on |
| `atlas.fare.events` | payments (outbox) | fare-consumer | Settles on `RIDE_COMPLETED`, refunds on `RIDE_CANCELLED` |
| `atlas.auth.tokens` | auth-service | auth-service | Cache invalidation fanout on revocation |
| `atlas.elo.recompute` | — | — | Unused; safety scores are computed at read time |

Every event carries a `project_id`. A consumer runs long after the request that produced the event, so there is no header and no token left to derive a tenant from; it has to travel with the event. A consumer that receives one without a usable project skips and commits rather than retrying, because a replay carries the same missing field and retrying would wedge the partition on one bad record.

Settlement is event-driven, not an API call. `POST /v1/payments/transactions` creates a pending transaction and payments writes a `RIDE_ACCEPTED` event through its outbox. The money moves later, when the application publishes `RIDE_COMPLETED` or `RIDE_CANCELLED` and fare-consumer calls `SettleTransaction` / `RefundTransaction`. That is why the gateway exposes no settle endpoint.

fare-consumer ignores payments' own events. Payments publishes `TRANSACTION_SETTLED` and `TRANSACTION_REFUNDED` onto the same topic fare-consumer reads. Those are acknowledgements for the audit log, not instructions; acting on them would settle in response to having settled, forever.

## Password reset and email verification

Both are two steps: ask for a token by email, then redeem it. The redeem half needs no bearer token (whoever holds the emailed token is, for that one call, the person the account belongs to) and no project id, because the token names its own project and the person clicking a link has no idea what a project is.

The request half tells you nothing. It answers `202 {"accepted": true}` whether or not the address belongs to a user. An endpoint that distinguished them would be an account enumeration oracle, and the addresses it confirmed would be the ones worth attacking.

Further properties:

* Only the SHA-256 of a token is stored, the same rule API keys follow. The plaintext exists in one place: the email.
* Tokens are single-use, enforced by a conditional `UPDATE ... WHERE used_at IS NULL` rather than a read-then-write, so two clicks on one link cannot both succeed.
* Requesting a new link invalidates the old one, so clicking "resend" three times does not leave three live ways into an account in a mailbox.
* A completed reset **revokes every session** and evicts them from the validation cache immediately. Without that eviction a token issued before the reset would keep working for the rest of the 30s cache TTL, the window the reset exists to close.
* A verification token cannot reset a password and vice versa. They are mailed under different pretexts, and "confirm your address" is far easier to get someone to click.
* Verification does not revoke sessions: confirming an address is not evidence anything leaked.

All four endpoints are on the strict credential quota: the request halves because an unthrottled one is a way to flood somebody's inbox using your sending reputation, the confirm halves because a token is the entire credential.

### Atlas does not send mail

`EmailSender` is a port, exactly like `PaymentProvider` on the money side. The only implementation is `LoggingEmailSender`, which writes the message to the log and returns success. That is right for local development and wrong in production, where it would print reset tokens into the log aggregator for anyone with log access to redeem.

The fake sender is only wired when `ATLAS_ALLOW_LOGGING_EMAIL=true` is set; shipping it has to be somebody's decision. `docker-compose.yml` sets it, the Kubernetes manifests do not.

With no provider configured the service still serves login and registration, and the four email endpoints answer **422 `failed_precondition`** naming the missing configuration. Only the flows that need mail fail. The alternative, booting a fake sender by default, would accept reset requests and drop them; a user experiences "the email never arrived" and an operator sees nothing at all.

`GET /v1/auth/me` reports `email_verified_at`, a nullable timestamp rather than a boolean, because "when" is the question support conversations actually ask and a boolean cannot be widened into one later without having already lost the answer. Gate features on it being non-null.

That costs `/me` a second round trip. Token claims are fixed at issue and cached for 30 seconds, while a profile changes while a token is live, and verifying an address is such a change. Folding the profile into the cached claims would show a stale "unverified" for up to half a minute to the user who just clicked the link. `/me` is called when an app opens, not per location ping.

## Restore drill

A restore fails in unglamorous ways: a missing PostGIS extension, an owner that does not exist on the target, a dump taken with a flag that skipped the schema. Each of those is found during the incident unless somebody looks first.

```bash
scripts/restore-drill.sh [SOURCE_DATABASE_URL]
```

It dumps, restores into a scratch database it creates, and then checks the result: row counts per table, plus the invariants that matter more than row counts.

* **PostGIS is present and a spatial query still runs.** A restore into a database without the extension fails on the geometry columns; one with a different version can restore rows that no longer index.
* **Tenancy survived**: no user with a null `project_id`, and the bootstrap defaults are still dropped. A restore that quietly restored them would accept unscoped writes afterwards.
* **Money reconciles**: no negative balances, and no settled transfer whose wallets belong to different projects.
* **Index, foreign key, and migration-version counts match the source.** These are the objects a `--data-only` dump omits.

Safe to run against production: it only ever reads the source, and only ever writes to a database it created and drops on exit, including when a check fails, so a red drill does not leave litter that makes the next one fail for an unrelated reason.

It runs in CI, last in the integration job. By then the test suites have written real rows, so it dumps a database with data rather than an empty schema; an empty one would still catch a broken dump flag, but not a restore that loses rows.

Last hand run (2026-08-19, before migrations 0080 and 0090 landed): 17 tables, 62 indexes, 27 foreign keys, schema version 70, all invariants green. It also ran against a deliberately broken backup, a `--data-only` dump, where it failed as it should.

## Rotating the JWT signing key

A single-secret signer cannot be rotated. Changing the secret invalidates every token signed with the old one at the instant of the change, so every user is logged out simultaneously: during a deploy, while replicas are already restarting, usually in response to a suspected leak.

Tokens now carry a `kid` header naming the key that signed them, and auth-service verifies against the active key plus any retired ones. That makes rotation three independently reversible steps:

| Step | `JWT_KEY_ID` | `JWT_SECRET` | `JWT_RETIRED_KEYS` |
|---|---|---|---|
| 0. before | `k1` | old | — |
| 1. deploy the new key | `k1` | old | `k2:<new>` |
| 2. promote it | `k2` | new | `k1:<old>` |
| 3. after one token lifetime | `k2` | new | — |

Only step 3 invalidates anything, and by then every token signed with the old key has expired on its own. Each step is safe to roll back.

Two refusals are load-bearing here. A token naming a key this deployment does not hold is rejected on the `kid` alone rather than by trying every key, so "signed by something we retired" and "forged" stay distinguishable and the cost of verification does not grow with a value the caller controls. A malformed `JWT_RETIRED_KEYS` entry fails at startup rather than being skipped; dropping a retired key logs out every user holding a token signed with it, with nothing in the logs to say why.

Tokens minted before this change carry no `kid` and verify against the active key, so deploying it logs nobody out.

## Safety scores

`POST /v1/geo/safety/votes` records one user's judgement about one place, `safe` or `unsafe`, attributed to the token's user rather than a body field. `GET /v1/geo/nearby` and route scoring are both computed from those votes.

A score is the Bayesian-smoothed balance of votes near a point, bounded to 1000..2000 with 1500 meaning neutral, which is also what a place nobody has voted on scores. One voter cannot swing a location to an extreme; two hundred agreeing ones get most of the way. Aggregation takes each voter's MOST RECENT verdict within 200m, so voting twice corrects rather than stuffs the ballot, and one user is one voter however often they vote.

Every score comes with a `vote_count`, and the two should be read together: 1500 from nobody voting and 1500 from a hundred evenly split voters are different facts, and a UI that renders them identically is claiming knowledge it does not have.

It is not ELO, despite what the schema used to call it. ELO rates competitors from pairwise outcomes (A played B, A won). Safety votes are absolute judgements about one place with no opponent and no match, so ELO over them produces numbers that move without meaning anything. The old `geo.safety_ratings` table scored line segments that nothing ever produced, alongside votes nothing ever cast; migration 0060 drops it and makes votes the single source.

Votes are per project. One customer's users voting a street unsafe does not change what another customer's users are told about it.

`atlas.elo.recompute` remains unwired (scores are computed at read time rather than materialised) and stays in `events.proto` as the topic a future caching pass would use.

## Dependency advisories

```bash
scripts/check-dependencies.sh
```

Fails only on advisories against crates this build **actually compiles**. `cargo audit` reads `Cargo.lock`, and a lockfile lists optional dependencies that no enabled feature ever builds. On this repository that was five of eight findings, including `quinn-proto` at CVSS 7.5 and the unfixable `rsa` Marvin timing attack, neither of which is in any build graph. A security check that mostly reports non-exposure is one people learn to skip, and then it stops catching the real thing too.

Findings are cross-checked against `cargo tree`, which resolves features. Runs in CI as its own job.

## Alerting

`infra/k8s/base/monitoring/alerts.yaml` holds 12 Prometheus rules. Three of them carry most of the design:

* **`AtlasOutboxNotDraining`** watches the AGE of the oldest pending outbox row, not the count. A big backlog that is draining is a busy system; a small one that is not draining is a broken one, and only age tells them apart. It alerts on a gauge rather than on `outbox_dispatched_total`, because that counter simply *stops* when Kafka is unreachable, and a counter that stops looks like a system with nothing to do.
* **`AtlasTenantMismatchSpike`** fires on tokens presented with the wrong project key. That is either a broken integration or somebody probing the boundary, and both deserve a human.
* **`AtlasPasswordResetsNotCompleting`** catches reset links that are requested but never redeemed, the signature of email failing with nothing erroring anywhere.

Every expression references a metric the services actually emit, and `scripts/check-alert-metrics.py` enforces that in CI. An alert on a misspelled metric never fires: Prometheus treats a nonexistent series as empty, which is indistinguishable from healthy, and the YAML looks equally correct either way, so a reviewer cannot catch it.

Every rule carries a non-zero `for:`. All of these can spike briefly during a deploy, and alert fatigue during deploys is its own outage.

## Rate limiting

The gateway and control plane both limit requests in-process. Credential endpoints get a much smaller quota than ordinary traffic, because those are the ones worth brute forcing:

| Service | Endpoint | Default |
|---|---|---|
| gateway | `/v1/auth/login`, `/v1/auth/register` | 10/min per client |
| gateway | everything else | 600/min per token |
| control-plane | `POST /v1/accounts` | 3/min per client |
| control-plane | everything else | 120/min per API key |

Authenticated traffic is keyed by a digest of the bearer token rather than by IP: behind carrier-grade NAT or a corporate egress, thousands of real users share one address, and IP keying would throttle them together while doing nothing about one attacker rotating addresses. Health probes are never throttled; rate limiting a liveness probe turns a busy pod into a crash loop.

**This is per-replica.** Three gateway pods at 600/min is 1800/min globally. A globally exact limit needs shared state on the hot path of every request; this is the layer that stops one client exhausting one replica, underneath the ingress limit which is the real global cap.

**`TRUSTED_PROXY_HOPS` matters.** It defaults to `0`, meaning `X-Forwarded-For` is ignored entirely and the socket peer address is used. That is correct for direct exposure and safe by default: the header is client-supplied, and trusting its first entry lets anyone mint a fresh bucket per forged value. Behind a proxy, set it to the number of proxies you actually run; the Kubernetes manifests set `1` for ingress-nginx.

## SDKs

Three, covering the same surface:

| | Package | Runtime dependencies |
|---|---|---|
| TypeScript | `sdks/typescript` | none — platform `fetch` |
| Rust | `sdks/rust` | `reqwest` + `rustls`, no system TLS |
| Dart | `sdks/dart` | none — `dart:io` |

`scripts/check-sdk-coverage.py` fails CI if any of them lags the gateway, because an SDK that quietly misses an endpoint looks finished until a user reaches for a raw HTTP client instead.

All three share the same decisions. The project key is required at construction (a missing one is a configuration mistake, not a 401 on every call). `Debug`/`toString` redacts credentials. Only idempotent requests are retried, plus deposits and transactions, which carry an idempotency key that is *reused across attempts*; a fresh key per retry would double-charge.

**The Dart SDK carries an extra warning**: shipping it inside a Flutter app would put a project key, which can read every user and move money, into a binary anyone can extract it from. Put it behind your own API.

```ts
import { AtlasClient, AtlasError } from '@atlas/sdk';

const atlas = new AtlasClient({
  baseUrl: 'http://localhost:8080',      // the gateway; use your own deployment's URL
  projectKey: process.env.ATLAS_KEY!,   // sent on every call
});
await atlas.auth.login({ email, password });   // token stored on the client

const { users } = await atlas.geo.nearby({ lat, lng, radiusM: 500 });
const { balanceCents } = await atlas.payments.wallet();
```

The TypeScript SDK inherits three rules from the API design. No method takes a user id or a project id; both come from credentials, mirroring the gateway's own rules. `projectKey` is required and cannot be overridden by a caller-supplied `headers` option, so a stray header cannot retarget another project. `payments.createTransaction` always sends an idempotency key, which is what makes it the only POST the transport will retry. Errors arrive as `AtlasError` carrying the stable `code` from the envelope. It has no runtime dependencies; it uses the platform `fetch`.

See `sdks/typescript/README.md` for the retry policy and the full surface.

## Architecture

```
                +-----------------+
     HTTPS      |   API Gateway   |   Rust / Axum
 -------------->|   (port 8080)   |   JWT validation, REST to gRPC fanout
                +--------+--------+
                         |
     +-------------------+-------------------+
     |                   |                   |
     v                   v                   v
+---------+      +-------------+      +------------+
|   auth  |      |     geo     |      |  payments  |
| (Kotlin)|      |   (Rust)    |      |  (Kotlin)  |
+---------+      +-------------+      +------------+

                +-----------------+
                |     Kafka       |
                +--------+--------+
                         |
     +-------------------+-------------------+
     |                   |                   |
     v                   v                   v
+---------+     +-------------+     +-------------+
|location |     |   safety    |     |    fare     |
|consumer |     |  consumer   |     |  consumer   |
+---------+     +-------------+     +-------------+

+-----------------+     +-----------------+
| Control plane   |     |   atlas CLI     |
| (Rust/Axum:8081)|     |   (Rust)        |
+-----------------+     +-----------------+
```

Five services, three consumers. Every service owns one database schema. Synchronous communication uses gRPC, asynchronous communication uses Kafka.

## Repo layout

```
atlas/
├── cli/                # Rust developer CLI (produces the `atlas` binary)
├── proto/              # gRPC contracts and Kafka event schemas
├── migrations/         # Numbered SQL files, applied in order
├── services/
│   ├── gateway/        # Public REST edge (Rust/Axum)
│   ├── control-plane/  # Projects, API keys, status (Rust/Axum)
│   ├── auth-service/   # Identity and JWTs (Kotlin)
│   ├── geo-engine/     # PostGIS queries (Rust)
│   └── payments-service/ # Wallets and transactions (Kotlin)
├── consumers/
│   ├── location-consumer/ # Location retention (Rust)
│   ├── safety-consumer/   # Geofence alerts (Rust)
│   └── fare-consumer/     # Settlement (Kotlin)
├── tools/
│   └── migrator/       # Schema migration runner (Rust)
├── sdks/
│   ├── typescript/     # @atlas/sdk — TypeScript client
│   ├── rust/           # Rust client
│   └── dart/           # Dart client
├── .github/workflows/  # CI
├── infra/
│   ├── k8s/
│   │   ├── base/       # Deployments, HPAs, PDBs, NetworkPolicies, Ingress
│   │   ├── overlays/   # dev (1 replica) and prod (zone spread)
│   │   └── kafka-topics.yaml  # Strimzi KafkaTopic CRDs
│   └── terraform/      # VPC, GKE, Cloud SQL, Secret Manager, IAM
├── atlas.toml.example  # Sample developer config
└── docker-compose.yml  # Local dev environment
```

## License

MIT. See [`LICENSE`](LICENSE).

## Author

Naing Lynn Kyaw ([@Nairinn](https://github.com/Nairinn))
