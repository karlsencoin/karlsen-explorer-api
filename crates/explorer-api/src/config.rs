//! Configuration is read from environment variables only. No defaults carry
//! credentials, and nothing here is ever logged.

use std::net::SocketAddr;

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub listen_addr: SocketAddr,
    pub db_max_connections: u32,
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
        Ok(Self {
            database_url,
            listen_addr,
            db_max_connections,
        })
    }
}
