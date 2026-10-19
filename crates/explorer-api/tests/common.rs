//! Test harness: creates a throwaway database from the v20 schema + seed data.
//! Requires TEST_DATABASE_URL pointing at an admin-capable Postgres (no database
//! name needed beyond a connectable one). Tests are skipped when it is unset.

use axum::Router;
use karlsen_explorer_api::{node::NodeClient, routes, AppState};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};

pub struct TestDb {
    pub pool: PgPool,
    admin: PgPool,
    name: String,
}

/// Returns `None` only when `TEST_DATABASE_URL` is unset. Once a database is
/// configured, any setup failure panics: a silent skip would make every database
/// test pass without running.
pub async fn setup() -> Option<TestDb> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    // Make server-side error logs (e.g. the cause behind a 503) visible in test output.
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");
    let name = format!("explorer_test_{}_{}", std::process::id(), rand_suffix());
    admin
        .execute(format!("CREATE DATABASE {name}").as_str())
        .await
        .expect("create test database");
    let base = url.rsplit_once('/').map(|x| x.0).unwrap_or(&url);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!("{base}/{name}"))
        .await
        .expect("connect to test database");
    // pg_dump 18 emits psql-only `\restrict` / `\unrestrict` lines, which the server
    // rejects, so every psql meta-command line is dropped. The dump also empties the
    // session `search_path`; on a pooled connection that would leave later queries
    // (and the unqualified seed statements) unable to find any table.
    let schema: String = include_str!("fixtures/schema-v20.sql")
        .lines()
        .filter(|l| !l.starts_with('\\') && !l.contains("set_config('search_path'"))
        .collect::<Vec<_>>()
        .join("\n");
    sqlx::raw_sql(&schema)
        .execute(&pool)
        .await
        .expect("load schema-v20.sql");
    sqlx::raw_sql(include_str!("fixtures/seed.sql"))
        .execute(&pool)
        .await
        .expect("seed");
    sqlx::raw_sql(include_str!("fixtures/seed-transactions.sql"))
        .execute(&pool)
        .await
        .expect("seed-transactions");
    Some(TestDb { pool, admin, name })
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

impl TestDb {
    pub fn app(&self) -> Router {
        routes::router(AppState::new(self.pool.clone(), None))
    }
    pub fn app_with_node(&self, node: NodeClient) -> Router {
        routes::router(AppState::new(self.pool.clone(), Some(node)))
    }
    pub async fn teardown(self) {
        self.pool.close().await;
        let _ = self
            .admin
            .execute(format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name).as_str())
            .await;
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null)
    }
    pub fn data_source(&self) -> Option<&str> {
        self.headers
            .get("x-data-source")
            .and_then(|v| v.to_str().ok())
    }
}

pub async fn request(app: &Router, uri: &str) -> Reply {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

pub async fn get(app: &Router, uri: &str) -> (u16, serde_json::Value) {
    let r = request(app, uri).await;
    (r.status, r.json())
}

/// A pool that never connects: proves a code path does not touch the database.
pub fn dead_pool() -> PgPool {
    PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(300))
        .connect_lazy("postgres://u:p@127.0.0.1:1/none")
        .unwrap()
}
