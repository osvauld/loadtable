//! The `tasks` desktop dashboard.
//!
//! Everything except [`app`] and [`raster`] is plain Rust with no GUI dependency: the daemon
//! link ([`link`]), the latest-only hand-over between threads ([`mailbox`]), the background
//! feeds ([`feed`]) and the pure view model ([`model`], [`format`], [`columns`], [`logview`],
//! [`stats`], [`actions`], [`detail`]). The egui window itself sits behind the `ui` feature, so a default
//! workspace build never compiles egui (R-199).
//!
//! The Audit tab ([`audit`]) and the Stalls tab ([`stalls`]) read the audit log; the
//! latter lists `pressure.trip` rows (design §9: what caused each stall).

pub mod actions;
pub mod audit;
pub mod columns;
pub mod detail;
pub mod feed;
pub mod fixtures;
pub mod format;
pub mod link;
pub mod logview;
pub mod machine;
pub mod mailbox;
pub mod model;
pub mod resources;
pub mod stalls;
pub mod stats;

#[cfg(feature = "ui")]
pub mod app;
#[cfg(feature = "ui")]
pub mod paint;
#[cfg(feature = "ui")]
pub mod raster;
#[cfg(feature = "ui")]
pub mod theme;
