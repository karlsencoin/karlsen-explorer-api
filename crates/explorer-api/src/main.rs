use karlsen_explorer_api::{
    config::Config,
    live::{LiveConfig, LiveHub},
    node::NodeClient,
    routes, AppState,
};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = Config::from_env()?;
    let statement_timeout_ms = config.db_statement_timeout.as_millis();
    let db = PgPoolOptions::new()
        .max_connections(config.db_max_connections)
        .after_connect(move |conn, _meta| {
            Box::pin(async move {
                sqlx::query(&format!("SET statement_timeout = {statement_timeout_ms}"))
                    .execute(conn)
                    .await?;
                Ok(())
            })
        })
        .connect(&config.database_url)
        .await?;

    let node = match &config.karlsend_grpc_url {
        Some(url) => Some(NodeClient::new(url, config.node_timeout)?),
        None => {
            tracing::warn!("KARLSEND_GRPC_URL not set: serving everything from the database");
            None
        }
    };

    let mut state = AppState::new(db, node.clone());
    // One persistent node subscription shared by every live client.
    state.live = node.map(|n| LiveHub::spawn(LiveConfig::from_env(), n));
    let app = routes::router(state);
    // Keep the explorer's heaviest pages computed ahead of visitors.
    let warm_every = std::env::var("ANALYTICS_WARM_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    karlsen_explorer_api::response_cache::spawn_warmer(
        app.clone(),
        routes::WARM_ANALYTICS_PATHS
            .iter()
            .map(|p| p.to_string())
            .collect(),
        std::time::Duration::from_secs(warm_every),
    );
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!(addr = %config.listen_addr, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
