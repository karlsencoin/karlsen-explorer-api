use std::sync::Arc;

use sqlx::PgPool;

use crate::{
    count_cache::CountCache, live::LiveHub, market::MarketFeed, node::NodeClient,
    response_cache::ResponseCache, utxo_cache::UtxoCache,
};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// karlsend gRPC client. `None` serves everything from the database.
    pub node: Option<NodeClient>,
    /// NonKYC ticker for `/info/price` and `/info/market-data`.
    pub market: MarketFeed,
    /// Per-address transaction counts (see `count_cache`).
    pub tx_counts: Arc<CountCache>,
    /// Live block feed (`/live/*`); `None` without a node or before it is started.
    pub live: Option<Arc<LiveHub>>,
    /// Sorted UTXO sets for the paged `/addresses/{address}/utxos`.
    pub utxo_cache: Arc<UtxoCache>,
    /// Cached `/analytics/addresses/*` aggregate responses (see `response_cache`).
    pub analytics_cache: Arc<ResponseCache>,
}

impl AppState {
    /// Ticker source from `NONKYC_TICKER_URL` (or the public NonKYC endpoint).
    pub fn new(db: PgPool, node: Option<NodeClient>) -> Self {
        Self {
            db,
            node,
            market: MarketFeed::from_env(),
            tx_counts: Arc::new(CountCache::default()),
            live: None,
            utxo_cache: Arc::new(UtxoCache::default()),
            // Kept long enough that a slow warm-up round never lets hot pages expire.
            analytics_cache: ResponseCache::new(std::time::Duration::from_secs(600), 256),
        }
    }
}
