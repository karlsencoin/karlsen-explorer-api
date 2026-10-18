//! Karlsen explorer API: a wire-compatible, read-only replacement for the
//! legacy Python `rest-v2` service.

pub mod address;
pub mod config;
pub mod difficulty;
pub mod error;
pub mod routes;
pub mod state;

pub use state::AppState;
