use axum::{
    routing::{get, post},
    Router,
};
use tower_http::{compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer};

use crate::AppState;

pub mod address_transactions;
pub mod addresses;
pub mod analytics;
pub mod blocks;
pub mod dag_window;
pub mod health;
pub mod info;
pub mod live;
pub mod miners;
pub mod peers;
pub mod transactions;
pub mod wallet_chain;

/// Whole-table aggregates over `balances`: slow to compute, slow to change, so
/// they are served from `response_cache` (per-address history is not).
fn cached_analytics(
    cache: std::sync::Arc<crate::response_cache::ResponseCache>,
) -> Router<AppState> {
    Router::new()
        .route("/analytics/addresses/top", get(analytics::top))
        .route("/analytics/addresses/total", get(analytics::total))
        .route(
            "/analytics/addresses/distribution",
            get(analytics::distribution),
        )
        .route("/analytics/addresses/range", get(analytics::range))
        .route(
            "/analytics/addresses/distribution-summary",
            get(analytics::distribution_summary),
        )
        .route(
            "/analytics/address-tags/auto",
            get(analytics::auto_address_tags),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            cache,
            crate::response_cache::cached,
        ))
}

/// Paths the explorer opens most; `main` keeps them warm in `response_cache`.
pub const WARM_ANALYTICS_PATHS: &[&str] = &[
    "/analytics/addresses/distribution-summary",
    "/analytics/addresses/top?limit=100&offset=0",
    "/analytics/addresses/range?min_amount=10000000&max_amount=-1",
    "/analytics/addresses/range?min_amount=1000000&max_amount=10000000",
    "/analytics/addresses/range?min_amount=500000&max_amount=1000000",
    "/analytics/addresses/range?min_amount=100000&max_amount=500000",
    "/analytics/addresses/range?min_amount=10000&max_amount=100000",
    "/analytics/addresses/range?min_amount=1000&max_amount=10000",
    "/analytics/addresses/range?min_amount=100&max_amount=1000",
    "/analytics/addresses/range?min_amount=1&max_amount=100",
];

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/info/health", get(health::health))
        .route("/live/stream", get(live::stream))
        .route("/live/recent", get(live::recent))
        .route("/info/blockdag", get(info::blockdag))
        .route("/info/network", get(info::blockdag))
        .route("/info/hashrate", get(info::hashrate))
        .route("/info/coinsupply", get(info::coinsupply))
        .route("/info/coinsupply/circulating", get(info::circulating))
        .route("/info/coinsupply/total", get(info::circulating))
        .route("/info/blockreward", get(info::blockreward))
        .route("/info/halving", get(info::halving))
        .route("/info/karlsend", get(info::karlsend))
        .route("/info/price", get(info::price))
        .route("/info/market-data", get(info::market_data))
        .route("/info/mempool", get(info::mempool))
        .route("/info/dag-window", get(dag_window::dag_window))
        .route("/info/miners/top", get(miners::miners_top))
        .route("/info/miners/network", get(miners::miners_network))
        .route("/info/miners/address/{address}", get(miners::miner_detail))
        .route("/info/nodes", get(peers::nodes))
        .route("/info/peers/geo", get(peers::peers_geo))
        .route("/info/seeder-nodes", get(peers::seeder_nodes))
        .route("/admin/seed-candidates", get(peers::seed_candidates))
        .route("/blocks", get(blocks::list_blocks))
        .route("/blocks/{block_id}", get(blocks::get_block))
        .merge(cached_analytics(state.analytics_cache.clone()))
        .route(
            "/analytics/addresses/{address}/history",
            get(analytics::history),
        )
        .route("/addresses/{address}/balance", get(addresses::get_balance))
        .route("/addresses/{address}/utxos", get(addresses::get_utxos))
        .route("/addresses/balances", post(addresses::post_balances))
        .route("/addresses/utxos", post(addresses::post_utxos))
        .route(
            "/addresses/{address}/full-transactions",
            get(address_transactions::full_transactions),
        )
        .route(
            "/addresses/{address}/transactions-count",
            get(address_transactions::transactions_count),
        )
        .route(
            "/addresses/{address}/wallet-chain",
            get(wallet_chain::wallet_chain),
        )
        .route(
            "/addresses/{address}/wallet-history",
            get(wallet_chain::wallet_history),
        )
        .route("/transactions/search", post(transactions::search))
        .route(
            "/transactions/{transaction_id}",
            get(transactions::get_transaction),
        )
        .layer(CompressionLayer::new())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
