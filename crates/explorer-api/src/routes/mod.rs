use axum::{
    routing::{get, post},
    Router,
};
use tower_http::{compression::CompressionLayer, cors::CorsLayer, trace::TraceLayer};

use crate::AppState;

pub mod address_transactions;
pub mod addresses;
pub mod blocks;
pub mod health;
pub mod info;
pub mod transactions;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/info/health", get(health::health))
        .route("/info/blockdag", get(info::blockdag))
        .route("/info/network", get(info::blockdag))
        .route("/info/hashrate", get(info::hashrate))
        .route("/info/coinsupply", get(info::coinsupply))
        .route("/info/coinsupply/circulating", get(info::circulating))
        .route("/info/coinsupply/total", get(info::circulating))
        .route("/info/blockreward", get(info::blockreward))
        .route("/info/halving", get(info::halving))
        .route("/info/karlsend", get(info::karlsend))
        .route("/blocks/{block_id}", get(blocks::get_block))
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
