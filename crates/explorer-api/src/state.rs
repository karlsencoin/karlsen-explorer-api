use sqlx::PgPool;

use crate::node::NodeClient;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    /// karlsend gRPC client. `None` serves everything from the database.
    pub node: Option<NodeClient>,
}
