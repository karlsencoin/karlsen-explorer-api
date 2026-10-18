//! Test harness: creates a throwaway database from the v20 schema + seed data.
//! Requires TEST_DATABASE_URL pointing at an admin-capable Postgres (no database
//! name needed beyond a connectable one). Tests are skipped when it is unset.

use axum::Router;
use karlsen_explorer_api::{routes, AppState};
use sqlx::{postgres::PgPoolOptions, Executor, PgPool};

pub struct TestDb {
    pub pool: PgPool,
    admin: PgPool,
    name: String,
}

pub async fn setup() -> Option<TestDb> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .ok()?;
    let name = format!("explorer_test_{}_{}", std::process::id(), rand_suffix());
    admin
        .execute(format!("CREATE DATABASE {name}").as_str())
        .await
        .ok()?;
    let base = url.rsplit_once('/').map(|x| x.0).unwrap_or(&url);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&format!("{base}/{name}"))
        .await
        .ok()?;
    sqlx::raw_sql(include_str!("fixtures/schema-v20.sql"))
        .execute(&pool)
        .await
        .ok()?;
    sqlx::raw_sql(include_str!("fixtures/seed.sql"))
        .execute(&pool)
        .await
        .expect("seed");
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
        routes::router(AppState {
            db: self.pool.clone(),
        })
    }
    pub async fn teardown(self) {
        self.pool.close().await;
        let _ = self
            .admin
            .execute(format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name).as_str())
            .await;
    }
}

pub async fn get(app: &Router, uri: &str) -> (u16, serde_json::Value) {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}
