//! Configuration is read from environment variables only. No defaults carry
//! credentials, and nothing here is ever logged.

use std::{net::SocketAddr, time::Duration};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen_addr: SocketAddr,
    pub db_max_connections: u32,
    /// Per-statement limit set on every pooled connection. Keeps one slow query
    /// from piling up behind nginx's 30 s proxy timeout.
    pub db_statement_timeout: Duration,
    /// karlsend gRPC endpoint, e.g. `http://127.0.0.1:42110`. Optional: without it
    /// every response comes from the database.
    pub karlsend_grpc_url: Option<String>,
    pub node_timeout: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required environment variable {0}")]
    Missing(&'static str),
    #[error("invalid value for {0}")]
    Invalid(&'static str),
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let database_url =
            std::env::var("DATABASE_URL").map_err(|_| ConfigError::Missing("DATABASE_URL"))?;
        let listen_addr = std::env::var("LISTEN_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:8013".to_string())
            .parse()
            .map_err(|_| ConfigError::Invalid("LISTEN_ADDR"))?;
        let db_max_connections = std::env::var("DB_MAX_CONNECTIONS")
            .unwrap_or_else(|_| "16".to_string())
            .parse()
            .map_err(|_| ConfigError::Invalid("DB_MAX_CONNECTIONS"))?;
        let db_statement_timeout = std::env::var("DB_STATEMENT_TIMEOUT_MS")
            .unwrap_or_else(|_| "25000".to_string())
            .parse()
            .map(Duration::from_millis)
            .map_err(|_| ConfigError::Invalid("DB_STATEMENT_TIMEOUT_MS"))?;
        let karlsend_grpc_url = std::env::var("KARLSEND_GRPC_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let node_timeout = std::env::var("NODE_TIMEOUT_MS")
            .unwrap_or_else(|_| "10000".to_string())
            .parse()
            .map(Duration::from_millis)
            .map_err(|_| ConfigError::Invalid("NODE_TIMEOUT_MS"))?;
        Ok(Self {
            database_url,
            listen_addr,
            db_max_connections,
            db_statement_timeout,
            karlsend_grpc_url,
            node_timeout,
        })
    }
}
