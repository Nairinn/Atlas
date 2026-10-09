//! Recorder install and scrape endpoint now live in `atlas_edge::metrics`;
//! this module remains only so `crate::metrics` call sites in main.rs read
//! naturally and the service keeps an obvious place for control-plane-only
//! metric helpers if any appear.

pub use atlas_edge::metrics::{install_recorder, serve};
