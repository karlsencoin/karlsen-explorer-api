//! Karlsen explorer API: a wire-compatible, read-only replacement for the
//! legacy Python `rest-v2` service.

pub mod address;
pub mod config;
pub mod difficulty;
pub mod emission;
pub mod error;
pub mod miner;
pub mod node;
pub mod pyfloat;
pub mod routes;
pub mod state;
pub mod transactions;

pub use state::AppState;
