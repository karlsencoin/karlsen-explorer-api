use karlsen_explorer_api::{config::Config, node::NodeClient, routes, AppState};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env()?;
    let db = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .connect(&config.database_url)
        .await?;

    let node = match &config.karlsend_grpc_url {
        Some(url) => Some(NodeClient::new(url, config.node_timeout)?),
        None => {
            tracing::warn!("KARLSEND_GRPC_URL not set: serving everything from the database");
            None
        }
    };

    let app = routes::router(AppState::new(db, node));
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!(addr = %config.listen_addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
