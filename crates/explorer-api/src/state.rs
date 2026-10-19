use sqlx::PgPool;

use crate::{market::MarketFeed, node::NodeClient};

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// karlsend gRPC client. `None` serves everything from the database.
    pub node: Option<NodeClient>,
    /// NonKYC ticker for `/info/price` and `/info/market-data`.
    pub market: MarketFeed,
}

impl AppState {
    /// Ticker source from `NONKYC_TICKER_URL` (or the public NonKYC endpoint).
    pub fn new(db: PgPool, node: Option<NodeClient>) -> Self {
        Self {
            db,
            node,
            market: MarketFeed::from_env(),
        }
    }
}
