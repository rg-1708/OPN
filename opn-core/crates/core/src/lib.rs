//! Library surface of the opn-core binary — exists so integration tests can
//! exercise the router, config, and infra helpers directly.

pub mod admin;
pub mod config;
pub mod gateway;
pub mod http;
pub mod infra;
pub mod janitor;
pub mod listener;
pub mod observe;
pub mod primitives;
pub mod state;

/// Embedded forward-only migrations (OPN-CORE.md §9). Shared by `main` and
/// `#[sqlx::test]` fixtures so tests always run the real schema.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
