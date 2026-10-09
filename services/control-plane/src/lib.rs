//! Library surface for `atlas-control-plane` — the backend the `atlas`
//! CLI and the dashboard talk to.
//!
//! # What it owns
//!
//! Projects, API keys, and the audit trail. It is the only service in the
//! platform that knows a "project" exists at all: auth, geo, and payments
//! deal in users, locations, and wallets, and are entirely unaware of who
//! is paying for them.
//!
//! # The contract is the CLI
//!
//! `cli/src/api.rs` was written against a control plane that did not exist
//! yet, with a `Transport::Mock` standing in for it. Every route, field
//! name, and JSON shape here is derived from that file, so that flipping
//! the CLI from `--mock` to `--live` changes nothing the user can see.
//! When the two disagree, the CLI is right and this is the bug.
//!
//! # Key scoping
//!
//! A key with a NULL `project_id` is account-scoped: it can create
//! projects and manage every project in its account. That is what
//! `atlas deploy` needs, because at deploy time the project may not exist
//! yet — a project-scoped key could never bootstrap one. Keys minted
//! through `atlas keys create` are scoped to the project they were
//! created under, so leaking a CI key does not hand over the account.
//!
//! # What this service does not do
//!
//! It does not proxy application traffic — that is the gateway — and it
//! does not aggregate application logs. See `routes::logs` for why
//! `atlas logs` returns the control plane's own audit trail rather than
//! stdout from the other services.

pub mod auth;
pub mod config;
pub mod db;
pub mod error;
pub mod metrics;
pub mod models;
pub mod ratelimit;
pub mod routes;
pub mod state;
pub mod status;
