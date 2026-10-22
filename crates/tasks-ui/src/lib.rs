//! The `tasks` desktop dashboard.
//!
//! Everything except [`app`] and [`raster`] is plain Rust with no GUI dependency: the daemon
//! link ([`link`]), the latest-only hand-over between threads ([`mailbox`]), the background
//! feeds ([`feed`]) and the pure view model ([`model`], [`format`], [`columns`], [`logview`],
//! [`stats`], [`actions`]). The egui window itself sits behind the `ui` feature, so a default
//! workspace build never compiles egui (R-199).
//!
//! The Audit tab is built ([`audit`]); the planned "Pressure events" tab (see
//! `docs/DESIGN-peers-sync-audit.md`) has a place in [`model::Tab`] but no content yet.

pub mod actions;
pub mod audit;
pub mod columns;
pub mod feed;
pub mod fixtures;
pub mod format;
pub mod link;
pub mod logview;
pub mod mailbox;
pub mod model;
pub mod stats;

#[cfg(feature = "ui")]
pub mod app;
#[cfg(feature = "ui")]
pub mod raster;
