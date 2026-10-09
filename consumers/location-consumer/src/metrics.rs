//! Prometheus recorder + scrape endpoint live in `atlas_edge::metrics`;
//! re-exported so existing call sites keep reading naturally.

pub use atlas_edge::metrics::{install_recorder, serve};
